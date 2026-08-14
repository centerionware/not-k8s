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

// ── The whole cycle, against a real filter chain ────────────────────────
//
// Everything above tests the cycle's arithmetic in isolation, and every
// plugin tests its own predicate in isolation. Both passed while the first
// live e2e run bound a pod requesting 10000 CPU to a 4-CPU node — which is
// the gap those two kinds of test leave between them: nothing exercised
// "run the real PreFilter, then the real Filter, and see what the cycle
// concludes".
//
// These do. They are the smallest thing that would have caught it.

use crate::cache::{Cache, PodInfo, Resources};
use crate::framework::plugins::node_resources_fit::NodeResourcesFit;
use crate::framework::Registry;
use k8s_openapi::api::core::v1::{Node, NodeStatus, Pod};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::collections::BTreeMap;

/// One node, `cpu` cores and 4Gi, with the real NodeResourcesFit wired into
/// both PreFilter and Filter exactly as `default_registry` does.
fn fit_scheduler(cpu_cores: &str) -> (Scheduler, Registry, crate::cache::Snapshot) {
    let registry = Registry {
        profile_name: "test".to_string(),
        pre_filter: vec![Box::new(NodeResourcesFit::default())],
        filter: vec![Box::new(NodeResourcesFit::default())],
        ..Default::default()
    };

    let mut cache = Cache::new();
    cache.upsert_node(&Node {
        metadata: ObjectMeta { name: Some("worker".to_string()), ..Default::default() },
        status: Some(NodeStatus {
            allocatable: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity(cpu_cores.to_string())),
                ("memory".to_string(), Quantity("4Gi".to_string())),
                ("pods".to_string(), Quantity("110".to_string())),
            ])),
            ..Default::default()
        }),
        ..Default::default()
    });
    let snapshot = cache.snapshot();
    (Scheduler::new(0), registry, snapshot)
}

fn pod_wanting_milli_cpu(milli: i64) -> PodInfo {
    PodInfo {
        namespace: "default".to_string(),
        name: "p".to_string(),
        uid: "p".to_string(),
        requests: Resources { milli_cpu: milli, ..Default::default() },
        ..Default::default()
    }
}

#[tokio::test]
async fn a_pod_larger_than_the_node_is_not_scheduled() {
    // The exact e2e case: 10000 cores against a 4-core node.
    let (mut sched, registry, snapshot) = fit_scheduler("4");
    let pod = pod_wanting_milli_cpu(10_000 * 1000);
    let mut rng = Rng::new(1);

    let (outcome, _) = sched.schedule_one(&registry, &[], &pod, &snapshot, &mut rng).await;

    match outcome {
        CycleOutcome::Unschedulable { reason, unschedulable_plugins, .. } => {
            assert!(
                reason.contains("Insufficient cpu"),
                "the reason must name the resource that did not fit, got: {reason}"
            );
            assert!(
                unschedulable_plugins.contains(&"NodeResourcesFit"),
                "NodeResourcesFit must be recorded, or no event can ever requeue this pod"
            );
        }
        CycleOutcome::Scheduled { node } => {
            panic!("a 10000-core pod was scheduled onto a 4-core node ({node})")
        }
        CycleOutcome::Error { reason } => panic!("unexpected error: {reason}"),
    }
}

#[tokio::test]
async fn a_pod_that_fits_is_scheduled() {
    // The other half: proving the rejection above is not simply "rejects
    // everything", which would pass the test above for the wrong reason.
    let (mut sched, registry, snapshot) = fit_scheduler("4");
    let pod = pod_wanting_milli_cpu(500);
    let mut rng = Rng::new(1);

    let (outcome, _) = sched.schedule_one(&registry, &[], &pod, &snapshot, &mut rng).await;

    match outcome {
        CycleOutcome::Scheduled { node } => assert_eq!(node, "worker"),
        CycleOutcome::Unschedulable { reason, .. } => {
            panic!("a 500m pod should fit a 4-core node, got: {reason}")
        }
        CycleOutcome::Error { reason } => panic!("unexpected error: {reason}"),
    }
}

#[tokio::test]
async fn a_pod_exactly_filling_the_node_still_fits() {
    let (mut sched, registry, snapshot) = fit_scheduler("4");
    let pod = pod_wanting_milli_cpu(4000);
    let mut rng = Rng::new(1);

    let (outcome, _) = sched.schedule_one(&registry, &[], &pod, &snapshot, &mut rng).await;
    assert!(matches!(outcome, CycleOutcome::Scheduled { .. }));
}

#[tokio::test]
async fn one_millicore_over_capacity_does_not_fit() {
    // The boundary, stated explicitly: > allocatable, not >=.
    let (mut sched, registry, snapshot) = fit_scheduler("4");
    let pod = pod_wanting_milli_cpu(4001);
    let mut rng = Rng::new(1);

    let (outcome, _) = sched.schedule_one(&registry, &[], &pod, &snapshot, &mut rng).await;
    assert!(
        matches!(outcome, CycleOutcome::Unschedulable { .. }),
        "4001m must not fit a 4-core node"
    );
}

#[tokio::test]
async fn an_empty_cluster_reports_no_nodes_rather_than_scheduling_nowhere() {
    let registry = Registry::default();
    let mut sched = Scheduler::new(0);
    let snapshot = crate::cache::Snapshot::default();
    let mut rng = Rng::new(1);

    let (outcome, _) = sched.schedule_one(&registry, &[], &pod_wanting_milli_cpu(1), &snapshot, &mut rng).await;
    match outcome {
        CycleOutcome::Unschedulable { reason, .. } => {
            assert!(reason.contains("no nodes"), "got: {reason}")
        }
        _ => panic!("an empty cluster must not schedule anything"),
    }
}

// ── Through the projection, not around it ───────────────────────────────
//
// The tests above build `PodInfo` by hand, which is how they passed while
// the live cluster bound a 10000-core pod to a 4-core node: the real path
// goes through `PodInfo::from_pod` -> `pod_requests` -> `parse_quantity_*`
// first, and hand-built fixtures skip every one of those.
//
// So these start from an actual `Pod` object, exactly as the watch layer
// receives it, and run the whole way to a cycle outcome. Any future test of
// a placement decision should start here rather than at `PodInfo`.

fn api_pod_requesting(cpu: &str) -> Pod {
    Pod {
        metadata: ObjectMeta {
            name: Some("toobig".to_string()),
            namespace: Some("default".to_string()),
            uid: Some("toobig-uid".to_string()),
            ..Default::default()
        },
        spec: Some(k8s_openapi::api::core::v1::PodSpec {
            containers: vec![k8s_openapi::api::core::v1::Container {
                name: "c".to_string(),
                resources: Some(k8s_openapi::api::core::v1::ResourceRequirements {
                    requests: Some(BTreeMap::from([(
                        "cpu".to_string(),
                        Quantity(cpu.to_string()),
                    )])),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn a_real_pod_object_asking_for_10000_cores_is_projected_as_10000_cores() {
    // The step the hand-built fixtures skipped. "10000" with no suffix means
    // 10000 whole cores, i.e. ten million millicores — if this ever came back
    // as 10000 millicores it would look like a 10-core request and fit
    // almost anywhere.
    let info = PodInfo::from_pod(&api_pod_requesting("10000"), Default::default());
    assert_eq!(info.requests.milli_cpu, 10_000_000);
    assert_eq!(
        info.requests.names(),
        vec!["cpu".to_string()],
        "cpu must appear in the requested set, or the fit check skips it entirely"
    );
}

#[tokio::test]
async fn a_real_pod_object_larger_than_the_node_is_not_scheduled() {
    // The live e2e case, end to end: API object -> projection -> cycle.
    let (mut sched, registry, snapshot) = fit_scheduler("4");
    let pod = PodInfo::from_pod(&api_pod_requesting("10000"), Default::default());
    let mut rng = Rng::new(1);

    let (outcome, _) = sched.schedule_one(&registry, &[], &pod, &snapshot, &mut rng).await;

    match outcome {
        CycleOutcome::Unschedulable { reason, .. } => {
            assert!(reason.contains("Insufficient cpu"), "got: {reason}")
        }
        CycleOutcome::Scheduled { node } => panic!(
            "a real Pod object requesting 10000 cores was scheduled onto a 4-core node ({node})"
        ),
        CycleOutcome::Error { reason } => panic!("unexpected error: {reason}"),
    }
}

#[tokio::test]
async fn a_real_pod_object_that_fits_is_scheduled() {
    let (mut sched, registry, snapshot) = fit_scheduler("4");
    let pod = PodInfo::from_pod(&api_pod_requesting("500m"), Default::default());
    let mut rng = Rng::new(1);

    let (outcome, _) = sched.schedule_one(&registry, &[], &pod, &snapshot, &mut rng).await;
    assert!(
        matches!(outcome, CycleOutcome::Scheduled { .. }),
        "500m must fit a 4-core node"
    );
}

#[test]
fn a_node_object_advertising_four_cores_is_projected_as_4000_millicores() {
    // The other half of the same arithmetic. If allocatable were read in
    // whole cores while requests were in millicores, every node would look
    // 1000x smaller than it is and nothing would ever schedule.
    let (_, _, snapshot) = fit_scheduler("4");
    let node = snapshot.node("worker").expect("the node is in the snapshot");
    assert_eq!(node.allocatable.milli_cpu, 4000);
    assert_eq!(node.allocatable_pods, 110);
}
