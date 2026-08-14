//! `ImageLocality` — prefer nodes that already have the pod's images. Score
//! weight **1**.
//!
//! # The spread ratio, which looks like a bug and is not
//!
//! An image's contribution is scaled by the fraction of nodes that already
//! hold it:
//!
//! ```text
//! scaled = size_bytes * nodes_with_image / total_nodes
//! ```
//!
//! Read quickly, that is backwards — a rare image saves the *most* download
//! time, so surely it should count for more. It is deliberate, and upstream
//! calls the failure it prevents the "node heating" problem: if a large image
//! exists on exactly one node, an undamped score sends every pod using it to
//! that node, which then becomes a hotspot while the rest of the cluster sits
//! idle. The pods pile up faster than the image propagates, so the problem is
//! self-sustaining.
//!
//! Damping by the spread ratio means a widely-cached image is a genuine
//! tiebreak while a rare one barely registers — the cluster spreads out and
//! the image propagates, which fixes the situation instead of entrenching it.
//!
//! # The thresholds
//!
//! Images below `MIN_THRESHOLD` (23 MB) score nothing: pulling them is fast
//! enough that placement should be decided on other grounds. Images above
//! `MAX_CONTAINER_THRESHOLD` (1000 MB) are capped, so one enormous image
//! cannot swamp every other consideration.

use crate::cache::{NodeInfo, PodInfo};
use crate::framework::status::Status;
use crate::framework::{CycleState, Plugin, PreScorePlugin, ScorePlugin, MAX_NODE_SCORE};
use std::collections::HashMap;

pub const NAME: &str = "ImageLocality";

/// Below this, an image is cheap enough to pull that locality is noise.
const MIN_THRESHOLD: i64 = 23 * 1024 * 1024;
/// Per-container cap, so one huge image cannot dominate the whole score.
const MAX_CONTAINER_THRESHOLD: i64 = 1000 * 1024 * 1024;

/// Captured once per cycle for the spread ratio. `NodeInfo::images`'
/// `ImageState::num_nodes` is always `1` (a per-node projection has no way
/// to know the cluster-wide count on its own) — `image_node_counts` is the
/// real cluster-wide count, computed here from the actual feasible node set
/// `PreScore` is handed, the same one `Snapshot::nodes_with_image` would
/// answer for the whole cluster.
struct PreScoreState {
    total_nodes: i64,
    image_node_counts: HashMap<String, i64>,
}

pub struct ImageLocality;

impl Plugin for ImageLocality {
    fn name(&self) -> &'static str {
        NAME
    }
    // A pure scorer: it never rejects, so nothing can be stranded by it.
}

impl PreScorePlugin for ImageLocality {
    fn pre_score(&self, state: &mut CycleState, _pod: &PodInfo, nodes: &[&NodeInfo]) -> Status {
        let mut image_node_counts: HashMap<String, i64> = HashMap::new();
        for node in nodes {
            for image in node.images.keys() {
                *image_node_counts.entry(image.clone()).or_default() += 1;
            }
        }
        state.write(NAME, PreScoreState { total_nodes: nodes.len() as i64, image_node_counts });
        Status::success()
    }
}

impl ScorePlugin for ImageLocality {
    fn score(&self, state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Result<i64, Status> {
        let pre = state.read::<PreScoreState>(NAME);
        let total_nodes = pre.map(|p| p.total_nodes).unwrap_or(1).max(1);

        let mut sum = 0i64;
        for image in &pod.images {
            let Some(present) = node.images.get(image) else {
                continue;
            };
            // How widely cached the image is, cluster-wide — see the header
            // for why a rare image is worth *less*, not more.
            let spread = pre
                .and_then(|p| p.image_node_counts.get(image))
                .copied()
                .unwrap_or(1)
                .clamp(1, total_nodes);
            sum += present.size_bytes * spread / total_nodes;
        }

        Ok(scaled_image_score(sum, pod.container_count))
    }

    fn weight(&self) -> i64 {
        1
    }
}

/// Map total cached bytes onto `[0, 100]`.
fn scaled_image_score(sum_bytes: i64, container_count: usize) -> i64 {
    let containers = container_count.max(1) as i64;
    let max = MAX_CONTAINER_THRESHOLD * containers;

    if sum_bytes < MIN_THRESHOLD {
        return 0;
    }
    if sum_bytes >= max {
        return MAX_NODE_SCORE;
    }
    // Linear between the two thresholds. The subtraction keeps a pod whose
    // images only just clear MIN_THRESHOLD near zero rather than jumping to a
    // meaningful score the moment it crosses.
    MAX_NODE_SCORE * (sum_bytes - MIN_THRESHOLD) / (max - MIN_THRESHOLD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::ImageState;
    use crate::framework::plugins::testutil::{node, pod};

    const MB: i64 = 1024 * 1024;

    fn node_holding(name: &str, images: &[(&str, i64)]) -> NodeInfo {
        let mut n = node(name);
        n.images = images
            .iter()
            .map(|(img, size)| (img.to_string(), ImageState { size_bytes: *size, num_nodes: 1 }))
            .collect();
        n
    }

    fn pod_using(images: &[&str]) -> PodInfo {
        let mut p = pod("p");
        p.images = images.iter().map(|s| s.to_string()).collect();
        p.container_count = images.len();
        p
    }

    /// Runs the real `PreScore` over `nodes` — the cluster-wide count each
    /// test wants (`image_node_counts`) comes from how many of `nodes`
    /// actually hold the image, not a hand-set fixture field.
    fn state_after_prescore(nodes: &[&NodeInfo]) -> CycleState {
        let mut s = CycleState::default();
        ImageLocality.pre_score(&mut s, &pod("p"), nodes);
        s
    }

    #[test]
    fn a_node_holding_the_image_beats_one_that_does_not() {
        let p = pod_using(&["app:v1"]);
        let has = node_holding("has", &[("app:v1", 500 * MB)]);
        let lacks = node("lacks");
        let state = state_after_prescore(&[&has, &lacks]);

        let with = ImageLocality.score(&state, &p, &has).unwrap();
        let without = ImageLocality.score(&state, &p, &lacks).unwrap();
        assert!(with > without);
        assert_eq!(without, 0);
    }

    #[test]
    fn a_widely_cached_image_counts_for_more_than_a_rare_one() {
        // THE property of this plugin. A rare image scoring higher is the
        // node-heating bug the spread ratio exists to prevent. Two separate
        // ten-node clusters: "app:v1" is on all ten in one, on just one node
        // in the other.
        let p = pod_using(&["app:v1"]);

        let common = node_holding("common", &[("app:v1", 900 * MB)]);
        let common_others: Vec<NodeInfo> = (1..10).map(|i| node_holding(&format!("c{i}"), &[("app:v1", 900 * MB)])).collect();
        let mut common_cluster: Vec<&NodeInfo> = vec![&common];
        common_cluster.extend(common_others.iter());
        let common_state = state_after_prescore(&common_cluster);

        let rare = node_holding("rare", &[("app:v1", 900 * MB)]);
        let rare_others: Vec<NodeInfo> = (1..10).map(|i| node(&format!("r{i}"))).collect();
        let mut rare_cluster: Vec<&NodeInfo> = vec![&rare];
        rare_cluster.extend(rare_others.iter());
        let rare_state = state_after_prescore(&rare_cluster);

        let common_score = ImageLocality.score(&common_state, &p, &common).unwrap();
        let rare_score = ImageLocality.score(&rare_state, &p, &rare).unwrap();
        assert!(
            common_score > rare_score,
            "a widely cached image ({common_score}) must beat a rare one ({rare_score})"
        );
    }

    #[test]
    fn a_small_image_scores_nothing() {
        // Below 23MB the pull is fast enough that placement should be decided
        // on other grounds.
        let p = pod_using(&["tiny:v1"]);
        let n = node_holding("n", &[("tiny:v1", 5 * MB)]);
        let state = state_after_prescore(&[&n]);

        assert_eq!(ImageLocality.score(&state, &p, &n).unwrap(), 0);
    }

    #[test]
    fn an_enormous_image_is_capped_at_the_maximum() {
        let p = pod_using(&["huge:v1"]);
        let n = node_holding("n", &[("huge:v1", 50_000 * MB)]);
        let state = state_after_prescore(&[&n]);

        assert_eq!(ImageLocality.score(&state, &p, &n).unwrap(), MAX_NODE_SCORE);
    }

    #[test]
    fn the_cap_scales_with_container_count() {
        // A four-container pod has four images' worth of headroom before it
        // saturates, so one big image must not max it out.
        let one = scaled_image_score(1000 * MB, 1);
        let four = scaled_image_score(1000 * MB, 4);
        assert_eq!(one, MAX_NODE_SCORE);
        assert!(four < MAX_NODE_SCORE);
    }

    #[test]
    fn several_cached_images_accumulate() {
        let p = pod_using(&["a:v1", "b:v1"]);
        let both = node_holding("both", &[("a:v1", 400 * MB), ("b:v1", 400 * MB)]);
        let one = node_holding("one", &[("a:v1", 400 * MB)]);
        let state = state_after_prescore(&[&both, &one]);

        assert!(
            ImageLocality.score(&state, &p, &both).unwrap()
                > ImageLocality.score(&state, &p, &one).unwrap()
        );
    }

    #[test]
    fn scoring_without_prescore_state_does_not_divide_by_zero() {
        // Defensive: a total of zero nodes is impossible in a real cycle, but
        // a panic in a scorer takes the whole scheduler down.
        let p = pod_using(&["app:v1"]);
        let n = node_holding("n", &[("app:v1", 500 * MB)]);

        let score = ImageLocality.score(&CycleState::default(), &p, &n).unwrap();
        assert!((0..=MAX_NODE_SCORE).contains(&score));
    }

    #[test]
    fn a_count_higher_than_the_cluster_size_is_clamped() {
        // Defensive: an inconsistent PreScoreState (more nodes-with-image
        // than nodes total) must not produce a score above the maximum.
        let p = pod_using(&["app:v1"]);
        let n = node_holding("n", &[("app:v1", 900 * MB)]);
        let mut state = CycleState::default();
        state.write(
            NAME,
            PreScoreState { total_nodes: 3, image_node_counts: HashMap::from([("app:v1".to_string(), 99)]) },
        );

        let score = ImageLocality.score(&state, &p, &n).unwrap();
        assert!((0..=MAX_NODE_SCORE).contains(&score), "score {score} out of range");
    }

    #[test]
    fn prescore_records_the_cluster_size_the_ratio_needs() {
        let mut state = CycleState::default();
        let a = node("a");
        let b = node("b");
        ImageLocality.pre_score(&mut state, &pod("p"), &[&a, &b]);

        assert_eq!(state.read::<PreScoreState>(NAME).map(|s| s.total_nodes), Some(2));
    }

    #[test]
    fn prescore_counts_how_many_nodes_actually_hold_each_image() {
        let a = node_holding("a", &[("app:v1", 500 * MB)]);
        let b = node_holding("b", &[("app:v1", 500 * MB)]);
        let c = node("c");
        let mut state = CycleState::default();
        ImageLocality.pre_score(&mut state, &pod("p"), &[&a, &b, &c]);

        assert_eq!(
            state.read::<PreScoreState>(NAME).and_then(|s| s.image_node_counts.get("app:v1")).copied(),
            Some(2),
            "two of the three nodes actually hold the image"
        );
    }

    #[test]
    fn it_registers_no_events_because_it_never_rejects() {
        assert!(ImageLocality.events_to_register().is_empty());
    }
}
