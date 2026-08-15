//! Tests for preemption.
//!
//! Almost every rule here decides *which* pods die rather than whether the
//! preemptor gets scheduled — so a wrong implementation still "works" and no
//! test of the outcome would notice. That is why these assert on the victim
//! set and the ordering rather than on success.

use super::*;
use crate::cache::{NodeInfo, PodInfo, Resources};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use std::collections::BTreeMap;
use std::sync::Arc;

fn at(secs: i64) -> k8s_openapi::jiff::Timestamp {
    k8s_openapi::jiff::Timestamp::from_second(secs).unwrap()
}

fn victim(name: &str, priority: i32, started: i64, milli_cpu: i64) -> Arc<PodInfo> {
    Arc::new(PodInfo {
        namespace: "default".to_string(),
        name: name.to_string(),
        uid: name.to_string(),
        priority,
        queued_at: at(started),
        requests: Resources { milli_cpu, ..Default::default() },
        labels: BTreeMap::from([("app".to_string(), "web".to_string())]),
        ..Default::default()
    })
}

fn preemptor(priority: i32, milli_cpu: i64) -> PodInfo {
    PodInfo {
        namespace: "default".to_string(),
        name: "preemptor".to_string(),
        uid: "preemptor".to_string(),
        priority,
        requests: Resources { milli_cpu, ..Default::default() },
        ..Default::default()
    }
}

fn node_with(pods: Vec<Arc<PodInfo>>, allocatable_cpu: i64) -> NodeInfo {
    let mut n = NodeInfo {
        name: "worker".to_string(),
        allocatable: Resources { milli_cpu: allocatable_cpu, ..Default::default() },
        allocatable_pods: 110,
        ..Default::default()
    };
    for (i, p) in pods.into_iter().enumerate() {
        n.add_pod(p, i as u64 + 1);
    }
    n
}

/// A `fits` closure over CPU: the preemptor fits when the node's committed
/// total, minus the removed pods, leaves room.
fn cpu_fits<'a>(
    node: &'a NodeInfo,
    preemptor: &'a PodInfo,
) -> impl FnMut(&[&PodInfo]) -> bool + 'a {
    move |removed: &[&PodInfo]| {
        let freed: i64 = removed.iter().map(|p| p.requests.milli_cpu).sum();
        let used = node.requested.milli_cpu - freed;
        used + preemptor.requests.milli_cpu <= node.allocatable.milli_cpu
    }
}

fn pdb(allowed: i32, already: &[&str]) -> PdbState {
    PdbState {
        namespace: "default".to_string(),
        selector: Some(LabelSelector {
            match_labels: Some(BTreeMap::from([("app".to_string(), "web".to_string())])),
            match_expressions: None,
        }),
        disruptions_allowed: allowed,
        already_disrupted: already.iter().map(|s| s.to_string()).collect(),
    }
}

// ── Eligibility ─────────────────────────────────────────────────────────

#[test]
fn preemption_policy_never_forbids_preempting() {
    assert_eq!(
        eligible_to_preempt(Some("Never"), false, false),
        Err(Ineligible::PolicyNever)
    );
    assert!(eligible_to_preempt(Some("PreemptLowerPriority"), false, false).is_ok());
    assert!(eligible_to_preempt(None, false, false).is_ok());
}

#[test]
fn a_pod_whose_previous_preemption_is_still_draining_waits() {
    // Otherwise it evicts a second set of pods for room that is already being
    // made — double the casualties for one placement.
    assert_eq!(
        eligible_to_preempt(None, true, false),
        Err(Ineligible::NominationInProgress)
    );
}

#[test]
fn a_dead_nomination_does_not_hold_a_pod_back_forever() {
    // If the nominated node turned out to be unresolvable, waiting for its
    // drain to finish would strand the pod permanently.
    assert!(eligible_to_preempt(None, true, true).is_ok());
}

// ── Victim ordering ─────────────────────────────────────────────────────

#[test]
fn more_important_sorts_higher_priority_first() {
    let high = victim("high", 100, 0, 0);
    let low = victim("low", 0, 0, 0);
    assert_eq!(more_important(&high, &low), std::cmp::Ordering::Less);
}

#[test]
fn among_equals_the_longer_running_pod_is_more_important() {
    // A pod running for hours has more to lose than one a minute old, so the
    // young one is taken first. Sorting the other way evicts exactly the
    // wrong member of a ReplicaSet.
    let old = victim("old", 0, 1_000, 0);
    let young = victim("young", 0, 9_000, 0);
    assert_eq!(more_important(&old, &young), std::cmp::Ordering::Less);
}

// ── PDB accounting ──────────────────────────────────────────────────────

#[test]
fn a_budget_with_headroom_absorbs_a_victim() {
    let mut budgets = vec![pdb(1, &[])];
    assert!(!violates_pdb(&victim("a", 0, 0, 0), &mut budgets));
    assert_eq!(budgets[0].disruptions_allowed, 0, "the budget is spent");
}

#[test]
fn a_budget_at_zero_makes_the_next_victim_a_violation() {
    let mut budgets = vec![pdb(1, &[])];
    assert!(!violates_pdb(&victim("a", 0, 0, 0), &mut budgets));
    assert!(violates_pdb(&victim("b", 0, 0, 0), &mut budgets));
}

#[test]
fn an_already_disrupted_pod_does_not_charge_the_budget_twice() {
    // THE exemption. Charging it again makes preemption believe it has less
    // headroom than it does, so it spares pods it could legitimately take.
    let mut budgets = vec![pdb(1, &["a"])];
    assert!(!violates_pdb(&victim("a", 0, 0, 0), &mut budgets));
    assert_eq!(
        budgets[0].disruptions_allowed, 1,
        "a disruption already booked by someone else must not be booked again"
    );
    assert!(!violates_pdb(&victim("b", 0, 0, 0), &mut budgets));
}

#[test]
fn a_budget_in_another_namespace_is_not_consulted() {
    let mut budgets = vec![PdbState { namespace: "other".to_string(), ..pdb(0, &[]) }];
    assert!(!violates_pdb(&victim("a", 0, 0, 0), &mut budgets));
}

#[test]
fn a_budget_whose_selector_does_not_match_is_not_consulted() {
    let mut budgets = vec![PdbState {
        selector: Some(LabelSelector {
            match_labels: Some(BTreeMap::from([("app".to_string(), "batch".to_string())])),
            match_expressions: None,
        }),
        ..pdb(0, &[])
    }];
    assert!(!violates_pdb(&victim("a", 0, 0, 0), &mut budgets));
}

// ── Victim selection ────────────────────────────────────────────────────

#[test]
fn a_node_where_the_preemptor_cannot_fit_even_empty_yields_no_victims() {
    // The gate that stops preemption killing pods for a pod it still cannot
    // place. Without it, a too-large pod empties a node and then stays
    // Pending anyway.
    let node = node_with(vec![victim("a", 0, 0, 500)], 1000);
    let p = preemptor(100, 5000);
    let mut budgets: Vec<PdbState> = vec![];

    let fits = cpu_fits(&node, &p);
    assert!(select_victims_on_node(&p, &node, &mut budgets, fits).is_none());
}

#[test]
fn only_the_pods_actually_needed_become_victims() {
    // Three removable pods, but freeing one is enough. Declaring all three
    // victims would work and would be catastrophic.
    let node = node_with(
        vec![victim("a", 0, 0, 300), victim("b", 0, 0, 300), victim("c", 0, 0, 300)],
        1000,
    );
    let p = preemptor(100, 300);
    let mut budgets: Vec<PdbState> = vec![];

    let fits = cpu_fits(&node, &p);
    let victims = select_victims_on_node(&p, &node, &mut budgets, fits).expect("candidate");

    assert_eq!(
        victims.pods.len(),
        1,
        "only one pod needed to go, got {:?}",
        victims.pods
    );
}

#[test]
fn a_pod_at_equal_priority_is_never_a_victim() {
    // Preemption is for making room for something *more* important. Allowing
    // equals lets same-priority pods evict each other in a loop.
    let node = node_with(vec![victim("peer", 100, 0, 900)], 1000);
    let p = preemptor(100, 900);
    let mut budgets: Vec<PdbState> = vec![];

    let fits = cpu_fits(&node, &p);
    assert!(select_victims_on_node(&p, &node, &mut budgets, fits).is_none());
}

#[test]
fn the_least_important_pod_is_taken_first() {
    // Two candidates, only one needs to go: the lower-priority one.
    let node = node_with(vec![victim("important", 50, 0, 500), victim("minor", 1, 0, 500)], 1000);
    let p = preemptor(100, 500);
    let mut budgets: Vec<PdbState> = vec![];

    let fits = cpu_fits(&node, &p);
    let victims = select_victims_on_node(&p, &node, &mut budgets, fits).expect("candidate");

    assert_eq!(victims.pods, vec!["default/minor".to_string()]);
}

#[test]
fn a_pdb_protected_pod_is_spared_when_another_will_do() {
    // The ordering rule that decides who dies. Both pods would satisfy the
    // preemptor; the one under a spent budget must be the survivor.
    // Only "protected" carries app=web, so only it is covered by the budget.
    let mut expendable = (*victim("expendable", 1, 100, 500)).clone();
    expendable.labels = BTreeMap::from([("app".to_string(), "batch".to_string())]);
    let node = node_with(vec![victim("protected", 1, 0, 500), Arc::new(expendable)], 1000);
    let p = preemptor(100, 500);

    // A budget with no headroom left, covering "protected" alone.
    let mut budgets = vec![pdb(0, &[])];

    let fits = cpu_fits(&node, &p);
    let victims = select_victims_on_node(&p, &node, &mut budgets, fits).expect("candidate");

    assert_eq!(
        victims.pods,
        vec!["default/expendable".to_string()],
        "the pod under an exhausted PDB should be the one spared"
    );
    assert_eq!(victims.pdb_violations, 0);
}

// ── Choosing the node ───────────────────────────────────────────────────

fn candidate(node: &str, victims: &[&str], violations: usize, highest: i32, sum: i64) -> Candidate {
    Candidate {
        node: node.to_string(),
        victims: Victims {
            pods: victims.iter().map(|s| s.to_string()).collect(),
            pdb_violations: violations,
        },
        highest_victim_priority: highest,
        sum_victim_priorities: sum,
        latest_start_of_highest: None,
    }
}

#[test]
fn no_candidates_yields_nothing() {
    assert!(pick_one_node(&[]).is_none());
}

#[test]
fn fewest_pdb_violations_wins_first() {
    let set = vec![
        candidate("breaks-budgets", &["a"], 2, 0, 0),
        candidate("clean", &["b", "c", "d"], 0, 0, 0),
    ];
    assert_eq!(
        pick_one_node(&set).unwrap().node,
        "clean",
        "breaking fewer budgets outranks killing fewer pods"
    );
}

#[test]
fn the_lowest_worst_casualty_wins_next() {
    let set = vec![
        candidate("kills-important", &["a"], 0, 1000, 1000),
        candidate("kills-minor", &["b", "c"], 0, 10, 20),
    ];
    assert_eq!(
        pick_one_node(&set).unwrap().node,
        "kills-minor",
        "minimising the worst thing killed outranks minimising how many"
    );
}

#[test]
fn the_smallest_total_damage_breaks_a_tie_on_the_worst() {
    let set = vec![
        candidate("heavier", &["a", "b"], 0, 50, 100),
        candidate("lighter", &["c", "d"], 0, 50, 60),
    ];
    assert_eq!(pick_one_node(&set).unwrap().node, "lighter");
}

#[test]
fn fewest_victims_breaks_a_tie_on_damage() {
    let set = vec![
        candidate("many", &["a", "b", "c"], 0, 50, 60),
        candidate("few", &["d"], 0, 50, 60),
    ];
    assert_eq!(pick_one_node(&set).unwrap().node, "few");
}

#[test]
fn the_youngest_worst_victim_breaks_a_tie_on_count() {
    // Among equally bad choices, kill the youngest of the worst.
    let mut older = candidate("older", &["a"], 0, 50, 50);
    older.latest_start_of_highest = Some(at(1_000));
    let mut younger = candidate("younger", &["b"], 0, 50, 50);
    younger.latest_start_of_highest = Some(at(9_000));

    assert_eq!(pick_one_node(&[older, younger]).unwrap().node, "younger");
}

#[test]
fn an_exact_tie_falls_back_to_list_order() {
    let set = vec![candidate("first", &["a"], 0, 0, 0), candidate("second", &["b"], 0, 0, 0)];
    assert_eq!(pick_one_node(&set).unwrap().node, "first");
}

// ── Candidate sampling ──────────────────────────────────────────────────

#[test]
fn no_potential_nodes_means_no_candidates() {
    let mut rng = Rng::new(1);
    assert_eq!(offset_and_num_candidates(0, &mut rng), (0, 0));
}

#[test]
fn a_small_cluster_considers_every_potential_node() {
    let mut rng = Rng::new(1);
    let (_, num) = offset_and_num_candidates(5, &mut rng);
    assert_eq!(num, 5, "the absolute floor exceeds the cluster, so take all of it");
}

#[test]
fn a_large_cluster_samples_by_percentage_above_the_floor() {
    let mut rng = Rng::new(1);
    let (_, num) = offset_and_num_candidates(5000, &mut rng);
    assert_eq!(num, 500, "10% of 5000, which is above the 100-node floor");
}

#[test]
fn the_starting_offset_moves_between_runs() {
    // Without this the same unlucky nodes are chewed over every time while
    // the rest of the cluster is never considered.
    let offsets: std::collections::HashSet<i32> = (0..50u64)
        .map(|seed| {
            let mut rng = Rng::new(seed);
            offset_and_num_candidates(100, &mut rng).0
        })
        .collect();
    assert!(offsets.len() > 5, "offsets barely varied: {offsets:?}");
}

#[test]
fn the_offset_is_always_inside_the_candidate_range() {
    for seed in 0..200u64 {
        let mut rng = Rng::new(seed);
        let (offset, _) = offset_and_num_candidates(7, &mut rng);
        assert!((0..7).contains(&offset), "offset {offset} out of range");
    }
}

// ── Nomination ──────────────────────────────────────────────────────────

fn nominee(uid: &str) -> Arc<PodInfo> {
    Arc::new(PodInfo { uid: uid.to_string(), name: uid.to_string(), ..Default::default() })
}

#[test]
fn a_nominated_pod_is_recorded_against_its_node() {
    let mut n = Nominator::default();
    n.nominate(nominee("p"), "worker-1");

    assert_eq!(n.nominated_node("p"), Some("worker-1"));
    assert_eq!(n.nominated_on("worker-1").len(), 1);
    assert_eq!(n.nominated_on("worker-1")[0].uid, "p");
}

#[test]
fn a_nominee_is_returned_whole_not_just_its_id() {
    // A filter injecting it needs its requests and labels, and they cannot be
    // recovered from the snapshot — a nominated pod is not placed yet.
    let mut n = Nominator::default();
    let mut p = (*nominee("p")).clone();
    p.requests.milli_cpu = 500;
    n.nominate(Arc::new(p), "worker-1");

    assert_eq!(n.nominated_on("worker-1")[0].requests.milli_cpu, 500);
}

#[test]
fn a_node_can_hold_several_nominees() {
    // And every one of them must be visible to the next pod filtering that
    // node — otherwise two preemptors both claim the same freed capacity.
    let mut n = Nominator::default();
    n.nominate(nominee("a"), "worker-1");
    n.nominate(nominee("b"), "worker-1");

    assert_eq!(n.nominated_on("worker-1").len(), 2);
}

#[test]
fn re_nominating_a_pod_moves_it_rather_than_duplicating_it() {
    let mut n = Nominator::default();
    n.nominate(nominee("p"), "worker-1");
    n.nominate(nominee("p"), "worker-2");

    assert_eq!(n.nominated_node("p"), Some("worker-2"));
    assert!(n.nominated_on("worker-1").is_empty());
    assert_eq!(n.nominated_on("worker-2").len(), 1);
    assert_eq!(n.len(), 1);
}

#[test]
fn removing_a_nomination_frees_the_node() {
    let mut n = Nominator::default();
    n.nominate(nominee("p"), "worker-1");
    n.remove("p");

    assert_eq!(n.nominated_node("p"), None);
    assert!(n.nominated_on("worker-1").is_empty());
    assert!(n.is_empty());
}

#[test]
fn removing_an_unknown_nomination_is_harmless() {
    let mut n = Nominator::default();
    n.remove("never-seen");
    assert!(n.is_empty());
}
