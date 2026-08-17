//! node-lifecycle-controller: taints a Node `NotReady`/`unreachable` after
//! it stops renewing its heartbeat Lease, so pods can be evicted off it.
//! `GAP_CLOSURE.md` explicitly scopes this to kube-controller-manager, not
//! nodelet — nodelet only clears the one taint that's genuinely its own job
//! (`node.cloudprovider.kubernetes.io/uninitialized`).
//!
//! **Scope of this first slice**: taints only. Upstream's node-lifecycle-
//! controller also evicts the Node's pods once tainted (a rate-limited,
//! per-zone process) and can flip the Node's own `Ready` condition to
//! `Unknown`; neither is implemented here yet — eviction needs its own
//! design pass (a real workqueue with per-zone rate limiting, not something
//! to bolt on silently), and overwriting `status.conditions` from a second
//! writer risks racing nodelet's own status push. Both are named explicitly
//! as follow-up, not silently dropped.
//!
//! # Why the Lease, not `NodeStatus`
//!
//! `nodelet` renews a per-node `Lease` in `kube-node-lease` every
//! `node-monitor-period` (10s in this project's control plane) — the same
//! real upstream `NodeLease` mechanism, chosen because it's far cheaper than
//! the full `NodeStatus` push nodelet does much less often (`node.rs`'s own
//! doc comment: "(conditions, system info) is pushed *infrequently*"). This
//! controller watches that Lease's `renewTime` for liveness, exactly what
//! upstream does — not the heavier NodeStatus object.
//!
//! # Why this is the wheel's first real consumer
//!
//! One entry per Node, rescheduled on every renewal (once per
//! `node-monitor-period`, cluster-wide) — the exact shape
//! `docs/CONTROLLER_MANAGER.md` names as the reason a `BinaryHeap` isn't
//! the right structure here: `wheel::TimingWheel`.

use crate::jitter::jitter;
use crate::wheel::{InsertError, TimingWheel};
use anyhow::Result;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Node, Taint};
use k8s_openapi::jiff::Timestamp;
use kube::runtime::watcher::Event;
use kube::{Api, Client, ResourceExt};
use rand::Rng;
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const NOT_READY_TAINT_KEY: &str = "node.kubernetes.io/not-ready";
pub const UNREACHABLE_TAINT_KEY: &str = "node.kubernetes.io/unreachable";
const NO_EXECUTE: &str = "NoExecute";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleTaint {
    None,
    NotReady,
    Unreachable,
}

/// The pure decision: what taint should this Node carry right now?
///
/// `lease_stale` is "has this Node's heartbeat Lease gone longer than the
/// grace period without renewing" — computed by the caller (the timing-wheel
/// scheduler firing an entry, or a fresh renewal proving the opposite), not
/// read from a clock in here. `ready_status` is the Node's own self-reported
/// `Ready` condition (`"True"`/`"False"`/`"Unknown"`/absent).
///
/// Staleness wins over a self-reported status: once a Node stops
/// heartbeating, its last-written `Ready` value can't be trusted (it might
/// say `True` because the kubelet died mid-cycle, right after writing a
/// healthy status) — this mirrors upstream's own rule that heartbeat
/// liveness is authoritative over the condition payload.
pub fn desired_taint(ready_status: Option<&str>, lease_stale: bool) -> LifecycleTaint {
    if lease_stale {
        return LifecycleTaint::Unreachable;
    }
    match ready_status {
        Some("False") => LifecycleTaint::NotReady,
        _ => LifecycleTaint::None,
    }
}

/// Which of our two taint keys (if either) a Node currently carries.
/// Pure, split out for the same reason `nodelet::node::taints_without` is:
/// the "what's there now" question is worth asserting on its own.
pub fn current_lifecycle_taint(taints: &[Taint]) -> LifecycleTaint {
    if taints.iter().any(|t| t.key == UNREACHABLE_TAINT_KEY) {
        LifecycleTaint::Unreachable
    } else if taints.iter().any(|t| t.key == NOT_READY_TAINT_KEY) {
        LifecycleTaint::NotReady
    } else {
        LifecycleTaint::None
    }
}

/// `existing` with both lifecycle taint keys removed, and `desired`'s taint
/// added back if it isn't `None`. Every other taint (device taints, manual
/// cordons, DRA taints) passes through untouched — this function only ever
/// owns these two specific keys, the same "remove exactly one key, keep
/// everything else" shape as `nodelet::node::taints_without`.
pub fn apply_desired_taint(existing: &[Taint], desired: LifecycleTaint) -> Vec<Taint> {
    let mut kept: Vec<Taint> = existing
        .iter()
        .filter(|t| t.key != NOT_READY_TAINT_KEY && t.key != UNREACHABLE_TAINT_KEY)
        .cloned()
        .collect();
    let key = match desired {
        LifecycleTaint::None => return kept,
        LifecycleTaint::NotReady => NOT_READY_TAINT_KEY,
        LifecycleTaint::Unreachable => UNREACHABLE_TAINT_KEY,
    };
    kept.push(Taint {
        key: key.to_string(),
        effect: NO_EXECUTE.to_string(),
        value: None,
        time_added: None,
    });
    kept
}

fn ready_condition_status(node: &Node) -> Option<String> {
    node.status
        .as_ref()?
        .conditions
        .as_ref()?
        .iter()
        .find(|c| c.type_ == "Ready")
        .map(|c| c.status.clone())
}

/// Converts a wall-clock deadline into a `std::time::Instant` the (monotonic)
/// wheel can use, anchored to one `(now_wall, now_instant)` sample taken at
/// the same moment. `SignedDuration::as_secs_f64` can be negative (a
/// deadline already in the past), which `checked_sub` below turns into
/// "now", not a panic.
fn wall_to_instant(target_wall: Timestamp, now_wall: Timestamp, now_instant: Instant) -> Instant {
    let delta_secs = target_wall.duration_since(now_wall).as_secs_f64();
    if delta_secs >= 0.0 {
        now_instant + Duration::from_secs_f64(delta_secs)
    } else {
        now_instant
            .checked_sub(Duration::from_secs_f64(-delta_secs))
            .unwrap_or(now_instant)
    }
}

async fn reconcile(
    api: &Api<Node>,
    node: &Node,
    ready_status: Option<&str>,
    lease_stale: bool,
    source: &str,
) -> Option<Vec<Taint>> {
    let node_name = node.name_any();
    let desired = desired_taint(ready_status, lease_stale);
    let existing = node
        .spec
        .as_ref()
        .and_then(|s| s.taints.clone())
        .unwrap_or_default();
    if current_lifecycle_taint(&existing) == desired {
        return None; // already correct — no patch, no log noise on every tick
    }
    let new_taints = apply_desired_taint(&existing, desired);
    // `source` names which of the three call sites (node-handler, lease-
    // handler, wheel-tick) made this decision, and `ready_status`/
    // `lease_stale` are its actual inputs — added chasing a real bug
    // (a watch relist redelivering a stale Lease was briefly, wrongly,
    // read as a fresh renewal — fixed in the lease-handler by recomputing
    // staleness instead of assuming it) and kept: every taint *change* is
    // rare enough that this costs nothing at INFO level, and having the
    // decision's own inputs on the line that changed the taint is
    // genuinely the fastest way to read the next one of these, live.
    tracing::info!(
        node = %node_name,
        ?desired,
        source,
        ready_status = ?ready_status,
        lease_stale,
        existing_taints = existing.len(),
        "updating node-lifecycle taint"
    );
    let patch = serde_json::json!({ "spec": { "taints": new_taints.clone() } });
    if let Err(e) = api
        .patch(
            &node_name,
            &kube::api::PatchParams::default(),
            &kube::api::Patch::Merge(&patch),
        )
        .await
    {
        tracing::warn!(node = %node_name, error = ?e, "failed to patch node-lifecycle taint — will retry on the next event");
        return None;
    }
    Some(new_taints)
}

struct NodeLiveness {
    /// The shared Node watch is the source of truth for the fields this
    /// controller owns. Lease renewals and timer-wheel checks must not turn
    /// into a GET storm just to rediscover this already-cached object.
    node: Node,
    ready_status: Option<String>,
    last_renew: Option<Timestamp>,
}

fn merge_cached_lifecycle_taints(node: &mut Node, cached: Option<&Node>) {
    let Some(cached) = cached else { return };
    let cached_owned: Vec<Taint> = cached
        .spec
        .as_ref()
        .and_then(|spec| spec.taints.as_ref())
        .into_iter()
        .flatten()
        .filter(|taint| taint.key == NOT_READY_TAINT_KEY || taint.key == UNREACHABLE_TAINT_KEY)
        .cloned()
        .collect();
    let spec = node.spec.get_or_insert_with(Default::default);
    let mut merged = spec
        .taints
        .take()
        .unwrap_or_default()
        .into_iter()
        .filter(|taint| taint.key != NOT_READY_TAINT_KEY && taint.key != UNREACHABLE_TAINT_KEY)
        .collect::<Vec<_>>();
    merged.extend(cached_owned);
    spec.taints = Some(merged);
}

fn update_cached_taints(cache: &mut HashMap<String, NodeLiveness>, name: &str, taints: Vec<Taint>) {
    if let Some(state) = cache.get_mut(name) {
        state.node.spec.get_or_insert_with(Default::default).taints = Some(taints);
    }
}

fn insert_jittered(
    wheel: &mut TimingWheel<String>,
    key: String,
    base_deadline: Instant,
    interval: Duration,
    jitter_fraction: f64,
    sample: f64,
) -> Result<(), InsertError> {
    let jittered = jitter(interval, jitter_fraction, sample);
    let delta = jittered.as_secs_f64() - interval.as_secs_f64();
    let deadline = if delta >= 0.0 {
        base_deadline + Duration::from_secs_f64(delta)
    } else {
        base_deadline
            .checked_sub(Duration::from_secs_f64(-delta))
            .unwrap_or(base_deadline)
    };
    wheel.insert(key, deadline)
}

pub async fn run(client: Client, cfg: &crate::config::Config) -> Result<()> {
    let node_api: Api<Node> = Api::all(client.clone());
    let mut cache: HashMap<String, NodeLiveness> = HashMap::new();

    // Horizon: grace period plus one full jitter swing plus a tick of
    // margin, so a maximally-jittered entry is never rejected as
    // BeyondHorizon by the wheel it's being inserted into.
    let horizon = cfg
        .node_monitor_grace_period
        .mul_f64(1.0 + cfg.jitter_fraction)
        + cfg.tick_period;
    let slot_count = (horizon.as_nanos() / cfg.tick_period.as_nanos().max(1)).max(1) as u64 + 1;
    let mut wheel: TimingWheel<String> = TimingWheel::new(slot_count, cfg.tick_period, Instant::now());

    let mut nodes = crate::watch::watch_nodes(&client);
    let mut leases = crate::watch::watch_node_leases(&client);
    let mut ticks = tokio::time::interval(cfg.tick_period);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            ev = nodes.next() => {
                match ev {
                    Some(Ok(Event::Apply(node))) | Some(Ok(Event::InitApply(node))) => {
                        let name = node.name_any();
                        let status = ready_condition_status(&node);
                        let stale = is_stale(&cache, &name, cfg.node_monitor_grace_period, cfg.jitter_fraction);
                        let mut node = node;
                        merge_cached_lifecycle_taints(&mut node, cache.get(&name).map(|state| &state.node));
                        let entry = cache.entry(name.clone()).or_insert_with(|| NodeLiveness {
                            node: node.clone(),
                            ready_status: None,
                            last_renew: None,
                        });
                        entry.node = node;
                        entry.ready_status = status.clone();
                        let cached_node = entry.node.clone();
                        if let Some(taints) = reconcile(&node_api, &cached_node, status.as_deref(), stale, "node-handler").await {
                            update_cached_taints(&mut cache, &name, taints);
                        }
                    }
                    Some(Ok(Event::Delete(node))) => {
                        let name = node.name_any();
                        cache.remove(&name);
                        wheel.cancel(&name);
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "node watch error in node-lifecycle-controller"),
                    None => return Ok(()), // stream ended — process exit lets the service manager restart us
                }
            }
            ev = leases.next() => {
                match ev {
                    Some(Ok(Event::Apply(lease))) | Some(Ok(Event::InitApply(lease))) => {
                        let name = lease.name_any();
                        let renew = lease.spec.as_ref().and_then(|s| s.renew_time.as_ref()).map(|t| t.0);
                        if let Some(renew_wall) = renew {
                            let now_wall = Timestamp::now();
                            let now_instant = Instant::now();
                            let deadline_wall = renew_wall + k8s_openapi::jiff::Span::new()
                                .seconds(cfg.node_monitor_grace_period.as_secs() as i64);
                            let deadline_instant = wall_to_instant(deadline_wall, now_wall, now_instant);
                            let sample = rand::thread_rng().gen_range(-1.0..=1.0);
                            if let Err(e) = insert_jittered(
                                &mut wheel,
                                name.clone(),
                                deadline_instant,
                                cfg.node_monitor_grace_period,
                                sample,
                                cfg.jitter_fraction,
                            ) {
                                tracing::warn!(node = %name, error = ?e, "couldn't schedule node-lifecycle recheck");
                            }
                        }
                        let Some(entry) = cache.get_mut(&name) else {
                            // The Node watch will populate the cache and
                            // perform the first reconciliation. Do not issue
                            // a compensating GET from this heartbeat path.
                            continue;
                        };
                        entry.last_renew = renew;
                        let status = entry.ready_status.clone();
                        let cached_node = entry.node.clone();
                        // Recompute staleness from the renewTime this event
                        // actually carried — do NOT assume "false" just
                        // because an Apply event arrived. Found live in CI:
                        // a watch relist (a real, ordinary occurrence —
                        // "too old resource version" is exactly what
                        // triggers one) redelivers an InitApply for the
                        // *same, still-stale* Lease with no new renewal in
                        // it, and treating that arrival itself as proof of
                        // liveness cleared an Unreachable taint the wheel
                        // had *just* correctly set a moment earlier — a
                        // real bug, not a flake, confirmed by the taint
                        // flipping Unreachable→None one second after every
                        // relist, node_lifecycle_controller.sh's own test
                        // catching it via a 90s timeout with the taint
                        // never staying put. A genuinely fresh renewal
                        // computes stale=false here exactly as before; a
                        // relist of a stale one now correctly computes
                        // stale=true and leaves the taint alone.
                        let lease_stale = is_stale(&cache, &name, cfg.node_monitor_grace_period, cfg.jitter_fraction);
                        if let Some(taints) = reconcile(&node_api, &cached_node, status.as_deref(), lease_stale, "lease-handler").await {
                            update_cached_taints(&mut cache, &name, taints);
                        }
                    }
                    Some(Ok(Event::Delete(_) | Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "lease watch error in node-lifecycle-controller"),
                    None => return Ok(()),
                }
            }
            _ = ticks.tick() => {
                let now = Instant::now();
                let due = wheel.advance(now);
                if due.is_empty() { continue; }
                for name in due {
                    let Some(state) = cache.get(&name) else { continue };
                    let status = state.ready_status.clone();
                    let cached_node = state.node.clone();
                    // A renewal can have arrived after this key was placed
                    // in the wheel. Recompute from the cache so a stale
                    // wheel entry cannot taint a live Node.
                    if is_stale(&cache, &name, cfg.node_monitor_grace_period, cfg.jitter_fraction) {
                        if let Some(taints) = reconcile(&node_api, &cached_node, status.as_deref(), true, "wheel-tick").await {
                            update_cached_taints(&mut cache, &name, taints);
                        }
                    }
                }
            }
        }
    }
}

/// Pure: has `last_renew` gone stale as of `now`? Split out from `is_stale`
/// below so the actual staleness arithmetic — the thing that was wrong,
/// twice — is testable without a live clock, the same discipline
/// `wheel.rs`'s `advance()` and `nodescheduler::cycle` both use.
///
/// `jitter_fraction` **must** match the wheel's own — this is the second
/// real bug found live in CI on the same test, after the relist one: the
/// wheel fires on a *jittered* deadline (as early as
/// `grace_period * (1 - jitter_fraction)`, see the wheel's jittered
/// insert), but this function was comparing against the full, unjittered
/// `grace_period`. In the ~jitter-fraction-wide window between those two
/// thresholds, the wheel had already (correctly, by its own schedule)
/// fired and set the taint — and the very next reconcile (triggered by
/// that same patch landing back through the Node watch) used this
/// function, disagreed, and reverted it. Using the same jittered
/// threshold here closes the gap: this can never return `false` for a
/// renewal the wheel has already legitimately treated as due.
fn renewal_is_stale(
    last_renew: Option<Timestamp>,
    now: Timestamp,
    grace_period: Duration,
    jitter_fraction: f64,
) -> bool {
    match last_renew {
        None => false, // never seen a Lease for this Node yet — not our call to make
        Some(renew) => {
            let threshold = grace_period.mul_f64((1.0 - jitter_fraction).max(0.0));
            now.duration_since(renew).as_secs_f64() >= threshold.as_secs_f64()
        }
    }
}

fn is_stale(
    cache: &HashMap<String, NodeLiveness>,
    name: &str,
    grace_period: Duration,
    jitter_fraction: f64,
) -> bool {
    renewal_is_stale(
        cache.get(name).and_then(|s| s.last_renew),
        Timestamp::now(),
        grace_period,
        jitter_fraction,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    #[test]
    fn a_lease_never_seen_is_not_stale() {
        assert!(!renewal_is_stale(
            None,
            t(1000),
            Duration::from_secs(40),
            0.0
        ));
    }

    #[test]
    fn a_recent_renewal_is_not_stale() {
        assert!(!renewal_is_stale(
            Some(t(970)),
            t(1000),
            Duration::from_secs(40),
            0.0
        ));
    }

    #[test]
    fn a_renewal_older_than_the_grace_period_is_stale() {
        assert!(renewal_is_stale(
            Some(t(900)),
            t(1000),
            Duration::from_secs(40),
            0.0
        ));
    }

    /// The regression this bug fix exists for. Found live in CI
    /// (node_lifecycle_controller.sh): a relist redelivers the *same,
    /// unchanged, still-stale* Lease as a fresh `Apply` event — an
    /// ordinary occurrence, not exceptional (a watcher relisting after
    /// "too old resource version" is expected behaviour). Recomputing
    /// staleness from the renewal time the event actually carries, rather
    /// than assuming an Apply event means "fresh", must say stale here —
    /// this is the exact call that was wrongly hardcoded to `false` at the
    /// Lease-handler call site, which cleared an Unreachable taint the
    /// wheel had just correctly set.
    #[test]
    fn a_relisted_but_unchanged_stale_renewal_is_still_stale() {
        assert!(renewal_is_stale(
            Some(t(900)),
            t(1000),
            Duration::from_secs(40),
            0.0
        ));
    }

    /// The second regression, found live in CI the same way, immediately
    /// after the first fix landed: the wheel fires on a *jittered*
    /// deadline (as early as `grace_period * (1 - jitter_fraction)`), but
    /// this function was comparing against the full, unjittered
    /// grace_period. In the gap between those two thresholds — real for a
    /// few seconds around every single staleness detection, not a rare
    /// edge case — the wheel had already, correctly, fired and set the
    /// taint, and the very next reconcile (from the Node watch event that
    /// patch itself generated) called this function, got `false`, and
    /// reverted it: `node_lifecycle_controller.sh`'s taint test timed out
    /// a second time even after the relist fix, because this was still
    /// wrong. Grace period 40s, 5% jitter: the wheel may fire as early as
    /// 38s, so a renewal 39s old — stale by the wheel's own earliest
    /// possible schedule — must already read as stale here too.
    #[test]
    fn a_renewal_within_the_wheels_own_jitter_window_is_already_stale() {
        assert!(renewal_is_stale(
            Some(t(1000)),
            t(1039),
            Duration::from_secs(40),
            0.05
        ));
    }

    #[test]
    fn a_renewal_before_even_the_jittered_earliest_deadline_is_not_stale() {
        assert!(!renewal_is_stale(
            Some(t(1000)),
            t(1030),
            Duration::from_secs(40),
            0.05
        ));
    }

    fn taint(key: &str, effect: &str) -> Taint {
        Taint {
            key: key.to_string(),
            effect: effect.to_string(),
            value: None,
            time_added: None,
        }
    }

    #[test]
    fn a_fresh_healthy_node_gets_no_taint() {
        assert_eq!(desired_taint(Some("True"), false), LifecycleTaint::None);
    }

    #[test]
    fn a_node_reporting_not_ready_gets_the_not_ready_taint() {
        assert_eq!(
            desired_taint(Some("False"), false),
            LifecycleTaint::NotReady
        );
    }

    #[test]
    fn a_stale_lease_means_unreachable_regardless_of_self_reported_status() {
        // Even a Node that last said "True" — its own report can't be
        // trusted once it's stopped heartbeating.
        assert_eq!(
            desired_taint(Some("True"), true),
            LifecycleTaint::Unreachable
        );
        assert_eq!(
            desired_taint(Some("False"), true),
            LifecycleTaint::Unreachable
        );
        assert_eq!(desired_taint(None, true), LifecycleTaint::Unreachable);
    }

    #[test]
    fn an_unknown_or_absent_status_with_a_fresh_lease_gets_no_taint() {
        // Genuinely ambiguous, but the heartbeat itself is fine — leave it
        // to the next reconcile rather than guessing.
        assert_eq!(desired_taint(Some("Unknown"), false), LifecycleTaint::None);
        assert_eq!(desired_taint(None, false), LifecycleTaint::None);
    }

    #[test]
    fn current_lifecycle_taint_prefers_unreachable_over_not_ready() {
        let taints = vec![
            taint(NOT_READY_TAINT_KEY, NO_EXECUTE),
            taint(UNREACHABLE_TAINT_KEY, NO_EXECUTE),
        ];
        assert_eq!(
            current_lifecycle_taint(&taints),
            LifecycleTaint::Unreachable
        );
    }

    #[test]
    fn current_lifecycle_taint_ignores_unrelated_taints() {
        let taints = vec![taint("some.other/taint", "NoSchedule")];
        assert_eq!(current_lifecycle_taint(&taints), LifecycleTaint::None);
    }

    #[test]
    fn apply_desired_taint_preserves_unrelated_taints() {
        let existing = vec![taint("some.other/taint", "NoSchedule")];
        let got = apply_desired_taint(&existing, LifecycleTaint::Unreachable);
        assert!(got.iter().any(|t| t.key == "some.other/taint"));
        assert!(got.iter().any(|t| t.key == UNREACHABLE_TAINT_KEY));
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn apply_desired_taint_replaces_not_ready_with_unreachable_not_stacking_both() {
        let existing = vec![taint(NOT_READY_TAINT_KEY, NO_EXECUTE)];
        let got = apply_desired_taint(&existing, LifecycleTaint::Unreachable);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key, UNREACHABLE_TAINT_KEY);
    }

    #[test]
    fn apply_desired_taint_of_none_clears_both_lifecycle_taints() {
        let existing = vec![
            taint(NOT_READY_TAINT_KEY, NO_EXECUTE),
            taint("keep-me", "NoSchedule"),
        ];
        let got = apply_desired_taint(&existing, LifecycleTaint::None);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key, "keep-me");
    }

    #[test]
    fn wall_to_instant_handles_a_deadline_already_in_the_past() {
        let now_wall = Timestamp::now();
        let now_instant = Instant::now();
        let past = now_wall - k8s_openapi::jiff::Span::new().seconds(30);
        let got = wall_to_instant(past, now_wall, now_instant);
        assert!(got <= now_instant);
    }

    #[test]
    fn wall_to_instant_handles_a_deadline_in_the_future() {
        let now_wall = Timestamp::now();
        let now_instant = Instant::now();
        let future = now_wall + k8s_openapi::jiff::Span::new().seconds(40);
        let got = wall_to_instant(future, now_wall, now_instant);
        assert!(got > now_instant);
        let delta = got.duration_since(now_instant);
        assert!(delta.as_secs() >= 39 && delta.as_secs() <= 41);
    }
}
