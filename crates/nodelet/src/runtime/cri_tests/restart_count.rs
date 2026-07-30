//! restart_count_from()/bump_restart_count_in()/clear_restart_counts_in():
//! the side table backing PodStatus.containerStatuses[].restartCount, which
//! was hardcoded 0 before this round regardless of how many times a
//! container actually crashed and got recreated.
use super::*;

#[test]
fn a_container_that_has_never_restarted_reports_zero() {
    let counts = HashMap::new();
    assert_eq!(restart_count_from(&counts, "sb-1", "app"), 0);
}

#[test]
fn bumping_increments_and_returns_the_new_count() {
    let mut counts = HashMap::new();
    assert_eq!(bump_restart_count_in(&mut counts, "sb-1", "app"), 1);
    assert_eq!(bump_restart_count_in(&mut counts, "sb-1", "app"), 2);
    assert_eq!(restart_count_from(&counts, "sb-1", "app"), 2);
}

#[test]
fn different_containers_in_the_same_sandbox_are_independent() {
    let mut counts = HashMap::new();
    bump_restart_count_in(&mut counts, "sb-1", "app");
    bump_restart_count_in(&mut counts, "sb-1", "app");
    bump_restart_count_in(&mut counts, "sb-1", "sidecar");
    assert_eq!(restart_count_from(&counts, "sb-1", "app"), 2);
    assert_eq!(restart_count_from(&counts, "sb-1", "sidecar"), 1);
}

#[test]
fn different_sandboxes_are_independent_even_with_the_same_container_name() {
    let mut counts = HashMap::new();
    bump_restart_count_in(&mut counts, "sb-1", "app");
    assert_eq!(restart_count_from(&counts, "sb-2", "app"), 0);
}

#[test]
fn clearing_a_sandbox_removes_only_its_own_entries() {
    let mut counts = HashMap::new();
    bump_restart_count_in(&mut counts, "sb-1", "app");
    bump_restart_count_in(&mut counts, "sb-2", "app");
    clear_restart_counts_in(&mut counts, "sb-1");
    assert_eq!(restart_count_from(&counts, "sb-1", "app"), 0);
    assert_eq!(restart_count_from(&counts, "sb-2", "app"), 1, "clearing sb-1 must not touch sb-2's entry");
}
