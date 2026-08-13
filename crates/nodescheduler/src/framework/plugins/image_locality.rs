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

pub const NAME: &str = "ImageLocality";

/// Below this, an image is cheap enough to pull that locality is noise.
const MIN_THRESHOLD: i64 = 23 * 1024 * 1024;
/// Per-container cap, so one huge image cannot dominate the whole score.
const MAX_CONTAINER_THRESHOLD: i64 = 1000 * 1024 * 1024;

/// Total node count, captured once per cycle for the spread ratio.
struct TotalNodes(i64);

pub struct ImageLocality;

impl Plugin for ImageLocality {
    fn name(&self) -> &'static str {
        NAME
    }
    // A pure scorer: it never rejects, so nothing can be stranded by it.
}

impl PreScorePlugin for ImageLocality {
    fn pre_score(&self, state: &mut CycleState, _pod: &PodInfo, nodes: &[&NodeInfo]) -> Status {
        state.write(NAME, TotalNodes(nodes.len() as i64));
        Status::success()
    }
}

impl ScorePlugin for ImageLocality {
    fn score(&self, state: &CycleState, pod: &PodInfo, node: &NodeInfo) -> Result<i64, Status> {
        let total_nodes = state.read::<TotalNodes>(NAME).map(|t| t.0).unwrap_or(1).max(1);

        let mut sum = 0i64;
        for image in &pod.images {
            let Some(present) = node.images.get(image) else {
                continue;
            };
            // num_nodes is how widely cached the image is; see the header for
            // why a rare image is worth *less*, not more.
            let spread = present.num_nodes.clamp(1, total_nodes);
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

    fn node_holding(name: &str, images: &[(&str, i64, i64)]) -> NodeInfo {
        let mut n = node(name);
        n.images = images
            .iter()
            .map(|(img, size, nodes)| {
                (img.to_string(), ImageState { size_bytes: *size, num_nodes: *nodes })
            })
            .collect();
        n
    }

    fn pod_using(images: &[&str]) -> PodInfo {
        let mut p = pod("p");
        p.images = images.iter().map(|s| s.to_string()).collect();
        p.container_count = images.len();
        p
    }

    fn state_with(total_nodes: usize) -> CycleState {
        let mut s = CycleState::default();
        s.write(NAME, TotalNodes(total_nodes as i64));
        s
    }

    #[test]
    fn a_node_holding_the_image_beats_one_that_does_not() {
        let p = pod_using(&["app:v1"]);
        let state = state_with(10);

        let has = node_holding("has", &[("app:v1", 500 * MB, 10)]);
        let lacks = node("lacks");

        let with = ImageLocality.score(&state, &p, &has).unwrap();
        let without = ImageLocality.score(&state, &p, &lacks).unwrap();
        assert!(with > without);
        assert_eq!(without, 0);
    }

    #[test]
    fn a_widely_cached_image_counts_for_more_than_a_rare_one() {
        // THE property of this plugin. A rare image scoring higher is the
        // node-heating bug the spread ratio exists to prevent.
        let p = pod_using(&["app:v1"]);
        let state = state_with(10);

        let common = node_holding("common", &[("app:v1", 900 * MB, 10)]);
        let rare = node_holding("rare", &[("app:v1", 900 * MB, 1)]);

        let common_score = ImageLocality.score(&state, &p, &common).unwrap();
        let rare_score = ImageLocality.score(&state, &p, &rare).unwrap();
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
        let n = node_holding("n", &[("tiny:v1", 5 * MB, 1)]);

        assert_eq!(ImageLocality.score(&state_with(1), &p, &n).unwrap(), 0);
    }

    #[test]
    fn an_enormous_image_is_capped_at_the_maximum() {
        let p = pod_using(&["huge:v1"]);
        let n = node_holding("n", &[("huge:v1", 50_000 * MB, 1)]);

        assert_eq!(ImageLocality.score(&state_with(1), &p, &n).unwrap(), MAX_NODE_SCORE);
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
        let state = state_with(1);

        let both = node_holding("both", &[("a:v1", 400 * MB, 1), ("b:v1", 400 * MB, 1)]);
        let one = node_holding("one", &[("a:v1", 400 * MB, 1)]);

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
        let n = node_holding("n", &[("app:v1", 500 * MB, 1)]);

        let score = ImageLocality.score(&CycleState::default(), &p, &n).unwrap();
        assert!((0..=MAX_NODE_SCORE).contains(&score));
    }

    #[test]
    fn a_node_claiming_more_copies_than_the_cluster_has_is_clamped() {
        // Stale image state must not produce a score above the maximum.
        let p = pod_using(&["app:v1"]);
        let n = node_holding("n", &[("app:v1", 900 * MB, 99)]);

        let score = ImageLocality.score(&state_with(3), &p, &n).unwrap();
        assert!((0..=MAX_NODE_SCORE).contains(&score), "score {score} out of range");
    }

    #[test]
    fn prescore_records_the_cluster_size_the_ratio_needs() {
        let mut state = CycleState::default();
        let a = node("a");
        let b = node("b");
        ImageLocality.pre_score(&mut state, &pod("p"), &[&a, &b]);

        assert_eq!(state.read::<TotalNodes>(NAME).map(|t| t.0), Some(2));
    }

    #[test]
    fn it_registers_no_events_because_it_never_rejects() {
        assert!(ImageLocality.events_to_register().is_empty());
    }
}
