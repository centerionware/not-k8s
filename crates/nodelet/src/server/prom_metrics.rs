//! `/metrics/resource` and `/metrics/cadvisor` — the two Prometheus-text
//! alternatives to `/stats/summary` real kubelet also exposes. Some
//! scrapers (Prometheus itself, some autoscalers, older dashboards) talk to
//! these directly instead of going through metrics-server's `metrics.k8s.io`
//! aggregated API, so implementing only `/stats/summary` (round 7) leaves a
//! real gap for anything that expects a kubelet to speak Prometheus text.
//!
//! Built from the exact same `PodUsage`/`UsageStats` CRI already gives
//! `/stats/summary` — no separate collection path.
//!
//! - **`/metrics/resource`** implements the small, well-specified set from
//!   [KEP-2371](https://github.com/kubernetes/enhancements/tree/master/keps/sig-node/2371-cri-pod-container-stats):
//!   `node_cpu_usage_seconds_total`, `node_memory_working_set_bytes`,
//!   `container_cpu_usage_seconds_total`, `container_memory_working_set_bytes`,
//!   `pod_cpu_usage_seconds_total`, `pod_memory_working_set_bytes`. This one
//!   is a complete, accurate implementation of that spec.
//! - **`/metrics/cadvisor`** is real cAdvisor's much larger, less strictly
//!   specified legacy metric catalog — implementing all of it isn't worth
//!   the weight for an edge agent that's otherwise deliberately lean.
//!   This implements the five metrics most dashboards/scrapers built
//!   against cAdvisor actually read: the four usage gauges/counter
//!   (`container_cpu_usage_seconds_total`, `container_memory_usage_bytes`,
//!   `container_memory_working_set_bytes`, `container_memory_rss`) plus
//!   `container_last_seen` (round 100 — trivial and genuinely useful: a
//!   scraper uses it to detect a container that's vanished since the last
//!   scrape, and every container in a fresh `ListPodSandboxStats` result
//!   is, by definition, being seen *right now*, so no new data collection
//!   is needed to report it honestly), plus `container_network_receive_bytes_total`/
//!   `container_network_transmit_bytes_total` (round 102 — corrects round
//!   100's own "needs new CRI data collection" claim about network I/O:
//!   `PodSandboxStats.linux.network.default_interface` was already present
//!   on the same `ListPodSandboxStats` response every other metric here
//!   reads, just unparsed until now). The first five carry a
//!   `{namespace, pod, container}` label set; the two network ones carry
//!   `{namespace, pod, interface}` instead — no `container` label, since a
//!   pod's containers share one network namespace and CRI only ever
//!   reports one measurement per pod, matching real cAdvisor's own
//!   `container_network_*` metrics (pod-scoped despite the `container_`
//!   name prefix). Real cAdvisor also labels every metric here with
//!   `id`/`name`/`image` (the container's cgroup path, runtime name, and
//!   image ref), which aren't tracked anywhere in nodelet's `PodUsage`
//!   today and are dropped here rather than faked. **Still out of scope,
//!   deliberately** (round 102 re-confirmed, not just carried over): disk
//!   I/O and per-cpu-core breakdowns need CRI data this codebase doesn't
//!   collect at all today (CRI's own `IoUsage` is PSI pressure-stall
//!   stats, not byte counters — genuinely nothing to parse, unlike network),
//!   and spec/limit metrics (`container_spec_memory_limit_bytes` etc.)
//!   would need cross-referencing every container against its Pod's
//!   resource spec on every scrape — real functionality, but a bigger,
//!   separate piece of work than this round's scope.

use super::{text_response, BoxedBody, ServerState};
use crate::runtime::PodUsage;
use hyper::{Response, StatusCode};
use std::fmt::Write;

fn escape_label_value(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn push_metric(out: &mut String, name: &str, labels: &[(&str, &str)], value: f64) {
    out.push_str(name);
    if !labels.is_empty() {
        out.push('{');
        for (i, (k, v)) in labels.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{k}=\"{}\"", escape_label_value(v));
        }
        out.push('}');
    }
    let _ = writeln!(out, " {value}");
}

fn push_help_type(out: &mut String, name: &str, help: &str, metric_type: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {metric_type}");
}

/// `usage_core_nano_seconds` (cumulative CPU nanoseconds) -> core-seconds,
/// the unit every one of these metrics reports CPU in.
fn cpu_seconds(nanos: Option<u64>) -> Option<f64> {
    nanos.map(|n| n as f64 / 1_000_000_000.0)
}

pub fn render_resource_metrics(
    node_name: &str,
    node_cpu_seconds: Option<f64>,
    node_memory_working_set_bytes: Option<u64>,
    pods: &[PodUsage],
) -> String {
    let mut out = String::new();

    push_help_type(&mut out, "node_cpu_usage_seconds_total", "Cumulative cpu time consumed by the node in core-seconds", "counter");
    if let Some(v) = node_cpu_seconds {
        push_metric(&mut out, "node_cpu_usage_seconds_total", &[("node", node_name)], v);
    }
    push_help_type(&mut out, "node_memory_working_set_bytes", "Current working set of the node in bytes", "gauge");
    if let Some(v) = node_memory_working_set_bytes {
        push_metric(&mut out, "node_memory_working_set_bytes", &[("node", node_name)], v as f64);
    }

    push_help_type(&mut out, "pod_cpu_usage_seconds_total", "Cumulative cpu time consumed by the pod in core-seconds", "counter");
    for pod in pods {
        if let Some(v) = cpu_seconds(pod.pod.cpu_usage_core_nano_seconds) {
            push_metric(&mut out, "pod_cpu_usage_seconds_total", &[("namespace", &pod.namespace), ("pod", &pod.name)], v);
        }
    }
    push_help_type(&mut out, "pod_memory_working_set_bytes", "Current working set of the pod in bytes", "gauge");
    for pod in pods {
        if let Some(v) = pod.pod.memory_working_set_bytes {
            push_metric(&mut out, "pod_memory_working_set_bytes", &[("namespace", &pod.namespace), ("pod", &pod.name)], v as f64);
        }
    }

    push_help_type(&mut out, "container_cpu_usage_seconds_total", "Cumulative cpu time consumed by the container in core-seconds", "counter");
    for pod in pods {
        for c in &pod.containers {
            if let Some(v) = cpu_seconds(c.stats.cpu_usage_core_nano_seconds) {
                push_metric(
                    &mut out,
                    "container_cpu_usage_seconds_total",
                    &[("namespace", &pod.namespace), ("pod", &pod.name), ("container", &c.name)],
                    v,
                );
            }
        }
    }
    push_help_type(&mut out, "container_memory_working_set_bytes", "Current working set of the container in bytes", "gauge");
    for pod in pods {
        for c in &pod.containers {
            if let Some(v) = c.stats.memory_working_set_bytes {
                push_metric(
                    &mut out,
                    "container_memory_working_set_bytes",
                    &[("namespace", &pod.namespace), ("pod", &pod.name), ("container", &c.name)],
                    v as f64,
                );
            }
        }
    }

    out
}

/// `now_unix_seconds` is a parameter (not read internally via
/// `SystemTime::now()`) so this stays a pure function unit-testable
/// without mocking the clock — the caller (`handle_metrics_cadvisor`)
/// supplies the real value.
pub fn render_cadvisor_metrics(pods: &[PodUsage], now_unix_seconds: u64) -> String {
    let mut out = String::new();

    push_help_type(&mut out, "container_cpu_usage_seconds_total", "Cumulative cpu time consumed by the container in core-seconds", "counter");
    for pod in pods {
        for c in &pod.containers {
            if let Some(v) = cpu_seconds(c.stats.cpu_usage_core_nano_seconds) {
                push_metric(
                    &mut out,
                    "container_cpu_usage_seconds_total",
                    &[("namespace", &pod.namespace), ("pod", &pod.name), ("container", &c.name)],
                    v,
                );
            }
        }
    }

    push_help_type(&mut out, "container_memory_usage_bytes", "Current memory usage of the container in bytes, including all memory regardless of when it was accessed", "gauge");
    for pod in pods {
        for c in &pod.containers {
            if let Some(v) = c.stats.memory_usage_bytes {
                push_metric(
                    &mut out,
                    "container_memory_usage_bytes",
                    &[("namespace", &pod.namespace), ("pod", &pod.name), ("container", &c.name)],
                    v as f64,
                );
            }
        }
    }

    push_help_type(&mut out, "container_memory_working_set_bytes", "Current working set of the container in bytes", "gauge");
    for pod in pods {
        for c in &pod.containers {
            if let Some(v) = c.stats.memory_working_set_bytes {
                push_metric(
                    &mut out,
                    "container_memory_working_set_bytes",
                    &[("namespace", &pod.namespace), ("pod", &pod.name), ("container", &c.name)],
                    v as f64,
                );
            }
        }
    }

    push_help_type(&mut out, "container_memory_rss", "Size of RSS in bytes", "gauge");
    for pod in pods {
        for c in &pod.containers {
            if let Some(v) = c.stats.memory_rss_bytes {
                push_metric(
                    &mut out,
                    "container_memory_rss",
                    &[("namespace", &pod.namespace), ("pod", &pod.name), ("container", &c.name)],
                    v as f64,
                );
            }
        }
    }

    // container_last_seen (round 100): every container in this snapshot is,
    // by definition, being observed right now — a scraper uses this to
    // detect a container that's vanished since the last scrape (its
    // last_seen value stops advancing).
    push_help_type(&mut out, "container_last_seen", "Last time a container was seen by the exporter", "gauge");
    for pod in pods {
        for c in &pod.containers {
            push_metric(
                &mut out,
                "container_last_seen",
                &[("namespace", &pod.namespace), ("pod", &pod.name), ("container", &c.name)],
                now_unix_seconds as f64,
            );
        }
    }

    // container_network_{receive,transmit}_bytes_total (round 102): pod-
    // scoped, not per-container -- see the module doc comment for why.
    // "interface" defaults to "" (rather than being omitted) when CRI
    // reported rx/tx bytes but no interface name, keeping the label set
    // shape consistent across samples.
    push_help_type(
        &mut out,
        "container_network_receive_bytes_total",
        "Cumulative count of bytes received",
        "counter",
    );
    for pod in pods {
        if let Some(v) = pod.network_rx_bytes {
            push_metric(
                &mut out,
                "container_network_receive_bytes_total",
                &[("namespace", &pod.namespace), ("pod", &pod.name), ("interface", pod.network_interface.as_deref().unwrap_or(""))],
                v as f64,
            );
        }
    }

    push_help_type(
        &mut out,
        "container_network_transmit_bytes_total",
        "Cumulative count of bytes transmitted",
        "counter",
    );
    for pod in pods {
        if let Some(v) = pod.network_tx_bytes {
            push_metric(
                &mut out,
                "container_network_transmit_bytes_total",
                &[("namespace", &pod.namespace), ("pod", &pod.name), ("interface", pod.network_interface.as_deref().unwrap_or(""))],
                v as f64,
            );
        }
    }

    out
}

fn prom_response(body: String) -> Response<BoxedBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
        .body(super::body_from_bytes(body.into_bytes()))
        .unwrap()
}

pub async fn handle_metrics_resource(state: &ServerState) -> Response<BoxedBody> {
    let usages = match state.runtime.pod_usage_stats().await {
        Ok(u) => u,
        Err(e) => return text_response(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let node_cpu = crate::metrics::read_node_cpu_seconds();
    let node_memory = crate::metrics::read_mem_info().map(|m| m.total_bytes.saturating_sub(m.available_bytes));
    prom_response(render_resource_metrics(&state.node_name, node_cpu, node_memory, &usages))
}

pub async fn handle_metrics_cadvisor(state: &ServerState) -> Response<BoxedBody> {
    let usages = match state.runtime.pod_usage_stats().await {
        Ok(u) => u,
        Err(e) => return text_response(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let now_unix_seconds =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    prom_response(render_cadvisor_metrics(&usages, now_unix_seconds))
}

#[cfg(test)]
#[path = "prom_metrics_tests/render_resource_metrics.rs"]
mod tests_render_resource_metrics;
#[cfg(test)]
#[path = "prom_metrics_tests/render_cadvisor_metrics.rs"]
mod tests_render_cadvisor_metrics;
