use super::*;

#[test]
fn shared_pool_excludes_reserved_cpus() {
    // 4 cores, 1000m reserved -> cpu 0 reserved, 1-3 shared.
    let mgr = CpuManager::new(4, 1000);
    assert_eq!(mgr.shared_pool(), [1, 2, 3].into_iter().collect());
}

#[test]
fn allocating_removes_cpus_from_the_shared_pool() {
    let mgr = CpuManager::new(4, 0);
    let picked = mgr.allocate("sandbox/app", 2).unwrap();
    assert_eq!(picked, [0, 1].into_iter().collect());
    assert_eq!(mgr.shared_pool(), [2, 3].into_iter().collect());
}

#[test]
fn releasing_returns_cpus_to_the_shared_pool() {
    let mgr = CpuManager::new(4, 0);
    mgr.allocate("sandbox/app", 2).unwrap();
    mgr.release("sandbox/app");
    assert_eq!(mgr.shared_pool(), [0, 1, 2, 3].into_iter().collect());
}

#[test]
fn releasing_an_unknown_key_is_a_harmless_no_op() {
    let mgr = CpuManager::new(4, 0);
    mgr.release("never-allocated");
    assert_eq!(mgr.shared_pool(), [0, 1, 2, 3].into_iter().collect());
}

#[test]
fn cannot_allocate_more_than_the_shared_pool_has() {
    let mgr = CpuManager::new(2, 0);
    assert!(mgr.allocate("sandbox/a", 3).is_none());
    // A failed allocation must not partially claim CPUs.
    assert_eq!(mgr.shared_pool(), [0, 1].into_iter().collect());
}

#[test]
fn two_containers_get_disjoint_exclusive_sets() {
    let mgr = CpuManager::new(4, 0);
    let a = mgr.allocate("sandbox/a", 2).unwrap();
    let b = mgr.allocate("sandbox/b", 2).unwrap();
    assert!(a.is_disjoint(&b));
    assert!(mgr.shared_pool().is_empty());
}

#[test]
fn release_sandbox_frees_every_container_in_it() {
    let mgr = CpuManager::new(4, 0);
    mgr.allocate("sandbox-1/a", 1).unwrap();
    mgr.allocate("sandbox-1/b", 1).unwrap();
    mgr.allocate("sandbox-2/c", 1).unwrap();
    mgr.release_sandbox("sandbox-1");
    // sandbox-1's two cores are back; sandbox-2's one core is still claimed.
    assert_eq!(mgr.shared_pool().len(), 3);
}

#[test]
fn release_sandbox_does_not_touch_other_sandboxes() {
    let mgr = CpuManager::new(4, 0);
    let c = mgr.allocate("sandbox-2/c", 1).unwrap();
    mgr.allocate("sandbox-1/a", 1).unwrap();
    mgr.release_sandbox("sandbox-1");
    assert!(!mgr.shared_pool().contains(c.iter().next().unwrap()));
}

#[test]
fn reserved_cpus_are_never_handed_out() {
    let mgr = CpuManager::new(2, 1000); // cpu 0 reserved, only cpu 1 available
    let picked = mgr.allocate("sandbox/a", 1).unwrap();
    assert_eq!(picked, [1].into_iter().collect());
    assert!(mgr.allocate("sandbox/b", 1).is_none()); // nothing left
}

#[test]
fn is_exclusive_reflects_a_live_claim() {
    let mgr = CpuManager::new(4, 0);
    assert!(!mgr.is_exclusive("sandbox/a"));
    mgr.allocate("sandbox/a", 1).unwrap();
    assert!(mgr.is_exclusive("sandbox/a"));
}

#[test]
fn is_exclusive_is_false_again_after_release() {
    let mgr = CpuManager::new(4, 0);
    mgr.allocate("sandbox/a", 1).unwrap();
    mgr.release("sandbox/a");
    assert!(!mgr.is_exclusive("sandbox/a"));
}

#[test]
fn is_exclusive_is_false_for_a_shared_pool_only_container() {
    // A container that never called allocate() (BestEffort/Burstable, or a
    // Guaranteed pod with a fractional CPU request) never appears here.
    let mgr = CpuManager::new(4, 0);
    assert!(!mgr.is_exclusive("sandbox/never-allocated"));
}

#[test]
fn allocate_preferring_picks_from_the_preferred_set_first() {
    let mgr = CpuManager::new(4, 0);
    let preferred: BTreeSet<u32> = [2, 3].into_iter().collect();
    let picked = mgr.allocate_preferring("sandbox/a", 2, Some(&preferred)).unwrap();
    assert_eq!(picked, preferred);
}

#[test]
fn allocate_preferring_tops_up_from_the_rest_of_the_pool_if_preferred_is_too_small() {
    let mgr = CpuManager::new(4, 0);
    let preferred: BTreeSet<u32> = [3].into_iter().collect();
    let picked = mgr.allocate_preferring("sandbox/a", 2, Some(&preferred)).unwrap();
    assert_eq!(picked.len(), 2);
    assert!(picked.contains(&3));
}

#[test]
fn allocate_preferring_with_none_behaves_like_plain_allocate() {
    let mgr = CpuManager::new(4, 0);
    let picked = mgr.allocate_preferring("sandbox/a", 2, None).unwrap();
    assert_eq!(picked, [0, 1].into_iter().collect());
}

#[test]
fn allocate_preferring_still_fails_if_the_whole_pool_cannot_satisfy_it() {
    let mgr = CpuManager::new(2, 0);
    let preferred: BTreeSet<u32> = [0].into_iter().collect();
    assert!(mgr.allocate_preferring("sandbox/a", 3, Some(&preferred)).is_none());
}

#[test]
fn assigned_is_none_before_any_allocation() {
    let mgr = CpuManager::new(4, 0);
    assert_eq!(mgr.assigned("sandbox/a"), None);
}

#[test]
fn assigned_reflects_the_live_claim() {
    let mgr = CpuManager::new(4, 0);
    let picked = mgr.allocate("sandbox/a", 2).unwrap();
    assert_eq!(mgr.assigned("sandbox/a"), Some(picked));
}

#[test]
fn assigned_is_none_again_after_release() {
    let mgr = CpuManager::new(4, 0);
    mgr.allocate("sandbox/a", 1).unwrap();
    mgr.release("sandbox/a");
    assert_eq!(mgr.assigned("sandbox/a"), None);
}

#[test]
fn allocatable_cpus_excludes_reserved_but_not_currently_claimed_ones() {
    // Unlike shared_pool(), allocatable_cpus() reports the whole
    // static-policy-managed pool regardless of what's claimed right now
    // -- PodResources API's GetAllocatableResources semantics.
    let mgr = CpuManager::new(4, 1000); // cpu 0 reserved
    mgr.allocate("sandbox/a", 2).unwrap();
    assert_eq!(mgr.allocatable_cpus(), [1, 2, 3].into_iter().collect());
}
