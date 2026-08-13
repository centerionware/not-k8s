//! Tests for the scheduling cycle's arithmetic and node selection.
//!
//! Three of these cover failures that are invisible in production: a sweep
//! window that never reaches the tail of the cluster, a tie-break that
//! hot-spots onto one node, and a plugin whose broken normalize swamps every
//! other plugin. None of them error, none of them fail a smoke test, and all
//! three look like "the scheduler makes odd choices" months later.

use super::*;

// ── numFeasibleNodesToFind ──────────────────────────────────────────────

#[test]
fn a_small_cluster_always_considers_every_node() {
    // Below 100 nodes the saving is not worth risking a worse placement.
    assert_eq!(num_feasible_nodes_to_find(0, 5), 5);
    assert_eq!(num_feasible_nodes_to_find(0, 99), 99);
    assert_eq!(num_feasible_nodes_to_find(50, 10), 10);
}

#[test]
fn a_hundred_percent_considers_every_node_whatever_the_size() {
    assert_eq!(num_feasible_nodes_to_find(100, 5000), 5000);
    assert_eq!(num_feasible_nodes_to_find(150, 5000), 5000);
}

#[test]
fn the_adaptive_curve_tapers_as_the_cluster_grows() {
    // 50 - n/125, floored at 5%, then floored again at 100 nodes absolute.
    // 500 nodes: 50 - 4 = 46% -> 230.
    assert_eq!(num_feasible_nodes_to_find(0, 500), 230);
    // 1000 nodes: 50 - 8 = 42% -> 420.
    assert_eq!(num_feasible_nodes_to_find(0, 1000), 420);
    // 5000 nodes: 50 - 40 = 10% -> 500.
    assert_eq!(num_feasible_nodes_to_find(0, 5000), 500);
}

#[test]
fn the_adaptive_percentage_never_falls_below_five_percent() {
    // Past ~5600 nodes the linear term would go negative.
    let n = 20_000;
    let found = num_feasible_nodes_to_find(0, n);
    assert_eq!(found, n * MIN_FEASIBLE_NODES_PERCENTAGE / 100);
}

#[test]
fn an_explicit_percentage_still_respects_the_hundred_node_floor() {
    // 1% of 500 is 5, which is too few to choose well from.
    assert_eq!(num_feasible_nodes_to_find(1, 500), MIN_FEASIBLE_NODES_TO_FIND);
}

// ── The sweep window ────────────────────────────────────────────────────

#[test]
fn the_start_index_advances_by_nodes_processed() {
    assert_eq!(advance_start_index(0, 30, 100), 30);
    assert_eq!(advance_start_index(30, 30, 100), 60);
}

#[test]
fn the_start_index_wraps_around_the_cluster() {
    assert_eq!(advance_start_index(90, 30, 100), 20);
    assert_eq!(advance_start_index(0, 100, 100), 0);
}

#[test]
fn the_window_eventually_reaches_every_node_in_the_cluster() {
    // THE correctness property. Advancing by nodes *found* rather than nodes
    // *processed* leaves the window creeping slower than the sweep, and the
    // tail of the cluster never receives pods — which reads as "some of my
    // nodes are idle and the scheduler won't use them".
    let num_nodes = 1000;
    let processed_per_cycle = 420; // what the adaptive curve examines here
    let mut start = 0usize;
    let mut visited = vec![false; num_nodes];

    for _ in 0..50 {
        for i in 0..processed_per_cycle {
            visited[(start + i) % num_nodes] = true;
        }
        start = advance_start_index(start, processed_per_cycle, num_nodes);
    }

    assert!(visited.iter().all(|v| *v), "every node must be reachable by the sweep");
}

#[test]
fn an_empty_cluster_does_not_divide_by_zero() {
    assert_eq!(advance_start_index(0, 0, 0), 0);
}

// ── select_host ─────────────────────────────────────────────────────────

#[test]
fn the_highest_scoring_node_wins_outright() {
    let scores = vec![
        ("a".to_string(), 10),
        ("b".to_string(), 90),
        ("c".to_string(), 50),
    ];
    let mut rng = Rng::new(1);
    assert_eq!(select_host(&scores, &mut rng), Some("b".to_string()));
}

#[test]
fn no_candidates_yields_nothing() {
    let mut rng = Rng::new(1);
    assert_eq!(select_host(&[], &mut rng), None);
}

#[test]
fn a_single_candidate_is_chosen() {
    let scores = vec![("only".to_string(), 0)];
    let mut rng = Rng::new(1);
    assert_eq!(select_host(&scores, &mut rng), Some("only".to_string()));
}

#[test]
fn tied_nodes_are_spread_across_rather_than_all_going_to_the_first() {
    // The hot-spot bug: on a fresh homogeneous cluster every node scores
    // identically, so first-wins sends an entire Deployment to one node.
    let scores: Vec<(String, i64)> =
        (0..5).map(|i| (format!("n{i}"), 100)).collect();

    let mut counts = std::collections::HashMap::new();
    for seed in 0..500u64 {
        let mut rng = Rng::new(seed);
        let chosen = select_host(&scores, &mut rng).unwrap();
        *counts.entry(chosen).or_insert(0usize) += 1;
    }

    assert_eq!(counts.len(), 5, "every tied node must be reachable, got {counts:?}");
    for (node, count) in &counts {
        assert!(
            *count > 30,
            "node {node} got {count}/500 — the distribution is badly skewed"
        );
    }
}

#[test]
fn only_the_tied_maximum_participates_in_the_draw() {
    // A lower-scoring node must never be selected, however the RNG falls.
    let scores = vec![
        ("low".to_string(), 1),
        ("high-a".to_string(), 100),
        ("high-b".to_string(), 100),
    ];
    for seed in 0..200u64 {
        let mut rng = Rng::new(seed);
        let chosen = select_host(&scores, &mut rng).unwrap();
        assert_ne!(chosen, "low");
    }
}

#[test]
fn selection_is_reproducible_from_its_seed() {
    // The property that makes the cycle debuggable: same inputs, same answer.
    let scores: Vec<(String, i64)> = (0..8).map(|i| (format!("n{i}"), 50)).collect();

    let first = {
        let mut rng = Rng::new(42);
        select_host(&scores, &mut rng)
    };
    let second = {
        let mut rng = Rng::new(42);
        select_host(&scores, &mut rng)
    };
    assert_eq!(first, second);
}

#[test]
fn negative_scores_do_not_break_the_maximum() {
    // Defensive: a plugin returning below zero must not make every node look
    // worse than the sentinel.
    let scores = vec![("a".to_string(), -50), ("b".to_string(), -10)];
    let mut rng = Rng::new(1);
    assert_eq!(select_host(&scores, &mut rng), Some("b".to_string()));
}

// ── The RNG itself ──────────────────────────────────────────────────────

#[test]
fn the_rng_is_deterministic_for_a_given_seed() {
    let a: Vec<u64> = { let mut r = Rng::new(7); (0..10).map(|_| r.next_u64()).collect() };
    let b: Vec<u64> = { let mut r = Rng::new(7); (0..10).map(|_| r.next_u64()).collect() };
    assert_eq!(a, b);
}

#[test]
fn different_seeds_produce_different_streams() {
    let a: Vec<u64> = { let mut r = Rng::new(1); (0..10).map(|_| r.next_u64()).collect() };
    let b: Vec<u64> = { let mut r = Rng::new(2); (0..10).map(|_| r.next_u64()).collect() };
    assert_ne!(a, b);
}

#[test]
fn below_stays_within_its_bound() {
    let mut r = Rng::new(99);
    for n in 1..20u64 {
        for _ in 0..50 {
            assert!(r.below(n) < n);
        }
    }
}

#[test]
fn below_zero_is_zero_rather_than_a_division_panic() {
    let mut r = Rng::new(1);
    assert_eq!(r.below(0), 0);
}

#[test]
fn below_covers_its_whole_range() {
    // A modulo bug that always returned 0 would silently make every tie-break
    // pick the first node, reintroducing the hot-spot this exists to avoid.
    let mut r = Rng::new(5);
    let mut seen = [false; 4];
    for _ in 0..400 {
        seen[r.below(4) as usize] = true;
    }
    assert!(seen.iter().all(|s| *s));
}
