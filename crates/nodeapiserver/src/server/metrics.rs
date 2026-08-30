//! `/metrics` — a scoped port of real upstream's own
//! `apiserver_request_total` counter (`k8s.io/apiserver/pkg/endpoints/metrics`,
//! the one metric every real Prometheus-scraped kube-apiserver dashboard
//! keys off of), rendered in the same hand-rolled Prometheus text
//! exposition format `crates/nodelet/src/server/prom_metrics.rs` already
//! established for this workspace (no metrics crate dependency, same
//! `push_metric`/`push_help_type` shape).
//!
//! The request counter carries real upstream's complete label set:
//! `verb`, `dry_run`, `group`, `version`, `resource`, `subresource`, `scope`,
//! `component`, and `code`. The request-duration histogram carries the same
//! labels except `code`, as upstream does; response sizes carry the same
//! labels except `dry_run`. The values are derived from the already-parsed
//! [`RequestInfo`] rather than reparsing request paths in the metrics layer.
//! `apiserver_request_duration_seconds` is real upstream's latency SLO metric,
//! with its own exact bucket boundaries (its own doc comment says to
//! customize them for SLO verification and regression tracking).
//! `apiserver_watch_events_total` (`group`/`version`/`resource` labels,
//! confirmed directly) is ported too — incremented at the exact point
//! real upstream's own `WatchEvents.WithLabelValues(...).Inc()` is
//! called: once per event actually encoded and written to a watch
//! client's connection (`server::listener::encode_watch_event`), not per
//! event this build merely considered and filtered out by a selector.
//! `apiserver_current_inflight_requests` is also exposed for the two
//! request kinds (`readOnly` and `mutating`). It reports the maximum number
//! of seats observed in the preceding one-second sample window, matching
//! upstream's pre-aggregated gauge, rather than counting long-running
//! requests that deliberately bypass this build's APF gate.
//! `apiserver_response_sizes` (real upstream's own exponential-bucket
//! histogram, `compbasemetrics.ExponentialBuckets(1000, 10.0, 7)` — 1KB
//! to 1GB, confirmed directly) is ported too, from `http_body::Body::
//! size_hint().exact()` on the finished response
//! (`server::listener::handle_with_audit`'s own call site) — **only
//! recorded when the size is known up front**, which is every verb this
//! crate serves except `watch` (an unbounded stream real upstream's own
//! byte-counting `ResponseWriterDelegator` instruments but this build's
//! simpler size-hint approach can't): a real, named, narrower scope than
//! upstream's own, not a silent gap — a `watch` response simply never
//! contributes an observation here rather than this build fabricating a
//! number for an inherently unknowable total. Everything else in that
//! package is still **not ported** — genuinely separate, smaller-value
//! pieces of work.
//!
//! One process-wide counter table (`std::sync::Mutex<HashMap<...>>`,
//! the same "good enough, no lock contention that matters at this scale"
//! choice a `Mutex` around a small `HashMap` already is elsewhere in this
//! workspace) rather than a real lock-free metrics registry — this
//! crate's own request rate doesn't remotely approach the point where
//! that would matter.

use super::path::RequestInfo;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

struct InflightWindow {
    started: Instant,
    current: [usize; 2],
    peak: [usize; 2],
}

impl InflightWindow {
    fn new(started: Instant) -> Self {
        Self { started, current: [0; 2], peak: [0; 2] }
    }

    fn roll_if_needed(&mut self, now: Instant) {
        if now.duration_since(self.started) < Duration::from_secs(1) {
            return;
        }
        self.started = now;
        self.peak = self.current;
    }

    fn begin(&mut self, mutating: bool, now: Instant) {
        self.roll_if_needed(now);
        let index = if mutating { 1 } else { 0 };
        self.current[index] += 1;
        self.peak[index] = self.peak[index].max(self.current[index]);
    }

    fn finish(&mut self, mutating: bool, now: Instant) {
        self.roll_if_needed(now);
        let index = if mutating { 1 } else { 0 };
        self.current[index] = self.current[index].saturating_sub(1);
    }

    fn peaks(&mut self, now: Instant) -> [usize; 2] {
        self.roll_if_needed(now);
        self.peak
    }
}

fn inflight_window() -> &'static Mutex<InflightWindow> {
    static WINDOW: OnceLock<Mutex<InflightWindow>> = OnceLock::new();
    WINDOW.get_or_init(|| Mutex::new(InflightWindow::new(Instant::now())))
}

/// A request's APF seat accounting. Dropping it at the same scope as the
/// limiter permit keeps the gauge aligned with the actual bounded request
/// budget, including early-return responses.
pub struct InFlightGuard {
    mutating: bool,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut window = inflight_window().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        window.finish(self.mutating, Instant::now());
    }
}

/// Begins accounting one request that successfully acquired an APF seat.
/// Exempt and long-running requests do not acquire a seat and must not call
/// this function.
pub fn begin_inflight(mutating: bool) -> InFlightGuard {
    let mut window = inflight_window().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    window.begin(mutating, Instant::now());
    InFlightGuard { mutating }
}

fn render_inflight_counts(readonly: usize, mutating: usize) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# HELP apiserver_current_inflight_requests Maximal number of currently used inflight request limit of this apiserver per request kind in last second."
    );
    let _ = writeln!(out, "# TYPE apiserver_current_inflight_requests gauge");
    let _ = writeln!(
        out,
        "apiserver_current_inflight_requests{{request_kind=\"mutating\"}} {mutating}"
    );
    let _ = writeln!(
        out,
        "apiserver_current_inflight_requests{{request_kind=\"readOnly\"}} {readonly}"
    );
    out
}

fn render_inflight() -> String {
    let mut window = inflight_window().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let [readonly, mutating] = window.peaks(Instant::now());
    render_inflight_counts(readonly, mutating)
}

const COMPONENT: &str = "apiserver";

/// The shared request label set used by the request counter and latency
/// histogram. Kubernetes exposes the empty string for labels that do not
/// apply, such as group/version on a non-resource request.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RequestLabels {
    verb: String,
    dry_run: String,
    group: String,
    version: String,
    resource: String,
    subresource: String,
    scope: String,
    component: String,
}

/// Builds the upstream-shaped metric labels for one parsed request.
pub fn labels_for_request(info: &RequestInfo, query: &str) -> RequestLabels {
    RequestLabels {
        verb: info.verb.to_ascii_uppercase(),
        dry_run: dry_run_label(query),
        group: if info.is_resource_request { info.api_group.clone() } else { String::new() },
        version: if info.is_resource_request { info.api_version.clone() } else { String::new() },
        resource: if info.is_resource_request { info.resource.clone() } else { String::new() },
        subresource: if info.is_resource_request { info.subresource.clone() } else { String::new() },
        scope: request_scope(info),
        component: COMPONENT.to_string(),
    }
}

fn request_scope(info: &RequestInfo) -> String {
    if !info.is_resource_request {
        return String::new();
    }
    if !info.name.is_empty() || info.verb == "create" {
        return "resource".to_string();
    }
    if !info.namespace.is_empty() {
        return "namespace".to_string();
    }
    "cluster".to_string()
}

fn dry_run_label(query: &str) -> String {
    let values: Vec<String> = super::path::parse_query(query)
        .into_iter()
        .filter_map(|(key, value)| (key == "dryRun").then_some(value))
        .collect();
    if values.is_empty() {
        return String::new();
    }
    if values.iter().all(|value| value == "All") {
        "All".to_string()
    } else {
        "invalid".to_string()
    }
}

/// `(request labels, code)` — labels are owned because requests can finish
/// after their parsed request object has gone out of scope.
type CounterKey = (RequestLabels, u16);

fn counters() -> &'static Mutex<HashMap<CounterKey, u64>> {
    static COUNTERS: OnceLock<Mutex<HashMap<CounterKey, u64>>> = OnceLock::new();
    COUNTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The response-size histogram has the upstream label set except `dry_run`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct ResponseLabels {
    verb: String,
    group: String,
    version: String,
    resource: String,
    subresource: String,
    scope: String,
    component: String,
}

impl From<&RequestLabels> for ResponseLabels {
    fn from(labels: &RequestLabels) -> Self {
        Self {
            verb: labels.verb.clone(),
            group: labels.group.clone(),
            version: labels.version.clone(),
            resource: labels.resource.clone(),
            subresource: labels.subresource.clone(),
            scope: labels.scope.clone(),
            component: labels.component.clone(),
        }
    }
}

type HistogramKey = RequestLabels;

/// Real upstream's own exact bucket boundaries for
/// `apiserver_request_duration_seconds` — see this module's own doc
/// comment for why these specific values, confirmed directly against
/// `metrics.go` rather than picked.
const DURATION_BUCKETS: &[f64] = &[0.005, 0.025, 0.05, 0.1, 0.2, 0.4, 0.6, 0.8, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0, 15.0, 20.0, 30.0, 45.0, 60.0];

/// One histogram's own accumulated state — cumulative-by-construction,
/// same convention Prometheus's own `_bucket{le=...}` exposition
/// requires (`bucket_counts[i]` is the count of observations `<=
/// DURATION_BUCKETS[i]`, not the count strictly *between* two
/// boundaries): incrementing every bucket from the matched one onward at
/// observation time keeps `render` a pure read, no cumulative-sum pass
/// needed there.
#[derive(Default)]
struct Histogram {
    bucket_counts: Vec<u64>,
    sum: f64,
    count: u64,
}

fn histograms() -> &'static Mutex<HashMap<HistogramKey, Histogram>> {
    static HISTOGRAMS: OnceLock<Mutex<HashMap<HistogramKey, Histogram>>> = OnceLock::new();
    HISTOGRAMS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Real upstream's own exact bucket boundaries for
/// `apiserver_response_sizes` — `compbasemetrics.ExponentialBuckets(1000,
/// 10.0, 7)`, confirmed directly (its own comment: "1000 bytes (1KB) to
/// 10^9 bytes (1GB)").
const RESPONSE_SIZE_BUCKETS: &[f64] = &[1_000.0, 10_000.0, 100_000.0, 1_000_000.0, 10_000_000.0, 100_000_000.0, 1_000_000_000.0];

fn response_size_histograms() -> &'static Mutex<HashMap<ResponseLabels, Histogram>> {
    static RESPONSE_SIZE_HISTOGRAMS: OnceLock<Mutex<HashMap<ResponseLabels, Histogram>>> = OnceLock::new();
    RESPONSE_SIZE_HISTOGRAMS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Shared by [`record_duration`] and [`record_response_size`] — both are
/// "observe one value into one bucketed histogram," differing only in
/// which map and which bucket boundaries. `seconds`/`size` in the two
/// callers is always non-negative in practice (a real `Instant::elapsed()`
/// duration or a real byte count); a negative `value` here would simply
/// not increment any bucket, no panic — same "malformed input degrades
/// rather than crashes" posture the rest of this crate's own metrics
/// code takes.
fn record_into<K>(map: &Mutex<HashMap<K, Histogram>>, buckets: &[f64], key: K, value: f64)
where
    K: Eq + std::hash::Hash,
{
    let mut map = map.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let hist = map.entry(key).or_insert_with(|| Histogram { bucket_counts: vec![0; buckets.len()], sum: 0.0, count: 0 });
    hist.sum += value;
    hist.count += 1;
    for (i, &bound) in buckets.iter().enumerate() {
        if value <= bound {
            hist.bucket_counts[i] += 1;
        }
    }
}

/// Records one completed request's own response body size, in bytes.
/// Real upstream's own instrumentation point (`ResponseWriterDelegator`)
/// counts every byte actually written, streaming responses included —
/// **this build only records a response whose size is known up front**
/// (`http_body::Body::size_hint().exact()`, `server::listener::
/// handle_with_audit`'s own call site), which is every non-streaming
/// response (every real verb this crate serves except `watch`) but not
/// `watch` itself (an unbounded, unknown-length stream) — a real, named,
/// narrower scope than upstream's own, not a silent gap: a `watch`
/// request's response size simply never contributes an observation here,
/// rather than this build fabricating a number for an inherently
/// unknowable total.
pub fn record_response_size(labels: &RequestLabels, size_bytes: u64) {
    record_into(response_size_histograms(), RESPONSE_SIZE_BUCKETS, ResponseLabels::from(labels), size_bytes as f64);
}

/// Records one completed request's own latency. The labels follow
/// [`labels_for_request`], including its empty-string-for-non-resource
/// convention. `seconds` is expected non-negative (a real `Instant::elapsed()`
/// duration always is) — a negative value would simply not increment
/// any bucket, no panic, same "malformed input degrades rather than
/// crashes" posture the rest of this crate's own metrics code takes.
pub fn record_duration(labels: &RequestLabels, seconds: f64) {
    record_into(histograms(), DURATION_BUCKETS, labels.clone(), seconds);
}

/// Records one completed request with the full upstream label set.
pub fn record_request(labels: &RequestLabels, code: u16) {
    let key = (labels.clone(), code);
    let mut counters = counters().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    *counters.entry(key).or_insert(0) += 1;
}

/// `(group, version, resource)` — `apiserver_watch_events_total`'s own
/// real label set (`metrics.go`'s own `WatchEvents`, confirmed directly)
/// — a genuinely different key shape from both [`CounterKey`] (`verb`
/// instead of `group`/`version`) and [`HistogramKey`], not reused.
type WatchEventKey = (String, String, String);

fn watch_event_counters() -> &'static Mutex<HashMap<WatchEventKey, u64>> {
    static WATCH_EVENT_COUNTERS: OnceLock<Mutex<HashMap<WatchEventKey, u64>>> = OnceLock::new();
    WATCH_EVENT_COUNTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Records one event actually written to a watch client's own
/// connection — real upstream's own increment point too
/// (`WatchEvents.WithLabelValues(...).Inc()`, called once per event
/// sent, not per event this build merely considered and filtered out).
/// `group` is `""` for the core group, matching every other real
/// upstream group-label convention this crate already follows.
pub fn record_watch_event(group: &str, version: &str, resource: &str) {
    let key = (group.to_string(), version.to_string(), resource.to_string());
    let mut counters = watch_event_counters().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    *counters.entry(key).or_insert(0) += 1;
}

fn render_watch_event_counts(counts: &[(WatchEventKey, u64)]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# HELP apiserver_watch_events_total Number of events sent in watch clients.");
    let _ = writeln!(out, "# TYPE apiserver_watch_events_total counter");
    let mut sorted: Vec<&(WatchEventKey, u64)> = counts.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for ((group, version, resource), count) in sorted {
        let _ = writeln!(
            out,
            "apiserver_watch_events_total{{group=\"{}\",version=\"{}\",resource=\"{}\"}} {count}",
            escape_label_value(group),
            escape_label_value(version),
            escape_label_value(resource),
        );
    }
    out
}

fn escape_label_value(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Renders every recorded count as real Prometheus text exposition
/// format — pure given a snapshot, so [`render`] (the one real I/O/lock
/// step) is a thin wrapper around this for unit testing.
fn render_counts(counts: &[(CounterKey, u64)]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# HELP apiserver_request_total Counter of apiserver requests broken out for each verb, dry run value, group, version, resource, scope, component, and HTTP response code.");
    let _ = writeln!(out, "# TYPE apiserver_request_total counter");
    let mut sorted: Vec<&(CounterKey, u64)> = counts.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for ((labels, code), count) in sorted {
        let _ = writeln!(
            out,
            "apiserver_request_total{{{},code=\"{code}\"}} {count}",
            render_request_labels(labels),
        );
    }
    out
}

/// One histogram's own snapshot, cheap to clone out from behind the
/// lock — `(bucket_counts, sum, count)`, the exact three numbers
/// [`render_histograms`] needs and nothing that ties it back to the
/// live, lockable [`Histogram`] type.
type HistogramSnapshot = (Vec<u64>, f64, u64);

/// Renders every recorded histogram as real Prometheus text exposition
/// format — cumulative `_bucket{le=...}` lines (real upstream's own
/// requirement: each bucket's count already includes every smaller
/// bucket's own observations, which [`record_duration`]'s own
/// increment-from-the-matched-bucket-onward loop already produces, so
/// this function does no summing of its own), a final `+Inf` bucket
/// (always equal to `count`, matching every value's own membership in
/// the unbounded top bucket), then `_sum`/`_count`. Pure given a
/// snapshot, same split [`render_counts`] already established.
/// Shared by every histogram's own render (`apiserver_request_duration_
/// seconds`, `apiserver_response_sizes`) — real Prometheus text
/// exposition format's own cumulative `_bucket{le=...}` lines (each
/// bucket's count already includes every smaller bucket's own
/// observations, which [`record_into`]'s own increment-from-the-matched-
/// bucket-onward loop already produces, so this does no summing of its
/// own), a final `+Inf` bucket (always equal to `count`), then
/// `_sum`/`_count`.
fn render_histogram<K>(name: &str, help: &str, buckets: &[f64], hists: &[(K, HistogramSnapshot)]) -> String
where
    K: HistogramLabels + Ord,
{
    let mut out = String::new();
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} histogram");
    let mut sorted: Vec<&(K, HistogramSnapshot)> = hists.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for ((labels, (bucket_counts, sum, count))) in sorted {
        let rendered_labels = render_histogram_labels(labels);
        for (bound, cumulative) in buckets.iter().zip(bucket_counts.iter()) {
            let _ = writeln!(out, "{name}_bucket{{{rendered_labels},le=\"{bound}\"}} {cumulative}");
        }
        let _ = writeln!(out, "{name}_bucket{{{rendered_labels},le=\"+Inf\"}} {count}");
        let _ = writeln!(out, "{name}_sum{{{rendered_labels}}} {sum}");
        let _ = writeln!(out, "{name}_count{{{rendered_labels}}} {count}");
    }
    out
}

fn render_histograms(hists: &[(HistogramKey, HistogramSnapshot)]) -> String {
    render_histogram("apiserver_request_duration_seconds", "Response latency distribution in seconds for each verb, dry run value, group, version, resource, scope, and component.", DURATION_BUCKETS, hists)
}

fn render_response_sizes(hists: &[(ResponseLabels, HistogramSnapshot)]) -> String {
    render_histogram("apiserver_response_sizes", "Response size distribution in bytes for each group, version, verb, resource, subresource, scope, and component.", RESPONSE_SIZE_BUCKETS, hists)
}

fn render_request_labels(labels: &RequestLabels) -> String {
    format!(
        "verb=\"{}\",dry_run=\"{}\",group=\"{}\",version=\"{}\",resource=\"{}\",subresource=\"{}\",scope=\"{}\",component=\"{}\"",
        escape_label_value(&labels.verb),
        escape_label_value(&labels.dry_run),
        escape_label_value(&labels.group),
        escape_label_value(&labels.version),
        escape_label_value(&labels.resource),
        escape_label_value(&labels.subresource),
        escape_label_value(&labels.scope),
        escape_label_value(&labels.component),
    )
}

trait HistogramLabels {
    fn render(&self) -> String;
}

impl HistogramLabels for RequestLabels {
    fn render(&self) -> String {
        render_request_labels(self)
    }
}

impl HistogramLabels for ResponseLabels {
    fn render(&self) -> String {
        format!(
            "verb=\"{}\",group=\"{}\",version=\"{}\",resource=\"{}\",subresource=\"{}\",scope=\"{}\",component=\"{}\"",
            escape_label_value(&self.verb),
            escape_label_value(&self.group),
            escape_label_value(&self.version),
            escape_label_value(&self.resource),
            escape_label_value(&self.subresource),
            escape_label_value(&self.scope),
            escape_label_value(&self.component),
        )
    }
}

fn render_histogram_labels<K: HistogramLabels>(labels: &K) -> String {
    labels.render()
}

/// The real, I/O-touching (well — lock-touching) half: snapshots the
/// process-wide counter table and renders it.
pub fn render() -> String {
    let counters = counters().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let counter_snapshot: Vec<(CounterKey, u64)> = counters.iter().map(|(k, &v)| (k.clone(), v)).collect();
    drop(counters);

    let histograms = histograms().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let histogram_snapshot: Vec<(HistogramKey, HistogramSnapshot)> = histograms.iter().map(|(k, h)| (k.clone(), (h.bucket_counts.clone(), h.sum, h.count))).collect();
    drop(histograms);

    let response_size_histograms = response_size_histograms().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let response_size_snapshot: Vec<(ResponseLabels, HistogramSnapshot)> = response_size_histograms.iter().map(|(k, h)| (k.clone(), (h.bucket_counts.clone(), h.sum, h.count))).collect();
    drop(response_size_histograms);

    let watch_event_counters = watch_event_counters().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let watch_event_snapshot: Vec<(WatchEventKey, u64)> = watch_event_counters.iter().map(|(k, &v)| (k.clone(), v)).collect();
    drop(watch_event_counters);

    render_counts(&counter_snapshot)
        + &render_histograms(&histogram_snapshot)
        + &render_response_sizes(&response_size_snapshot)
        + &render_watch_event_counts(&watch_event_snapshot)
        + &render_inflight()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(verb: &str, resource: &str) -> RequestLabels {
        RequestLabels {
            verb: verb.to_string(),
            dry_run: String::new(),
            group: String::new(),
            version: "v1".to_string(),
            resource: resource.to_string(),
            subresource: String::new(),
            scope: "namespace".to_string(),
            component: COMPONENT.to_string(),
        }
    }

    #[test]
    fn labels_follow_upstream_scope_and_dry_run_conventions() {
        let namespaced = super::path::parse(
            "GET",
            "/api/v1/namespaces/default/configmaps",
            "dryRun=All",
        );
        let namespaced_labels = labels_for_request(&namespaced, "dryRun=All");
        assert_eq!(namespaced_labels.verb, "LIST");
        assert_eq!(namespaced_labels.dry_run, "All");
        assert_eq!(namespaced_labels.scope, "namespace");
        assert_eq!(namespaced_labels.component, "apiserver");

        let cluster = super::path::parse("GET", "/api/v1/nodes", "");
        assert_eq!(labels_for_request(&cluster, "").scope, "cluster");

        let non_resource = super::path::parse("GET", "/metrics", "");
        let non_resource_labels = labels_for_request(&non_resource, "");
        assert_eq!(non_resource_labels.scope, "");
        assert_eq!(non_resource_labels.group, "");
        assert_eq!(non_resource_labels.version, "");
        assert_eq!(non_resource_labels.resource, "");
    }

    #[test]
    fn invalid_dry_run_values_are_labeled_without_panicking() {
        let info = super::path::parse("GET", "/api/v1/pods", "");
        assert_eq!(labels_for_request(&info, "dryRun=typo").dry_run, "invalid");
    }

    #[test]
    fn render_counts_produces_real_prometheus_text_exposition_format() {
        let counts = vec![((labels("get", "pods"), 200u16), 3u64)];
        let text = render_counts(&counts);
        assert!(text.contains("# HELP apiserver_request_total"));
        assert!(text.contains("# TYPE apiserver_request_total counter"));
        assert!(text.contains("apiserver_request_total{verb=\"get\",dry_run=\"\",group=\"\",version=\"v1\",resource=\"pods\",subresource=\"\",scope=\"namespace\",component=\"apiserver\",code=\"200\"} 3"));
    }

    #[test]
    fn render_counts_is_sorted_for_stable_output() {
        let counts = vec![((labels("list", "services"), 200u16), 1u64), ((labels("get", "pods"), 200u16), 1u64)];
        let text = render_counts(&counts);
        let get_pos = text.find("verb=\"get\"").unwrap();
        let list_pos = text.find("verb=\"list\"").unwrap();
        assert!(get_pos < list_pos, "output should be sorted so repeated scrapes diff cleanly");
    }

    #[test]
    fn render_counts_escapes_label_values() {
        let counts = vec![((labels("get", "weird\"resource"), 200u16), 1u64)];
        let text = render_counts(&counts);
        assert!(text.contains("resource=\"weird\\\"resource\""));
    }

    #[test]
    fn record_request_and_render_round_trip() {
        // Uses the real global table -- a distinct (verb, resource, code)
        // key keeps this test from colliding with any other test's counts.
        record_request(&labels("delete", "a-key-unique-to-this-test"), 204);
        let text = render();
        assert!(text.contains("apiserver_request_total{verb=\"delete\",dry_run=\"\",group=\"\",version=\"v1\",resource=\"a-key-unique-to-this-test\",subresource=\"\",scope=\"namespace\",component=\"apiserver\",code=\"204\"} "));
    }

    #[test]
    fn render_histograms_produces_real_prometheus_text_exposition_format() {
        // 0.15s falls between the real 0.1 and 0.2 buckets -- every
        // bucket from 0.2 onward (inclusive) must be incremented, every
        // bucket below 0.1 (exclusive) must not be.
        let hist = (vec![0u64; DURATION_BUCKETS.len()], 0.0, 0u64);
        let mut counts = hist.0;
        for (i, &b) in DURATION_BUCKETS.iter().enumerate() {
            if 0.15 <= b {
                counts[i] = 1;
            }
        }
        let hists = vec![(labels("get", "pods"), (counts, 0.15, 1u64))];
        let text = render_histograms(&hists);
        assert!(text.contains("# TYPE apiserver_request_duration_seconds histogram"));
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"get\",dry_run=\"\",group=\"\",version=\"v1\",resource=\"pods\",subresource=\"\",scope=\"namespace\",component=\"apiserver\",le=\"0.1\"} 0"));
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"get\",dry_run=\"\",group=\"\",version=\"v1\",resource=\"pods\",subresource=\"\",scope=\"namespace\",component=\"apiserver\",le=\"0.2\"} 1"));
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"get\",dry_run=\"\",group=\"\",version=\"v1\",resource=\"pods\",subresource=\"\",scope=\"namespace\",component=\"apiserver\",le=\"+Inf\"} 1"));
        assert!(text.contains("apiserver_request_duration_seconds_sum{verb=\"get\",dry_run=\"\",group=\"\",version=\"v1\",resource=\"pods\",subresource=\"\",scope=\"namespace\",component=\"apiserver\"} 0.15"));
        assert!(text.contains("apiserver_request_duration_seconds_count{verb=\"get\",dry_run=\"\",group=\"\",version=\"v1\",resource=\"pods\",subresource=\"\",scope=\"namespace\",component=\"apiserver\"} 1"));
    }

    #[test]
    fn record_duration_increments_every_bucket_from_the_observed_value_onward() {
        record_duration(&labels("watch", "a-duration-key-unique-to-this-test"), 0.15);
        let text = render();
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"watch\",dry_run=\"\",group=\"\",version=\"v1\",resource=\"a-duration-key-unique-to-this-test\",subresource=\"\",scope=\"namespace\",component=\"apiserver\",le=\"0.1\"} 0"));
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"watch\",dry_run=\"\",group=\"\",version=\"v1\",resource=\"a-duration-key-unique-to-this-test\",subresource=\"\",scope=\"namespace\",component=\"apiserver\",le=\"0.2\"} 1"));
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"watch\",dry_run=\"\",group=\"\",version=\"v1\",resource=\"a-duration-key-unique-to-this-test\",subresource=\"\",scope=\"namespace\",component=\"apiserver\",le=\"60\"} 1"));
    }

    #[test]
    fn a_value_larger_than_every_bucket_only_appears_in_plus_inf() {
        record_duration(&labels("list", "a-duration-key-unique-to-this-test-2"), 999.0);
        let text = render();
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"list\",dry_run=\"\",group=\"\",version=\"v1\",resource=\"a-duration-key-unique-to-this-test-2\",subresource=\"\",scope=\"namespace\",component=\"apiserver\",le=\"60\"} 0"));
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"list\",dry_run=\"\",group=\"\",version=\"v1\",resource=\"a-duration-key-unique-to-this-test-2\",subresource=\"\",scope=\"namespace\",component=\"apiserver\",le=\"+Inf\"} 1"));
    }

    #[test]
    fn render_watch_event_counts_produces_real_prometheus_text_exposition_format() {
        let counts = vec![(("apps".to_string(), "v1".to_string(), "deployments".to_string()), 5u64)];
        let text = render_watch_event_counts(&counts);
        assert!(text.contains("# HELP apiserver_watch_events_total"));
        assert!(text.contains("# TYPE apiserver_watch_events_total counter"));
        assert!(text.contains("apiserver_watch_events_total{group=\"apps\",version=\"v1\",resource=\"deployments\"} 5"));
    }

    #[test]
    fn record_watch_event_and_render_round_trip() {
        record_watch_event("", "v1", "a-watch-resource-unique-to-this-test");
        let text = render();
        assert!(text.contains("apiserver_watch_events_total{group=\"\",version=\"v1\",resource=\"a-watch-resource-unique-to-this-test\"} "));
    }

    #[test]
    fn render_response_sizes_produces_real_prometheus_text_exposition_format() {
        // 5000 bytes falls between the real 1000 and 10000 buckets.
        let mut counts = vec![0u64; RESPONSE_SIZE_BUCKETS.len()];
        for (i, &b) in RESPONSE_SIZE_BUCKETS.iter().enumerate() {
            if 5000.0 <= b {
                counts[i] = 1;
            }
        }
        let hists = vec![(ResponseLabels::from(&labels("get", "pods")), (counts, 5000.0, 1u64))];
        let text = render_response_sizes(&hists);
        assert!(text.contains("# TYPE apiserver_response_sizes histogram"));
        assert!(text.contains("apiserver_response_sizes_bucket{verb=\"get\",group=\"\",version=\"v1\",resource=\"pods\",subresource=\"\",scope=\"namespace\",component=\"apiserver\",le=\"1000\"} 0"));
        assert!(text.contains("apiserver_response_sizes_bucket{verb=\"get\",group=\"\",version=\"v1\",resource=\"pods\",subresource=\"\",scope=\"namespace\",component=\"apiserver\",le=\"10000\"} 1"));
        assert!(text.contains("apiserver_response_sizes_bucket{verb=\"get\",group=\"\",version=\"v1\",resource=\"pods\",subresource=\"\",scope=\"namespace\",component=\"apiserver\",le=\"+Inf\"} 1"));
        assert!(text.contains("apiserver_response_sizes_sum{verb=\"get\",group=\"\",version=\"v1\",resource=\"pods\",subresource=\"\",scope=\"namespace\",component=\"apiserver\"} 5000"));
    }

    #[test]
    fn record_response_size_and_render_round_trip() {
        record_response_size(&labels("get", "a-size-resource-unique-to-this-test"), 2500);
        let text = render();
        assert!(text.contains("apiserver_response_sizes_bucket{verb=\"get\",group=\"\",version=\"v1\",resource=\"a-size-resource-unique-to-this-test\",subresource=\"\",scope=\"namespace\",component=\"apiserver\",le=\"1000\"} 0"));
        assert!(text.contains("apiserver_response_sizes_bucket{verb=\"get\",group=\"\",version=\"v1\",resource=\"a-size-resource-unique-to-this-test\",subresource=\"\",scope=\"namespace\",component=\"apiserver\",le=\"10000\"} 1"));
    }

    #[test]
    fn render_inflight_counts_uses_the_upstream_request_kind_labels() {
        let text = render_inflight_counts(2, 1);
        assert!(text.contains("# TYPE apiserver_current_inflight_requests gauge"));
        assert!(text.contains("apiserver_current_inflight_requests{request_kind=\"mutating\"} 1"));
        assert!(text.contains("apiserver_current_inflight_requests{request_kind=\"readOnly\"} 2"));
    }

    #[test]
    fn inflight_guard_decrements_the_matching_kind() {
        let before = {
            let mut window = inflight_window().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            window.roll_if_needed(Instant::now());
            window.current[0]
        };
        {
            let _guard = begin_inflight(false);
            let window = inflight_window().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(window.current[0], before + 1);
        }
        let window = inflight_window().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(window.current[0], before);
    }

    #[test]
    fn inflight_window_reports_the_peak_for_one_second() {
        let started = Instant::now();
        let mut window = InflightWindow::new(started);
        window.begin(false, started);
        window.begin(false, started);
        window.finish(false, started);
        assert_eq!(window.peaks(started), [2, 0]);
        assert_eq!(window.peaks(started + Duration::from_secs(1)), [1, 0]);
    }
}
