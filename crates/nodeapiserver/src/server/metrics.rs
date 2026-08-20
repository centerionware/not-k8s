//! `/metrics` — a scoped port of real upstream's own
//! `apiserver_request_total` counter (`k8s.io/apiserver/pkg/endpoints/metrics`,
//! the one metric every real Prometheus-scraped kube-apiserver dashboard
//! keys off of), rendered in the same hand-rolled Prometheus text
//! exposition format `crates/nodelet/src/server/prom_metrics.rs` already
//! established for this workspace (no metrics crate dependency, same
//! `push_metric`/`push_help_type` shape).
//!
//! **Labels, scoped down and named honestly**: real upstream's own
//! `apiserver_request_total` carries `verb`, `dry_run`, `group`,
//! `version`, `resource`, `subresource`, `scope`, `component`, `code` —
//! nine labels. This port tracks `verb`, `resource`, `code` only, the
//! three that answer the practically useful questions ("what's erroring",
//! "what's being hit hardest") without the cardinality cost of the full
//! set on a crate this early in its metrics story. Not ported at all:
//! `apiserver_request_duration_seconds` (a histogram — real upstream's
//! own latency SLO metric) and everything else in that package
//! (`apiserver_current_inflight_requests`, `apiserver_watch_events_total`,
//! ...) — genuinely separate, larger pieces of work, not a quick
//! follow-up to this one counter.
//!
//! One process-wide counter table (`std::sync::Mutex<HashMap<...>>`,
//! the same "good enough, no lock contention that matters at this scale"
//! choice a `Mutex` around a small `HashMap` already is elsewhere in this
//! workspace) rather than a real lock-free metrics registry — this
//! crate's own request rate doesn't remotely approach the point where
//! that would matter.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock};

/// `(verb, resource, code)` — deliberately not `String` per axis to keep
/// the map's own comparisons cheap; interned nowhere, just cloned into
/// owned `String`s on insert (request volume here never remotely
/// approaches where that would matter).
type CounterKey = (String, String, u16);

fn counters() -> &'static Mutex<HashMap<CounterKey, u64>> {
    static COUNTERS: OnceLock<Mutex<HashMap<CounterKey, u64>>> = OnceLock::new();
    COUNTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Records one completed request. `resource` is `""` for a non-resource
/// request (a discovery route, `/healthz`, ...) — matches real upstream's
/// own empty-string convention for that case rather than inventing a
/// placeholder label value.
pub fn record_request(verb: &str, resource: &str, code: u16) {
    let key = (verb.to_string(), resource.to_string(), code);
    let mut counters = counters().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    *counters.entry(key).or_insert(0) += 1;
}

fn escape_label_value(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Renders every recorded count as real Prometheus text exposition
/// format — pure given a snapshot, so [`render`] (the one real I/O/lock
/// step) is a thin wrapper around this for unit testing.
fn render_counts(counts: &[(CounterKey, u64)]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# HELP apiserver_request_total Counter of apiserver requests broken out by verb, resource, and HTTP response code.");
    let _ = writeln!(out, "# TYPE apiserver_request_total counter");
    let mut sorted: Vec<&(CounterKey, u64)> = counts.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for ((verb, resource, code), count) in sorted {
        let _ = writeln!(
            out,
            "apiserver_request_total{{verb=\"{}\",resource=\"{}\",code=\"{code}\"}} {count}",
            escape_label_value(verb),
            escape_label_value(resource),
        );
    }
    out
}

/// The real, I/O-touching (well — lock-touching) half: snapshots the
/// process-wide counter table and renders it.
pub fn render() -> String {
    let counters = counters().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let snapshot: Vec<(CounterKey, u64)> = counters.iter().map(|(k, &v)| (k.clone(), v)).collect();
    render_counts(&snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_counts_produces_real_prometheus_text_exposition_format() {
        let counts = vec![(("get".to_string(), "pods".to_string(), 200u16), 3u64)];
        let text = render_counts(&counts);
        assert!(text.contains("# HELP apiserver_request_total"));
        assert!(text.contains("# TYPE apiserver_request_total counter"));
        assert!(text.contains("apiserver_request_total{verb=\"get\",resource=\"pods\",code=\"200\"} 3"));
    }

    #[test]
    fn render_counts_is_sorted_for_stable_output() {
        let counts = vec![(("list".to_string(), "services".to_string(), 200u16), 1u64), (("get".to_string(), "pods".to_string(), 200u16), 1u64)];
        let text = render_counts(&counts);
        let get_pos = text.find("verb=\"get\"").unwrap();
        let list_pos = text.find("verb=\"list\"").unwrap();
        assert!(get_pos < list_pos, "output should be sorted so repeated scrapes diff cleanly");
    }

    #[test]
    fn render_counts_escapes_label_values() {
        let counts = vec![(("get".to_string(), "weird\"resource".to_string(), 200u16), 1u64)];
        let text = render_counts(&counts);
        assert!(text.contains("resource=\"weird\\\"resource\""));
    }

    #[test]
    fn record_request_and_render_round_trip() {
        // Uses the real global table -- a distinct (verb, resource, code)
        // key keeps this test from colliding with any other test's counts.
        record_request("delete", "a-key-unique-to-this-test", 204);
        let text = render();
        assert!(text.contains("apiserver_request_total{verb=\"delete\",resource=\"a-key-unique-to-this-test\",code=\"204\"} "));
    }
}
