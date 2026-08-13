//! `NodeResourcesBalancedAllocation` — prefer nodes whose resources are
//! consumed *evenly*. Score weight **1**.
//!
//! # What this fixes that NodeResourcesFit does not
//!
//! `NodeResourcesFit` asks "how full is this node". It is indifferent to a
//! node that is 90% CPU and 10% memory versus one that is 50% of each — both
//! are "50% full" on average. But the first is nearly useless: it can accept
//! only memory-heavy pods, and its remaining memory is stranded behind CPU it
//! does not have.
//!
//! So this scores the *spread* between resource utilisations and prefers the
//! balanced node. The two plugins are both weight 1 and pull in different
//! directions on purpose — one wants an empty node, the other wants an evenly
//! used one.
//!
//! # The two-resource case is not the general formula
//!
//! With exactly two resources upstream uses `|f0 − f1| / 2`, not the standard
//! deviation. They differ: for `f0=1.0, f1=0.0`, the mean-based deviation is
//! 0.5 while `|1−0|/2` is also 0.5 — but for three or more the formulas
//! genuinely diverge, and the two-resource shortcut is what the default
//! `{cpu, memory}` configuration actually runs. Implementing only the general
//! formula changes every score on a default cluster.

use crate::cache::{NodeInfo, PodInfo};
use crate::framework::status::Status;
use crate::framework::{CycleState, Plugin, PreScorePlugin, ScorePlugin, MAX_NODE_SCORE};

pub const NAME: &str = "NodeResourcesBalancedAllocation";

pub struct NodeResourcesBalancedAllocation {
    pub resources: Vec<String>,
}

impl Default for NodeResourcesBalancedAllocation {
    fn default() -> Self {
        Self { resources: vec!["cpu".to_string(), "memory".to_string()] }
    }
}

impl Plugin for NodeResourcesBalancedAllocation {
    fn name(&self) -> &'static str {
        NAME
    }
    // A pure scorer: it never rejects, so no cluster event can un-stick a pod
    // on its account and it correctly subscribes to nothing.
}

impl PreScorePlugin for NodeResourcesBalancedAllocation {
    fn pre_score(&self, _state: &mut CycleState, _pod: &PodInfo, _nodes: &[&NodeInfo]) -> Status {
        Status::success()
    }
}

/// `(1 - spread) * 100`, where spread is measured as upstream measures it.
fn balance_score(fractions: &[f64]) -> i64 {
    if fractions.len() < 2 {
        // Nothing to balance against. Upstream scores this 0 rather than
        // full marks: with one resource the plugin has no opinion, and
        // awarding 100 would silently add a constant to every node.
        return 0;
    }

    let spread = if fractions.len() == 2 {
        (fractions[0] - fractions[1]).abs() / 2.0
    } else {
        let mean = fractions.iter().sum::<f64>() / fractions.len() as f64;
        let variance =
            fractions.iter().map(|f| (f - mean).powi(2)).sum::<f64>() / fractions.len() as f64;
        variance.sqrt()
    };

    (((1.0 - spread) * MAX_NODE_SCORE as f64).round() as i64).clamp(0, MAX_NODE_SCORE)
}

impl ScorePlugin for NodeResourcesBalancedAllocation {
    fn score(&self, _state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Result<i64, Status> {
        let requests = pod.non_zero_requests();

        let mut fractions = Vec::with_capacity(self.resources.len());
        for name in &self.resources {
            let allocatable = node.allocatable.get(name);
            if allocatable <= 0 {
                // A resource the node does not have cannot be part of its
                // balance. Treating it as 0% would make every node look
                // wildly imbalanced for a resource nobody uses.
                continue;
            }
            let used = node.non_zero_requested.get(name) + requests.get(name);
            fractions.push((used as f64 / allocatable as f64).min(1.0));
        }

        Ok(balance_score(&fractions))
    }

    // No normalize: the formula already lands in 0..=100 absolutely, and
    // rescaling it per cycle would make a node's balance depend on which
    // other nodes happen to exist.

    fn weight(&self) -> i64 {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::plugins::testutil::{node, pod, res};

    /// A node's memory has to be a *realistic* number of bytes in these
    /// tests, not a token like 1000. Scoring substitutes 200Mi for a pod that
    /// requests no memory, so against a node advertising 1000 bytes every
    /// utilisation clamps to 1.0 and the memory axis swamps the comparison —
    /// which is exactly how the first version of these tests managed to fail
    /// against a correct implementation.
    const GIB: i64 = 1024 * 1024 * 1024;

    fn node_used(alloc_cpu: i64, alloc_mem: i64, used_cpu: i64, used_mem: i64) -> NodeInfo {
        let mut n = node("n");
        n.allocatable = res(alloc_cpu, alloc_mem);
        n.non_zero_requested = res(used_cpu, used_mem);
        n
    }

    #[test]
    fn a_perfectly_balanced_node_scores_the_maximum() {
        assert_eq!(balance_score(&[0.5, 0.5]), 100);
        assert_eq!(balance_score(&[0.0, 0.0]), 100);
    }

    #[test]
    fn a_maximally_skewed_node_scores_half() {
        // |1.0 - 0.0| / 2 = 0.5 spread, so (1 - 0.5) * 100.
        assert_eq!(balance_score(&[1.0, 0.0]), 50);
    }

    #[test]
    fn the_two_resource_case_uses_half_the_difference() {
        // Not the standard deviation. The default {cpu, memory} config runs
        // exactly this path, so getting it wrong changes every score on a
        // default cluster.
        assert_eq!(balance_score(&[0.8, 0.4]), 80); // |0.8-0.4|/2 = 0.2
        assert_eq!(balance_score(&[0.9, 0.1]), 60); // |0.9-0.1|/2 = 0.4
    }

    #[test]
    fn three_or_more_resources_use_the_standard_deviation() {
        // Equal fractions have zero deviation whatever their count.
        assert_eq!(balance_score(&[0.5, 0.5, 0.5]), 100);
        // Genuinely spread values score lower.
        assert!(balance_score(&[1.0, 0.5, 0.0]) < 100);
    }

    #[test]
    fn a_single_resource_has_no_opinion() {
        // Awarding full marks would add a constant to every node and silently
        // dilute every other plugin.
        assert_eq!(balance_score(&[0.5]), 0);
        assert_eq!(balance_score(&[]), 0);
    }

    #[test]
    fn a_balanced_node_beats_a_skewed_one_at_the_same_fullness() {
        // The case NodeResourcesFit cannot see: both are "50% full".
        let plugin = NodeResourcesBalancedAllocation::default();
        let state = CycleState::default();
        let p = pod("p");

        // Both nodes are ~50% full overall; only the shape differs.
        let balanced = node_used(1000, 4 * GIB, 500, 2 * GIB);
        let skewed = node_used(1000, 4 * GIB, 900, GIB / 10);

        let b = plugin.score(&state, &p, &balanced).unwrap();
        let s = plugin.score(&state, &p, &skewed).unwrap();
        assert!(b > s, "balanced {b} should beat skewed {s}");
    }

    #[test]
    fn the_incoming_pod_is_counted_in_the_balance() {
        // Placing a CPU-heavy pod on a CPU-heavy node must score worse than
        // placing it on a memory-heavy one.
        let plugin = NodeResourcesBalancedAllocation::default();
        let state = CycleState::default();
        let mut p = pod("cpu-heavy");
        p.requests = res(400, 0);

        let cpu_loaded = node_used(1000, 4 * GIB, 400, 0);
        let mem_loaded = node_used(1000, 4 * GIB, 0, 2 * GIB);

        let onto_cpu = plugin.score(&state, &p, &cpu_loaded).unwrap();
        let onto_mem = plugin.score(&state, &p, &mem_loaded).unwrap();
        assert!(onto_mem > onto_cpu);
    }

    #[test]
    fn a_resource_the_node_lacks_is_left_out_of_the_balance() {
        // Counting it as 0% would make every node look wildly imbalanced for
        // a resource nobody uses. Stated as "configuring an extra resource
        // the node does not have changes nothing", which is the property
        // that matters and does not depend on the exact score.
        let n = node_used(1000, 4 * GIB, 500, 2 * GIB);
        let state = CycleState::default();

        let without_gpu = NodeResourcesBalancedAllocation::default()
            .score(&state, &pod("p"), &n)
            .unwrap();
        let with_gpu = NodeResourcesBalancedAllocation {
            resources: vec![
                "cpu".to_string(),
                "memory".to_string(),
                "nvidia.com/gpu".to_string(),
            ],
        }
        .score(&state, &pod("p"), &n)
        .unwrap();

        assert_eq!(with_gpu, without_gpu);
    }

    #[test]
    fn an_overcommitted_resource_does_not_push_the_score_out_of_range() {
        let plugin = NodeResourcesBalancedAllocation::default();
        let n = node_used(1000, 4 * GIB, 9000, 0);

        let score = plugin.score(&CycleState::default(), &pod("p"), &n).unwrap();
        assert!((0..=MAX_NODE_SCORE).contains(&score), "score {score} out of range");
    }

    #[test]
    fn it_registers_no_events_because_it_never_rejects() {
        assert!(NodeResourcesBalancedAllocation::default().events_to_register().is_empty());
    }

    #[test]
    fn the_score_weight_is_one() {
        assert_eq!(NodeResourcesBalancedAllocation::default().weight(), 1);
    }
}
