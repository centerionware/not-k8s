//! The scheduling cycle — and the rules the rest of this crate is built on.
//!
//! This is the file to read first, the way `nodestore`'s `command.rs` is for
//! the datastore.
//!
//! # The cycle is pure over a snapshot
//!
//! `PreFilter`, `Filter`, `PreScore`, `Score` and `NormalizeScore` read only
//! the [`Snapshot`] they are handed and the pod being placed. No I/O, no
//! clock, no RNG, no environment. That is enforced by the trait signatures in
//! `framework/mod.rs` — those methods receive a snapshot and a pod and nothing
//! else, no client and no context — so it cannot quietly erode the way a
//! convention would.
//!
//! Two reasons, and the second is the one that matters day to day. A placement
//! decision is reproducible from its inputs, so a bad placement can be
//! replayed from a snapshot instead of guessed at. And every scoring formula
//! in `framework/plugins/` is unit-testable against a hand-computed number
//! with no cluster involved, which is the same property that makes
//! `nodeproxy`'s `build_ruleset()` testable.
//!
//! # Nondeterminism is resolved before the cycle, never inside it
//!
//! Exactly two decisions here genuinely need randomness: breaking a tie among
//! equally-scored nodes, and choosing where preemption starts scanning. Both
//! take it as an explicit parameter, chosen by the caller *before* the cycle
//! begins and threaded through — see [`Rng`]. A test pins the seed and gets a
//! deterministic answer; production passes a fresh one per cycle.
//!
//! This mirrors `nodestore`'s rule that the leader resolves nondeterminism
//! before proposing, rather than each replica inventing its own. Same shape,
//! different payoff: there it stops replicas diverging, here it stops a
//! placement being unexplainable after the fact.
//!
//! # One pod at a time, and where "at a time" stops
//!
//! The scheduling cycle is synchronous and handles exactly one pod. Only when
//! it reaches `PreBind` does the work move to the pod's own task and become
//! concurrent with the next pod's cycle.
//!
//! `Reserve` and `Permit` are therefore **in the scheduling cycle**, not the
//! binding cycle — a `Reserve` plugin may assume no other pod is mid-cycle.
//! Every summary that places them in the binding cycle is wrong for
//! implementation purposes, and writing them as if they were concurrent would
//! reintroduce exactly the double-allocation the assume mechanism exists to
//! prevent.
//!
//! # Assumed pods never expire
//!
//! The cycle ends by assuming the pod onto the chosen node, which commits its
//! resources to that node immediately, before any API call. Nothing times that
//! out — see `cache/assume.rs` for why upstream removed the TTL. The
//! reservation is released only by binding failure or by the informer
//! delivering the real bound pod.

use crate::cache::{NodeInfo, PodInfo, Snapshot};
use crate::framework::status::{Code, NodeToStatus, Status};
use crate::framework::{CycleState, Registry, MAX_NODE_SCORE};
use rayon::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Below this many nodes, always consider all of them: the saving is not worth
/// the risk of missing a good placement on a small cluster.
const MIN_FEASIBLE_NODES_TO_FIND: i32 = 100;
/// Never consider fewer than this fraction, however large the cluster.
const MIN_FEASIBLE_NODES_PERCENTAGE: i32 = 5;

/// A tiny SplitMix64, so the crate takes no dependency on `rand`.
///
/// Deliberately seeded and passed in rather than sampled from a thread-local:
/// the whole point is that the cycle's randomness is an input, not an ambient
/// effect. See the module header.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    /// Seed from the clock. Called once per cycle, *outside* the pure region.
    pub fn from_clock() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15);
        Rng(nanos)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, n)`. `n == 0` yields 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        // Rejection removes `% n`'s bias when 2^64 is not divisible by n.
        // The discarded tail is tiny (and zero for powers of two), while
        // every returned bucket has exactly the same number of source values.
        let zone = u64::MAX - (u64::MAX % n);
        loop {
            let value = self.next_u64();
            if value < zone {
                return value % n;
            }
        }
    }
}

/// How many feasible nodes are enough for this cluster size.
///
/// Scoring every node for every pod is wasted work on a large cluster: past a
/// few hundred candidates the marginal placement quality is negligible and the
/// per-cycle cost is not. `percentage == 0` selects upstream's adaptive curve,
/// which tapers from 50% down to a 5% floor as the cluster grows.
///
/// This is only safe because [`advance_start_index`] rotates where the sweep
/// begins — without that, sampling a subset would mean the same prefix of
/// nodes is examined forever.
pub fn num_feasible_nodes_to_find(percentage: i32, num_all_nodes: i32) -> i32 {
    if num_all_nodes < MIN_FEASIBLE_NODES_TO_FIND || percentage >= 100 {
        return num_all_nodes;
    }
    let percentage = if percentage == 0 {
        (50 - num_all_nodes / 125).max(MIN_FEASIBLE_NODES_PERCENTAGE)
    } else {
        percentage
    };
    let n = num_all_nodes * percentage / 100;
    if n < MIN_FEASIBLE_NODES_TO_FIND {
        return MIN_FEASIBLE_NODES_TO_FIND;
    }
    n
}

/// Where the next cycle starts scanning.
///
/// Advances by nodes **processed**, not by nodes found feasible. That
/// distinction is correctness, not fairness: with `percentageOfNodesToScore`
/// below 100 the sweep stops early, and advancing by the smaller number would
/// leave the window creeping forward slower than it scans, so the tail of the
/// cluster would never be reached. Nodes there would receive pods only when
/// the cluster was nearly full — which reads as "some of my nodes are idle and
/// the scheduler won't use them".
pub fn advance_start_index(current: usize, processed: usize, num_all_nodes: usize) -> usize {
    if num_all_nodes == 0 {
        return 0;
    }
    (current + processed) % num_all_nodes
}

/// Pick among the highest-scoring nodes by reservoir sampling.
///
/// Each of the `k` tied nodes is chosen with probability `1/k`, in one pass
/// and without collecting them. Taking the first tied node instead is the
/// obvious implementation and hot-spots badly: on a fresh homogeneous cluster
/// every node scores identically, so every pod in a Deployment lands on
/// whichever node happens to sort first.
pub fn select_host(scores: &[(String, i64)], rng: &mut Rng) -> Option<String> {
    let mut best: Option<&str> = None;
    let mut best_score = i64::MIN;
    let mut tied = 0u64;

    for (name, score) in scores {
        if *score > best_score {
            best_score = *score;
            best = Some(name);
            tied = 1;
        } else if *score == best_score {
            tied += 1;
            // Replace with probability 1/tied — the reservoir step.
            if rng.below(tied) == 0 {
                best = Some(name);
            }
        }
    }
    best.map(String::from)
}

/// What a cycle concluded.
pub enum CycleOutcome {
    /// Place the pod here. The caller assumes it and starts the binding cycle.
    Scheduled { node: String },
    /// Nothing fits. The caller parks the pod, blaming these plugins so the
    /// queue knows which events could revive it.
    Unschedulable {
        reason: String,
        unschedulable_plugins: Vec<&'static str>,
        pending_plugins: Vec<&'static str>,
        /// Set once preemption has promised this pod a node.
        nominated_node: Option<String>,
        /// Why each node was rejected. Carried out of the cycle because
        /// preemption's first question is which nodes eviction could
        /// plausibly fix — the ones rejected `Unschedulable` rather than
        /// `UnschedulableAndUnresolvable` — and that answer only exists here.
        node_statuses: NodeToStatus,
    },
    /// A plugin itself failed. Retried; never treated as a placement decision.
    Error { reason: String },
}

/// Everything a cycle needs that is not the pod, minus the plugin registry.
///
/// The registry is *not* a field here, deliberately — see the module's
/// "Multiple profiles, one Scheduler" section below. Everything that genuinely
/// is per-cycle-but-cluster-wide state (the round-robin sweep position, and
/// which nodes are already promised to a pending preemption) lives here
/// instead, shared across every profile's cycles because they all compete for
/// the same nodes.
///
/// # Multiple profiles, one `Scheduler`
///
/// A cluster can run several `spec.schedulerName` profiles out of one
/// process, sharing one queue and one `QueueSort` — see `docs/SCHEDULER.md`'s
/// "Phase 5" and `lib.rs`'s `schedule_forever`. What must **not** be shared
/// per profile is `next_start_node_index`/`nominator`: two profiles placing
/// pods onto the same nodes have to see one consistent sweep position and one
/// consistent set of preemption promises, or they double-book exactly the way
/// two pods within one profile would. So `Scheduler` holds no `Registry` at
/// all; every method below takes the calling cycle's `&Registry` as an
/// explicit parameter, resolved by the caller from `pod.scheduler_name`.
pub struct Scheduler {
    pub percentage_of_nodes_to_score: i32,
    parallelism: usize,
    workers: rayon::ThreadPool,
    /// Rotates across cycles; see [`advance_start_index`].
    pub next_start_node_index: usize,
    /// Pods that have preempted and are waiting for their victims to drain.
    ///
    /// Shared (not owned outright) with `watch::WatchTargets` — a pod
    /// deleted before it ever reaches this cycle's own "scheduled" branch
    /// (see `lib.rs`) needs its nomination cleared from the same map the
    /// watch task can reach, or it leaks: `Nominator` has no TTL and a
    /// filter treats every nominee on a node as permanently there (see
    /// `find_feasible_nodes`'s nominees loop below) until something calls
    /// `remove`.
    pub nominator: Arc<Mutex<crate::preempt::Nominator>>,
}

impl Scheduler {
    pub fn new(
        percentage_of_nodes_to_score: i32,
        parallelism: usize,
        nominator: Arc<Mutex<crate::preempt::Nominator>>,
    ) -> Self {
        let parallelism = parallelism.max(1);
        Self {
            percentage_of_nodes_to_score,
            parallelism,
            workers: rayon::ThreadPoolBuilder::new()
                .num_threads(parallelism)
                .thread_name(|i| format!("nodescheduler-worker-{i}"))
                .build()
                .expect("a positive NODESCHEDULER_PARALLELISM builds a worker pool"),
            next_start_node_index: 0,
            nominator,
        }
    }

    /// Run one full scheduling cycle for one pod, against `registry` — the
    /// profile this pod's `spec.schedulerName` resolved to.
    ///
    /// Returns the [`CycleState`] alongside the outcome: PreFilter and Reserve
    /// wrote into it, and the binding cycle needs exactly that state to run
    /// PreBind and — on failure — to unwind the right reservations. Dropping
    /// it here instead would make `unreserve` a no-op, and a plugin that had
    /// claimed something would leak it on every failed bind.
    pub async fn schedule_one(
        &mut self,
        registry: &Registry,
        extenders: &[crate::extender::Extender],
        pod: &PodInfo,
        snapshot: &Snapshot,
        rng: &mut Rng,
    ) -> (CycleOutcome, CycleState) {
        if snapshot.is_empty() {
            return (
                CycleOutcome::Unschedulable {
                    reason: "no nodes available to schedule pods".to_string(),
                    // Not an empty list: an empty `unschedulable_plugins`
                    // tells the queue no registered hint applies, so
                    // nothing ever re-wakes this pod except the blind
                    // backoff timer — found live in CI as
                    // test_scheduler_consults_an_http_extender_and_honours_a_filter_rejection
                    // flaking whenever its pod's very first cycle lands in
                    // the empty-cache window right after a scheduler
                    // restart: the pod got parked with no event
                    // subscription at all and never got a second cycle to
                    // actually reach the extender. `NodeResourcesFit` is
                    // unconditionally present in every profile and its own
                    // `events_to_register()` already reacts to `Node ADD`
                    // — reusing it here is exactly the event that resolves
                    // "there were no nodes".
                    unschedulable_plugins: vec![crate::framework::plugins::node_resources_fit::NAME],
                    pending_plugins: Vec::new(),
                    nominated_node: None,
                    node_statuses: NodeToStatus::default(),
                },
                CycleState::default(),
            );
        }

        let mut state = CycleState::default();
        // ImageLocality's upstream implementation reads the full scheduler
        // snapshot from its framework handle during Score. Preserve that
        // contract even though later filtering passes only feasible nodes to
        // PreScore; the summary itself is maintained at Node-watch time.
        crate::framework::plugins::image_locality::prepare_state(&mut state, snapshot);

        // ── PreFilter ───────────────────────────────────────────────────
        let mut restricted: Option<Vec<String>> = None;
        for plugin in &registry.pre_filter {
            let (status, nodes) = plugin.pre_filter(&mut state, pod, snapshot);
            match status.code {
                Code::Success | Code::Skip => {}
                Code::Error => {
                    return (CycleOutcome::Error { reason: status.to_string() }, state);
                }
                _ => {
                    // A whole-cluster rejection: no point looking at nodes.
                    let (unschedulable_plugins, pending_plugins) =
                        if status.code == Code::Pending {
                            (Vec::new(), vec![status.plugin])
                        } else {
                            (vec![status.plugin], Vec::new())
                        };
                    return (
                        CycleOutcome::Unschedulable {
                            reason: status.to_string(),
                            unschedulable_plugins,
                            pending_plugins,
                            nominated_node: None,
                            node_statuses: NodeToStatus::default(),
                        },
                        state,
                    );
                }
            }
            // Intersect rather than replace: two plugins each naming a node
            // subset both have to be satisfied.
            if let Some(names) = nodes {
                restricted = Some(match restricted {
                    None => names,
                    Some(existing) => {
                        existing.into_iter().filter(|n| names.contains(n)).collect()
                    }
                });
            }
        }

        // Computed before Filter, not just before scoring: `find_feasible_nodes`
        // itself needs to know whether an extender can still turn one more
        // feasible node into a meaningful choice, or its own "nothing to
        // score, stop at one" short-circuit (`registry.score.is_empty()`)
        // would strand an extender-only prioritizer at a single candidate
        // before it ever got a say.
        let has_extender_filter_or_scoring = extenders.iter().any(|e| {
            e.config.filter_verb.is_some() || e.config.prioritize_verb.is_some()
        });

        // A pod which already preempted victims is overwhelmingly likely to
        // fit only the node it was promised. Upstream evaluates that node
        // first and returns immediately when it passes, before doing a normal
        // cluster sweep or scoring unrelated nodes.
        let preferred_nomination = pod.nominated_node_name.clone().or_else(|| {
            self.nominator
                .lock()
                .unwrap()
                .nominated_node(&pod.uid)
                .map(str::to_string)
        });
        if let Some(name) = preferred_nomination {
            let allowed = restricted.as_ref().is_none_or(|nodes| nodes.contains(&name));
            if allowed {
                if let Some(node) = snapshot.node(&name) {
                    if self.filter_one_node(registry, &mut state, pod, node).is_none() {
                        let mut preferred = vec![node.clone()];
                        let mut extender_failed = false;
                        for extender in extenders {
                            if !extender.config.applies_to(pod) {
                                continue;
                            }
                            let refs: Vec<&NodeInfo> =
                                preferred.iter().map(|candidate| candidate.as_ref()).collect();
                            match extender.filter(pod, &refs).await {
                                Ok(Some(outcome)) => {
                                    if let Some(nodes) = outcome.replacement_nodes {
                                        preferred = nodes
                                            .into_iter()
                                            .map(|api| {
                                                let mut projected = NodeInfo::default();
                                                projected.update_from_node(&api, 0);
                                                projected.api_object = Some(Box::new(api));
                                                Arc::new(projected)
                                            })
                                            .collect();
                                    } else {
                                        preferred.retain(|candidate| {
                                            outcome.passed.contains(&candidate.name)
                                        });
                                    }
                                    if preferred.is_empty() {
                                        extender_failed = true;
                                        break;
                                    }
                                }
                                Ok(None) => {}
                                Err(error) if extender.config.ignorable => {
                                    tracing::warn!(extender = %extender.config.url_prefix, %error, "ignorable extender failed while evaluating nominated node");
                                }
                                Err(error) => {
                                    // The ordinary sweep below repeats the
                                    // call and owns the final cycle error,
                                    // matching upstream's nominated-node fast
                                    // path falling back after an evaluation
                                    // error.
                                    tracing::warn!(extender = %extender.config.url_prefix, %error, "nominated-node extender evaluation failed; falling back to normal sweep");
                                    extender_failed = true;
                                    break;
                                }
                            }
                        }
                        if !extender_failed {
                            if let Some(chosen) = preferred.first() {
                                return (
                                    CycleOutcome::Scheduled { node: chosen.name.clone() },
                                    state,
                                );
                            }
                        }
                    }
                }
            }
        }

        // ── Filter ──────────────────────────────────────────────────────
        let (feasible, node_statuses, processed) =
            self.find_feasible_nodes(
                registry,
                &mut state,
                pod,
                snapshot,
                restricted.as_deref(),
                has_extender_filter_or_scoring,
            );

        self.next_start_node_index =
            advance_start_index(self.next_start_node_index, processed, snapshot.num_nodes());

        // A Filter or PreFilterExtension error is a cycle failure, not an
        // ordinary rejection attached to one node. In particular, a second
        // feasible node must not hide that the cycle used invalid state.
        if let Some(status) = node_statuses.first_error() {
            return (CycleOutcome::Error { reason: status.to_string() }, state);
        }

        // ── HTTP extenders' Filter ──────────────────────────────────────
        //
        // Run in configured order, each one narrowing what the last left —
        // upstream's own `findNodesThatPassExtenders` does the same
        // sequential narrowing rather than intersecting independent calls,
        // so a later extender only ever sees nodes every earlier one already
        // accepted. A node an extender rejects becomes `Unschedulable` (its
        // own reason recorded, same as a plugin's); one on the *unresolvable*
        // list is excluded from preemption candidacy the same way a plugin's
        // `UnschedulableAndUnresolvable` verdict is. An `ignorable` extender
        // that errors is logged and skipped rather than failing the whole
        // cycle — that is the entire meaning of the flag.
        let mut feasible = feasible;
        let mut node_statuses = node_statuses;
        for extender in extenders {
            if feasible.is_empty() {
                break;
            }
            if !extender.config.applies_to(pod) {
                continue; // managedResources set, and pod requests none of them
            }
            let refs: Vec<&NodeInfo> = feasible.iter().map(|n| n.as_ref()).collect();
            match extender.filter(pod, &refs).await {
                Ok(Some(outcome)) => {
                    for (node, reason) in &outcome.failed {
                        node_statuses.record(node.clone(), Status::unschedulable("HTTPExtender", reason.clone()));
                    }
                    for (node, reason) in &outcome.failed_unresolvable {
                        node_statuses.record(node.clone(), Status::unresolvable("HTTPExtender", reason.clone()));
                    }
                    if let Some(nodes) = outcome.replacement_nodes {
                        feasible = nodes
                            .into_iter()
                            .map(|api| {
                                let mut node = NodeInfo::default();
                                node.update_from_node(&api, 0);
                                node.api_object = Some(Box::new(api));
                                Arc::new(node)
                            })
                            .collect();
                    } else {
                        feasible.retain(|n| outcome.passed.contains(&n.name));
                    }
                }
                Ok(None) => {} // this extender has no filterVerb configured
                Err(e) => {
                    if extender.config.ignorable {
                        tracing::warn!(extender = %extender.config.url_prefix, error = %e, "ignorable extender filter call failed; continuing without it");
                    } else {
                        return (CycleOutcome::Error { reason: e.to_string() }, state);
                    }
                }
            }
        }

        if feasible.is_empty() {
            // ── PostFilter (preemption) ─────────────────────────────────
            for plugin in &registry.post_filter {
                let (status, nominated) =
                    plugin.post_filter(&mut state, pod, snapshot, &node_statuses).await;
                if status.is_success() || nominated.is_some() {
                    return (
                        CycleOutcome::Unschedulable {
                            reason: status.to_string(),
                            unschedulable_plugins: node_statuses.rejecting_plugins(),
                            pending_plugins: Vec::new(),
                            nominated_node: nominated,
                            node_statuses,
                        },
                        state,
                    );
                }
            }
            return (
                CycleOutcome::Unschedulable {
                    reason: node_statuses.summary(snapshot.num_nodes()),
                    unschedulable_plugins: node_statuses.rejecting_plugins(),
                    pending_plugins: Vec::new(),
                    nominated_node: None,
                    node_statuses,
                },
                state,
            );
        }

        // A single candidate needs no scoring, extenders included — there is
        // nothing left to distinguish it from. With more than one candidate,
        // scoring can still matter even with zero Score *plugins* if an
        // extender configures `prioritizeVerb` — `any_prioritizer` above is
        // exactly what let `find_feasible_nodes` collect more than one
        // candidate in the first place.
        let any_prioritizer = extenders.iter().any(|e| e.config.prioritize_verb.is_some());
        if feasible.len() == 1 || (registry.score.is_empty() && !any_prioritizer) {
            return (CycleOutcome::Scheduled { node: feasible[0].name.clone() }, state);
        }

        // ── PreScore / Score / Normalize ────────────────────────────────
        let refs: Vec<&NodeInfo> = feasible.iter().map(|n| n.as_ref()).collect();
        for plugin in &registry.pre_score {
            let status = plugin.pre_score(&mut state, pod, &refs);
            if status.code == Code::Error {
                return (CycleOutcome::Error { reason: status.to_string() }, state);
            }
        }

        let mut totals: Vec<(String, i64)> =
            feasible.iter().map(|n| (n.name.clone(), 0i64)).collect();

        for plugin in &registry.score {
            if state.score_skipped(plugin.name()) {
                continue;
            }
            let scored: Vec<Result<i64, Status>> = self.workers.install(|| {
                feasible
                    .par_iter()
                    .map(|node| plugin.score(&state, pod, node))
                    .collect()
            });
            let mut raw = Vec::with_capacity(scored.len());
            for score in scored {
                match score {
                    Ok(value) => raw.push(value),
                    Err(status) => return (CycleOutcome::Error { reason: status.to_string() }, state),
                }
            }
            let status = plugin.normalize(&state, pod, &mut raw);
            if status.code == Code::Error {
                return (CycleOutcome::Error { reason: status.to_string() }, state);
            }
            let weight = plugin.weight();
            for (total, score) in totals.iter_mut().zip(raw.iter()) {
                // Upstream rejects an invalid normalized score as a framework
                // error. Silently clamping it can select a different node and
                // hides a broken plugin configuration.
                if !(0..=MAX_NODE_SCORE).contains(score) {
                    return (
                        CycleOutcome::Error {
                            reason: format!(
                                "plugin {:?} returned score {score} outside [0, {MAX_NODE_SCORE}] after normalizing",
                                plugin.name()
                            ),
                        },
                        state,
                    );
                }
                total.1 = total.1.saturating_add(score.saturating_mul(weight));
            }
        }

        // ── HTTP extenders' Prioritize ──────────────────────────────────
        //
        // Added into the already-normalized-and-weighted plugin totals.
        // `Extender::prioritize` already applied upstream's own rescale
        // (`score * weight * MAX_NODE_SCORE / MaxExtenderPriority`), so what
        // lands here is on the same `[0, MAX_NODE_SCORE]`-per-weight-unit
        // scale a plugin's contribution is — see that function's doc
        // comment for the upstream source this was checked against. An
        // `ignorable` extender that errors here is skipped, not fatal — the
        // pod still gets placed using whatever scores plugins and any other
        // extenders already produced.
        let refs: Vec<&NodeInfo> = feasible.iter().map(|n| n.as_ref()).collect();
        let calls = extenders
            .iter()
            .filter(|extender| extender.config.applies_to(pod))
            .map(|extender| {
                let refs = refs.as_slice();
                async move { (extender, extender.prioritize(pod, refs).await) }
            });
        for (extender, result) in futures::future::join_all(calls).await {
            match result {
                Ok(Some(scores)) => {
                    for (host, score) in scores {
                        if let Some(total) = totals.iter_mut().find(|(n, _)| *n == host) {
                            total.1 = total.1.saturating_add(score);
                        }
                    }
                }
                Ok(None) => {} // this extender has no prioritizeVerb configured
                Err(e) => {
                    // Upstream ignores Prioritize failures for every extender,
                    // irrespective of `ignorable`; Filter and preemption are
                    // the phases where that flag controls fatality.
                    tracing::warn!(extender = %extender.config.url_prefix, error = %e, "extender prioritize call failed; continuing without its score");
                }
            }
        }

        let outcome = match select_host(&totals, rng) {
            Some(node) => CycleOutcome::Scheduled { node },
            None => CycleOutcome::Error {
                reason: "scoring produced no candidate despite feasible nodes".to_string(),
            },
        };
        (outcome, state)
    }

    /// Run the Filter chain for exactly one node, including the nominated-pod
    /// PreFilterExtension accounting installed and then undone around that
    /// node's predicates.
    fn filter_one_node(
        &self,
        registry: &Registry,
        state: &mut CycleState,
        pod: &PodInfo,
        node: &NodeInfo,
    ) -> Option<Status> {
        let nominees: Vec<Arc<PodInfo>> = self
            .nominator
            .lock()
            .unwrap()
            .nominated_on(&node.name)
            .into_iter()
            .filter(|nominee| nominee.priority >= pod.priority && nominee.uid != pod.uid)
            .collect();
        let mut rejected = None;
        let mut added_nominees: Vec<(usize, usize)> = Vec::new();
        for (plugin_index, plugin) in registry.pre_filter.iter().enumerate() {
            if let Some(extension) = plugin.extensions() {
                for (nominee_index, nominee) in nominees.iter().enumerate() {
                    let status = extension.add_pod(state, pod, nominee, node);
                    if !status.is_success() && !status.is_skip() {
                        rejected = Some(status);
                        break;
                    }
                    added_nominees.push((plugin_index, nominee_index));
                }
            }
            if rejected.is_some() {
                break;
            }
        }

        if rejected.is_none() {
            rejected = Self::run_filters(registry, state, pod, node);
        }

        for (plugin_index, nominee_index) in added_nominees.into_iter().rev() {
            if let Some(extension) = registry.pre_filter[plugin_index].extensions() {
                let status = extension.remove_pod(state, pod, &nominees[nominee_index], node);
                if rejected.is_none() && !status.is_success() && !status.is_skip() {
                    rejected = Some(status);
                }
            }
        }
        rejected
    }

    /// Apply the ordinary Filter plugins once. Both the sequential path
    /// (which also installs nominated-pod extension state) and the parallel
    /// path use this same status/skip handling.
    fn run_filters(
        registry: &Registry,
        state: &CycleState,
        pod: &PodInfo,
        node: &NodeInfo,
    ) -> Option<Status> {
        registry.filter.iter().find_map(|plugin| {
            if state.filter_skipped(plugin.name()) {
                return None;
            }
            let status = plugin.filter(state, pod, node);
            (!status.is_success() && !status.is_skip()).then_some(status)
        })
    }

    /// Sweep nodes until enough are feasible, starting where the last cycle
    /// left off.
    ///
    /// Returns the feasible nodes, why the rest were rejected, and how many
    /// nodes were examined — the last of which is what
    /// [`advance_start_index`] needs, and is *not* the same as the number
    /// found.
    fn find_feasible_nodes(
        &self,
        registry: &Registry,
        state: &mut CycleState,
        pod: &PodInfo,
        snapshot: &Snapshot,
        restricted: Option<&[String]>,
        has_extender_filter_or_scoring: bool,
    ) -> (Vec<Arc<NodeInfo>>, NodeToStatus, usize) {
        let all = snapshot.nodes();
        let num_all = all.len();
        let wanted = if registry.score.is_empty() && !has_extender_filter_or_scoring {
            // With nothing to compare on, the first feasible node is as good
            // as the best one, so stop at one. An extender's own
            // `prioritizeVerb` counts as "something to compare on" even when
            // no built-in Score plugin is registered — otherwise the sweep
            // would strand a profile with an extender-only prioritizer at a
            // single candidate before the extender ever got a say.
            1
        } else {
            num_feasible_nodes_to_find(self.percentage_of_nodes_to_score, num_all as i32)
                .max(1) as usize
        };

        let mut feasible = Vec::new();
        let mut statuses = NodeToStatus::default();
        let mut processed = 0usize;

        // Nominated pods mutate PreFilterExtension state while one node is
        // evaluated, so that uncommon preemption-drain path remains
        // sequential. The normal path is read-only and fans Filter out over
        // the configured worker pool in bounded waves, stopping after the
        // first wave that reaches the upstream feasible-node target.
        if self.parallelism > 1 && self.nominator.lock().unwrap().is_empty() {
            let allowed: Option<std::collections::HashSet<&str>> =
                restricted.map(|names| names.iter().map(String::as_str).collect());
            let ordered: Vec<Arc<NodeInfo>> = (0..num_all)
                .map(|i| all[(self.next_start_node_index + i) % num_all].clone())
                .collect();

            for wave in ordered.chunks(self.parallelism) {
                let allowed_wave: Vec<Arc<NodeInfo>> = wave
                    .iter()
                    .filter(|node| allowed.as_ref().is_none_or(|set| set.contains(node.name.as_str())))
                    .cloned()
                    .collect();
                let results: Vec<Option<Status>> = self.workers.install(|| {
                    allowed_wave.par_iter()
                        .map(|node| Self::run_filters(registry, state, pod, node))
                        .collect()
                });
                processed += wave.len();
                for (node, rejected) in allowed_wave.iter().zip(results) {
                    match rejected {
                        None => feasible.push(node.clone()),
                        Some(status) => statuses.record(node.name.clone(), status),
                    }
                }
                if feasible.len() >= wanted || statuses.first_error().is_some() {
                    feasible.truncate(wanted);
                    return (feasible, statuses, processed);
                }
            }
            return (feasible, statuses, processed);
        }

        for i in 0..num_all {
            let node = &all[(self.next_start_node_index + i) % num_all];
            processed += 1;

            if let Some(allowed) = restricted {
                if !allowed.contains(&node.name) {
                    continue;
                }
            }

            let rejected = self.filter_one_node(registry, state, pod, node);

            match rejected {
                None => {
                    feasible.push(node.clone());
                    if feasible.len() >= wanted { break; }
                }
                Some(status) => statuses.record(node.name.clone(), status),
            }
        }

        (feasible, statuses, processed)
    }
}

/// Run the Reserve plugins, unwinding in reverse on the first failure.
///
/// Unwinding in reverse matters for the same reason it does for locks: a
/// plugin's `unreserve` may depend on state a later plugin's `reserve` has not
/// touched, and releasing in acquisition order can free something still
/// referenced.
pub fn run_reserve(registry: &Registry, state: &mut CycleState, pod: &PodInfo, node: &str) -> Status {
    for (i, plugin) in registry.reserve.iter().enumerate() {
        let status = plugin.reserve(state, pod, node);
        if !status.is_success() {
            for done in registry.reserve[..i].iter().rev() {
                done.unreserve(state, pod, node);
            }
            return status;
        }
    }
    Status::success()
}

/// Release every Reserve plugin, in reverse order. Must not fail.
pub fn run_unreserve(registry: &Registry, state: &mut CycleState, pod: &PodInfo, node: &str) {
    for plugin in registry.reserve.iter().rev() {
        plugin.unreserve(state, pod, node);
    }
}

#[cfg(test)]
#[path = "cycle_tests.rs"]
mod tests;

// ── Preemption ──────────────────────────────────────────────────────────
//
// Driven from here rather than from a `PostFilterPlugin`, and the reason is
// structural rather than behavioural. Preemption's dry runs must re-run the
// Filter plugins against a hypothetical pod set, and a plugin is not handed
// the other plugins — upstream solves this by passing a framework Handle into
// the plugin, which is a larger surface than this crate needs for one caller.
//
// The behaviour is unchanged: it runs only when zero nodes were feasible,
// only over nodes rejected as `Unschedulable`, and produces the same victims
// upstream's DefaultPreemption would. `preempt.rs` holds every rule; this is
// the part that needs the registry.

/// What a preemption attempt concluded.
#[derive(Debug)]
pub struct PreemptionOutcome {
    /// The node promised to the preemptor.
    pub nominated_node: String,
    /// Pods that must be deleted before it can be placed, as `namespace/name`.
    pub victims: Vec<String>,
}

impl Scheduler {
    /// Try to make room for `pod` by evicting less important pods.
    ///
    /// Returns `None` when preemption is not eligible, no node is a
    /// candidate, or no victim set makes the pod fit — all of which mean
    /// "leave the cluster alone", which is the right answer far more often
    /// than not.
    pub async fn preempt(
        &self,
        registry: &Registry,
        extenders: &[crate::extender::Extender],
        pod: &PodInfo,
        snapshot: &Snapshot,
        node_statuses: &NodeToStatus,
        budgets: &[crate::preempt::PdbState],
        rng: &mut Rng,
    ) -> anyhow::Result<Option<PreemptionOutcome>> {
        use crate::preempt::{
            eligible_to_preempt, offset_and_num_candidates, pick_one_node, select_victims_on_node,
            Candidate,
        };

        // A nomination still draining means room is already being made; a
        // second attempt would evict a second set of pods for it.
        let nominated = self
            .nominator
            .lock()
            .unwrap()
            .nominated_node(&pod.uid)
            .map(str::to_string)
            .or_else(|| pod.nominated_node_name.clone());
        let draining = nominated
            .as_deref()
            .and_then(|n| snapshot.node(n))
            .map(|n| {
                n.pods
                    .iter()
                    .any(|p| p.priority < pod.priority && p.terminating_by_preemption)
            })
            .unwrap_or(false);
        let nominated_unresolvable = nominated
            .as_deref()
            .and_then(|node| node_statuses.for_node(node))
            .is_some_and(|status| {
                status.code == crate::framework::status::Code::UnschedulableAndUnresolvable
            });
        if eligible_to_preempt(
            pod.preemption_policy.as_deref(),
            draining,
            nominated_unresolvable,
        )
        .is_err()
        {
            return Ok(None);
        }

        // Only nodes eviction could actually fix. A node rejected as
        // UnschedulableAndUnresolvable — wrong name, unmatched affinity, no
        // topology domain — stays rejected however many pods die on it.
        let candidates_by_name = node_statuses.preemption_candidates();
        if candidates_by_name.is_empty() {
            return Ok(None);
        }

        let (offset, wanted) =
            offset_and_num_candidates(candidates_by_name.len() as i32, rng);

        // Match upstream's DryRunPreemption: nodes are evaluated in parallel,
        // each PDB-safe/PDB-violating bucket retains at most `wanted`
        // candidates, and cancellation only starts once there is at least one
        // PDB-safe choice and the combined shortlist is large enough. Stopping
        // at `wanted` violating candidates would miss a clean node immediately
        // after them and evict through a budget unnecessarily.
        #[derive(Default)]
        struct CandidateBuckets {
            non_violating: Vec<Candidate>,
            violating: Vec<Candidate>,
            errors: Vec<Status>,
        }

        let buckets = Mutex::new(CandidateBuckets::default());
        let cancelled = AtomicBool::new(false);
        self.workers.install(|| {
            (0..candidates_by_name.len()).into_par_iter().for_each(|i| {
                if cancelled.load(Ordering::Acquire) {
                    return;
                }
                let idx = (offset as usize + i) % candidates_by_name.len();
                let Some(node) = snapshot.node(candidates_by_name[idx]) else {
                    return;
                };

                // A fresh CycleState per node: PreFilter's per-cycle work is
                // cheap to redo and sharing one across nodes would leak one
                // node's hypothetical removals into the next node's answer.
                let mut state = CycleState::default();
                for plugin in &registry.pre_filter {
                    plugin.pre_filter(&mut state, pod, snapshot);
                }

                let mut filter_error = None;
                let mut node_budgets = budgets.to_vec();
                let victims = select_victims_on_node(pod, node, &mut node_budgets, |removed| {
                    match self.fits_without(registry, &mut state, pod, node, removed) {
                        Ok(fits) => fits,
                        Err(status) => {
                            filter_error = Some(status);
                            false
                        }
                    }
                });

                let mut buckets = buckets.lock().unwrap();
                if let Some(status) = filter_error {
                    buckets.errors.push(status);
                    return;
                }
                let Some(victims) = victims else { return };
                let non_violating = victims.pdb_violations == 0;
                let target = if non_violating {
                    &mut buckets.non_violating
                } else {
                    &mut buckets.violating
                };
                if target.len() < wanted as usize {
                    target.push(Candidate::from_victims(node, victims));
                }
                if !buckets.non_violating.is_empty()
                    && buckets.non_violating.len() + buckets.violating.len() >= wanted as usize
                {
                    cancelled.store(true, Ordering::Release);
                }
            });
        });

        let mut buckets = buckets.into_inner().unwrap();
        if !buckets.errors.is_empty() {
            anyhow::bail!(
                "preemption Filter dry run failed: {}",
                buckets
                    .errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }
        let mut found = buckets.non_violating;
        found.append(&mut buckets.violating);

        // Preemption-capable extenders run sequentially and each receives the
        // candidate map returned by the previous one. A non-ignorable error
        // aborts preemption; an ignorable error preserves the current map.
        for extender in extenders.iter().filter(|extender| {
            extender.config.preempt_verb.is_some() && extender.config.applies_to(pod)
        }) {
            match extender.process_preemption(pod, &found, snapshot).await {
                Ok(Some(replaced)) => {
                    found = replaced
                        .into_iter()
                        .map(|selection| {
                            let node = snapshot.node(&selection.node).expect("extender resolved a snapshot node");
                            Candidate::from_victims(
                                node,
                                crate::preempt::Victims {
                                    pods: selection.pod_keys,
                                    pdb_violations: selection.pdb_violations,
                                },
                            )
                        })
                        .collect();
                }
                Ok(None) => {}
                Err(error) if extender.config.ignorable => {
                    tracing::warn!(extender = %extender.config.url_prefix, %error, "ignorable extender preemption call failed; preserving candidates");
                }
                Err(error) => return Err(error),
            }
        }

        let Some(best) = pick_one_node(&found) else { return Ok(None) };
        let Some(node) = snapshot.node(&best.node) else { return Ok(None) };
        Ok(Some(PreemptionOutcome {
            nominated_node: best.node.clone(),
            victims: node
                .pods
                .iter()
                .filter(|p| best.victims.pods.contains(&p.key()))
                .map(|p| p.key())
                .collect(),
        }))
    }

    /// Would the pod fit on this node with `removed` hypothetically gone?
    ///
    /// The removals are applied to `CycleState` through each plugin's
    /// `PreFilterExtensions`, the filters are run, and then the removals are
    /// **undone**. That undo is what makes the state reusable across trials,
    /// and it is only correct because every extension's add/remove pair is
    /// symmetric — each is a `+1`/`-1` on the same counter. A plugin whose
    /// pair was not symmetric would corrupt every subsequent trial rather
    /// than only its own, which is why they are tested in pairs.
    fn fits_without(
        &self,
        registry: &Registry,
        state: &mut CycleState,
        pod: &PodInfo,
        node: &NodeInfo,
        removed: &[&PodInfo],
    ) -> Result<bool, Status> {
        for plugin in &registry.pre_filter {
            if let Some(ext) = plugin.extensions() {
                for victim in removed {
                    ext.remove_pod(state, pod, victim, node);
                }
            }
        }

        let mut fits = true;
        let mut error = None;
        for plugin in &registry.filter {
            if state.filter_skipped(plugin.name()) {
                continue;
            }
            let status = plugin.filter(state, pod, node);
            if status.code == Code::Error {
                error = Some(status);
                fits = false;
                break;
            }
            if !status.is_success() && !status.is_skip() {
                fits = false;
                break;
            }
        }

        for plugin in &registry.pre_filter {
            if let Some(ext) = plugin.extensions() {
                for victim in removed {
                    ext.add_pod(state, pod, victim, node);
                }
            }
        }

        match error {
            Some(status) => Err(status),
            None => Ok(fits),
        }
    }
}
