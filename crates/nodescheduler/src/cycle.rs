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
use std::sync::Arc;

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
        self.next_u64() % n
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
        /// Set by a PostFilter plugin that freed capacity — the pod is
        /// promised this node once its victims are gone.
        nominated_node: Option<String>,
    },
    /// A plugin itself failed. Retried; never treated as a placement decision.
    Error { reason: String },
}

/// Everything a cycle needs that is not the pod.
pub struct Scheduler {
    pub registry: Registry,
    pub percentage_of_nodes_to_score: i32,
    /// Rotates across cycles; see [`advance_start_index`].
    pub next_start_node_index: usize,
}

impl Scheduler {
    pub fn new(registry: Registry, percentage_of_nodes_to_score: i32) -> Self {
        Self { registry, percentage_of_nodes_to_score, next_start_node_index: 0 }
    }

    /// Run one full scheduling cycle for one pod.
    pub fn schedule_one(
        &mut self,
        pod: &PodInfo,
        snapshot: &Snapshot,
        rng: &mut Rng,
    ) -> CycleOutcome {
        if snapshot.is_empty() {
            return CycleOutcome::Unschedulable {
                reason: "no nodes available to schedule pods".to_string(),
                unschedulable_plugins: Vec::new(),
                pending_plugins: Vec::new(),
                nominated_node: None,
            };
        }

        let mut state = CycleState::default();

        // ── PreFilter ───────────────────────────────────────────────────
        let mut restricted: Option<Vec<String>> = None;
        for plugin in &self.registry.pre_filter {
            let (status, nodes) = plugin.pre_filter(&mut state, pod, snapshot);
            match status.code {
                Code::Success | Code::Skip => {}
                Code::Error => {
                    return CycleOutcome::Error { reason: status.to_string() };
                }
                _ => {
                    // A whole-cluster rejection: no point looking at nodes.
                    return CycleOutcome::Unschedulable {
                        reason: status.to_string(),
                        unschedulable_plugins: if status.code == Code::Pending {
                            Vec::new()
                        } else {
                            vec![status.plugin]
                        },
                        pending_plugins: if status.code == Code::Pending {
                            vec![status.plugin]
                        } else {
                            Vec::new()
                        },
                        nominated_node: None,
                    };
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

        // ── Filter ──────────────────────────────────────────────────────
        let (feasible, node_statuses, processed) =
            self.find_feasible_nodes(&state, pod, snapshot, restricted.as_deref());

        self.next_start_node_index =
            advance_start_index(self.next_start_node_index, processed, snapshot.num_nodes());

        if feasible.is_empty() {
            // ── PostFilter (preemption) ─────────────────────────────────
            for plugin in &self.registry.post_filter {
                let (status, nominated) =
                    plugin.post_filter(&mut state, pod, snapshot, &node_statuses);
                if status.is_success() || nominated.is_some() {
                    return CycleOutcome::Unschedulable {
                        reason: status.to_string(),
                        unschedulable_plugins: node_statuses.rejecting_plugins(),
                        pending_plugins: Vec::new(),
                        nominated_node: nominated,
                    };
                }
            }
            let mut unschedulable = node_statuses.rejecting_plugins();
            let pending: Vec<&'static str> = Vec::new();
            unschedulable.retain(|p| !pending.contains(p));
            return CycleOutcome::Unschedulable {
                reason: node_statuses.summary(snapshot.num_nodes()),
                unschedulable_plugins: unschedulable,
                pending_plugins: pending,
                nominated_node: None,
            };
        }

        // A single candidate needs no scoring — and with no Score plugins at
        // all, scoring cannot distinguish anything anyway.
        if feasible.len() == 1 || self.registry.score.is_empty() {
            return CycleOutcome::Scheduled { node: feasible[0].name.clone() };
        }

        // ── PreScore / Score / Normalize ────────────────────────────────
        let refs: Vec<&NodeInfo> = feasible.iter().map(|n| n.as_ref()).collect();
        for plugin in &self.registry.pre_score {
            let status = plugin.pre_score(&mut state, pod, &refs);
            if status.code == Code::Error {
                return CycleOutcome::Error { reason: status.to_string() };
            }
        }

        let mut totals: Vec<(String, i64)> =
            feasible.iter().map(|n| (n.name.clone(), 0i64)).collect();

        for plugin in &self.registry.score {
            if state.score_skipped(plugin.name()) {
                continue;
            }
            let mut raw: Vec<i64> = Vec::with_capacity(feasible.len());
            for node in &feasible {
                match plugin.score(&state, pod, node) {
                    Ok(v) => raw.push(v),
                    Err(status) => return CycleOutcome::Error { reason: status.to_string() },
                }
            }
            let status = plugin.normalize(&state, pod, &mut raw);
            if status.code == Code::Error {
                return CycleOutcome::Error { reason: status.to_string() };
            }
            let weight = plugin.weight();
            for (total, score) in totals.iter_mut().zip(raw.iter()) {
                // Clamp before weighting: a plugin whose normalize is wrong
                // must not be able to swamp every other plugin's contribution.
                total.1 += score.clamp(0, MAX_NODE_SCORE) * weight;
            }
        }

        match select_host(&totals, rng) {
            Some(node) => CycleOutcome::Scheduled { node },
            None => CycleOutcome::Error {
                reason: "scoring produced no candidate despite feasible nodes".to_string(),
            },
        }
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
        state: &CycleState,
        pod: &PodInfo,
        snapshot: &Snapshot,
        restricted: Option<&[String]>,
    ) -> (Vec<Arc<NodeInfo>>, NodeToStatus, usize) {
        let all = snapshot.nodes();
        let num_all = all.len();
        let wanted = if self.registry.score.is_empty() {
            // With nothing to compare on, the first feasible node is as good
            // as the best one, so stop at one.
            1
        } else {
            num_feasible_nodes_to_find(self.percentage_of_nodes_to_score, num_all as i32)
                .max(1) as usize
        };

        let mut feasible = Vec::new();
        let mut statuses = NodeToStatus::default();
        let mut processed = 0usize;

        for i in 0..num_all {
            let node = &all[(self.next_start_node_index + i) % num_all];
            processed += 1;

            if let Some(allowed) = restricted {
                if !allowed.contains(&node.name) {
                    continue;
                }
            }

            let mut rejected = None;
            for plugin in &self.registry.filter {
                if state.filter_skipped(plugin.name()) {
                    continue;
                }
                let status = plugin.filter(state, pod, node);
                if !status.is_success() && !status.is_skip() {
                    rejected = Some(status);
                    // First rejection wins: the remaining filters cannot
                    // un-reject the node, and running them is pure cost on
                    // the node-count-times-plugin-count hot path.
                    break;
                }
            }

            match rejected {
                None => {
                    feasible.push(node.clone());
                    if feasible.len() >= wanted {
                        break;
                    }
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
