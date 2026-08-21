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
//! set on a crate this early in its metrics story.
//! `apiserver_request_duration_seconds` (a histogram — real upstream's
//! own latency SLO metric, `k8s.io/apiserver/pkg/endpoints/metrics/
//! metrics.go`'s own `requestLatencies`, fetched and read directly) is
//! now ported too — same `verb`/`resource` label scope as the counter
//! above (real upstream's own histogram carries no `code` label either,
//! so this isn't even a narrowing there), and real upstream's own exact
//! bucket boundaries (its own doc comment: "customize buckets
//! significantly, to empower both" SLO verification and regression
//! tracking, so these aren't arbitrary and shouldn't be re-picked).
//! `apiserver_watch_events_total` (`group`/`version`/`resource` labels,
//! confirmed directly) is ported too — incremented at the exact point
//! real upstream's own `WatchEvents.WithLabelValues(...).Inc()` is
//! called: once per event actually encoded and written to a watch
//! client's connection (`server::listener::encode_watch_event`), not per
//! event this build merely considered and filtered out by a selector.
//! **`apiserver_current_inflight_requests` is deliberately NOT
//! ported**, checked and rejected rather than skipped by omission: its
//! real semantics (`metrics.go`'s own doc comment: "Maximal number of
//! currently used inflight request limit... in last second") measure
//! utilization of real upstream's own APF concurrency-limiting semaphore
//! (`request_kind`: `mutating`/`readonly`), sampled once per second by
//! its own ticker — not a plain "requests currently being handled"
//! count. This build has no concurrency limiter at all yet (Group M's
//! own doc comment: APF's fair-queuing/seat-borrowing half is a
//! genuinely separate, larger, not-yet-started undertaking) — faking
//! this metric from raw in-flight request counts would misrepresent
//! what it actually measures to anyone reading a real Prometheus
//! dashboard, so it's skipped entirely rather than approximated.
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
//! pieces of work, not a quick follow-up to these four.
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

/// `(verb, resource)` — the histogram's own label key, one narrower than
/// [`CounterKey`] since real upstream's own `requestLatencies` carries no
/// `code` label either (this isn't a scope-narrowing choice this port
/// made, it's what upstream itself does).
type HistogramKey = (String, String);

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

fn response_size_histograms() -> &'static Mutex<HashMap<HistogramKey, Histogram>> {
    static RESPONSE_SIZE_HISTOGRAMS: OnceLock<Mutex<HashMap<HistogramKey, Histogram>>> = OnceLock::new();
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
fn record_into(map: &Mutex<HashMap<HistogramKey, Histogram>>, buckets: &[f64], verb: &str, resource: &str, value: f64) {
    let key = (verb.to_string(), resource.to_string());
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
pub fn record_response_size(verb: &str, resource: &str, size_bytes: u64) {
    record_into(response_size_histograms(), RESPONSE_SIZE_BUCKETS, verb, resource, size_bytes as f64);
}

/// Records one completed request's own latency. `resource` follows
/// [`record_request`]'s own empty-string-for-non-resource convention.
/// `seconds` is expected non-negative (a real `Instant::elapsed()`
/// duration always is) — a negative value would simply not increment
/// any bucket, no panic, same "malformed input degrades rather than
/// crashes" posture the rest of this crate's own metrics code takes.
pub fn record_duration(verb: &str, resource: &str, seconds: f64) {
    record_into(histograms(), DURATION_BUCKETS, verb, resource, seconds);
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
fn render_histogram(name: &str, help: &str, buckets: &[f64], hists: &[(HistogramKey, HistogramSnapshot)]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} histogram");
    let mut sorted: Vec<&(HistogramKey, HistogramSnapshot)> = hists.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for ((verb, resource), (bucket_counts, sum, count)) in sorted {
        let verb = escape_label_value(verb);
        let resource = escape_label_value(resource);
        for (bound, cumulative) in buckets.iter().zip(bucket_counts.iter()) {
            let _ = writeln!(out, "{name}_bucket{{verb=\"{verb}\",resource=\"{resource}\",le=\"{bound}\"}} {cumulative}");
        }
        let _ = writeln!(out, "{name}_bucket{{verb=\"{verb}\",resource=\"{resource}\",le=\"+Inf\"}} {count}");
        let _ = writeln!(out, "{name}_sum{{verb=\"{verb}\",resource=\"{resource}\"}} {sum}");
        let _ = writeln!(out, "{name}_count{{verb=\"{verb}\",resource=\"{resource}\"}} {count}");
    }
    out
}

fn render_histograms(hists: &[(HistogramKey, HistogramSnapshot)]) -> String {
    render_histogram("apiserver_request_duration_seconds", "Response latency distribution in seconds for each verb and resource.", DURATION_BUCKETS, hists)
}

fn render_response_sizes(hists: &[(HistogramKey, HistogramSnapshot)]) -> String {
    render_histogram("apiserver_response_sizes", "Response size distribution in bytes for each verb and resource.", RESPONSE_SIZE_BUCKETS, hists)
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
    let response_size_snapshot: Vec<(HistogramKey, HistogramSnapshot)> = response_size_histograms.iter().map(|(k, h)| (k.clone(), (h.bucket_counts.clone(), h.sum, h.count))).collect();
    drop(response_size_histograms);

    let watch_event_counters = watch_event_counters().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let watch_event_snapshot: Vec<(WatchEventKey, u64)> = watch_event_counters.iter().map(|(k, &v)| (k.clone(), v)).collect();
    drop(watch_event_counters);

    render_counts(&counter_snapshot) + &render_histograms(&histogram_snapshot) + &render_response_sizes(&response_size_snapshot) + &render_watch_event_counts(&watch_event_snapshot)
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
        let hists = vec![(("get".to_string(), "pods".to_string()), (counts, 0.15, 1u64))];
        let text = render_histograms(&hists);
        assert!(text.contains("# TYPE apiserver_request_duration_seconds histogram"));
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"get\",resource=\"pods\",le=\"0.1\"} 0"));
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"get\",resource=\"pods\",le=\"0.2\"} 1"));
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"get\",resource=\"pods\",le=\"+Inf\"} 1"));
        assert!(text.contains("apiserver_request_duration_seconds_sum{verb=\"get\",resource=\"pods\"} 0.15"));
        assert!(text.contains("apiserver_request_duration_seconds_count{verb=\"get\",resource=\"pods\"} 1"));
    }

    #[test]
    fn record_duration_increments_every_bucket_from_the_observed_value_onward() {
        record_duration("watch", "a-duration-key-unique-to-this-test", 0.15);
        let text = render();
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"watch\",resource=\"a-duration-key-unique-to-this-test\",le=\"0.1\"} 0"));
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"watch\",resource=\"a-duration-key-unique-to-this-test\",le=\"0.2\"} 1"));
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"watch\",resource=\"a-duration-key-unique-to-this-test\",le=\"60\"} 1"));
    }

    #[test]
    fn a_value_larger_than_every_bucket_only_appears_in_plus_inf() {
        record_duration("list", "a-duration-key-unique-to-this-test-2", 999.0);
        let text = render();
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"list\",resource=\"a-duration-key-unique-to-this-test-2\",le=\"60\"} 0"));
        assert!(text.contains("apiserver_request_duration_seconds_bucket{verb=\"list\",resource=\"a-duration-key-unique-to-this-test-2\",le=\"+Inf\"} 1"));
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
        let hists = vec![(("get".to_string(), "pods".to_string()), (counts, 5000.0, 1u64))];
        let text = render_response_sizes(&hists);
        assert!(text.contains("# TYPE apiserver_response_sizes histogram"));
        assert!(text.contains("apiserver_response_sizes_bucket{verb=\"get\",resource=\"pods\",le=\"1000\"} 0"));
        assert!(text.contains("apiserver_response_sizes_bucket{verb=\"get\",resource=\"pods\",le=\"10000\"} 1"));
        assert!(text.contains("apiserver_response_sizes_bucket{verb=\"get\",resource=\"pods\",le=\"+Inf\"} 1"));
        assert!(text.contains("apiserver_response_sizes_sum{verb=\"get\",resource=\"pods\"} 5000"));
    }

    #[test]
    fn record_response_size_and_render_round_trip() {
        record_response_size("get", "a-size-resource-unique-to-this-test", 2500);
        let text = render();
        assert!(text.contains("apiserver_response_sizes_bucket{verb=\"get\",resource=\"a-size-resource-unique-to-this-test\",le=\"1000\"} 0"));
        assert!(text.contains("apiserver_response_sizes_bucket{verb=\"get\",resource=\"a-size-resource-unique-to-this-test\",le=\"10000\"} 1"));
    }
}
