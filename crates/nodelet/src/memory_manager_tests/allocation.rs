use super::*;

fn cap(pairs: &[(u32, u64)]) -> BTreeMap<u32, u64> {
    pairs.iter().copied().collect()
}

#[test]
fn allocates_from_the_lowest_numbered_node_with_room() {
    let mgr = MemoryManager::new(cap(&[(0, 1_000), (1, 1_000)]));
    let node = mgr.allocate("sandbox/a", 500).unwrap();
    assert_eq!(node, 0);
}

#[test]
fn allocating_reduces_free_capacity_on_that_node_only() {
    let mgr = MemoryManager::new(cap(&[(0, 1_000), (1, 1_000)]));
    mgr.allocate("sandbox/a", 700).unwrap();
    let free = mgr.free_per_node();
    assert_eq!(free.get(&0), Some(&300));
    assert_eq!(free.get(&1), Some(&1_000));
}

#[test]
fn cannot_allocate_more_than_any_single_node_has_even_if_the_total_would_fit() {
    // 1500 bytes requested; no single node has that much even though two
    // nodes combined would — this implementation never spans nodes.
    let mgr = MemoryManager::new(cap(&[(0, 1_000), (1, 1_000)]));
    assert!(mgr.allocate("sandbox/a", 1_500).is_none());
}

#[test]
fn releasing_returns_capacity() {
    let mgr = MemoryManager::new(cap(&[(0, 1_000)]));
    mgr.allocate("sandbox/a", 700).unwrap();
    mgr.release("sandbox/a");
    assert_eq!(mgr.free_per_node().get(&0), Some(&1_000));
}

#[test]
fn releasing_an_unknown_key_is_a_harmless_no_op() {
    let mgr = MemoryManager::new(cap(&[(0, 1_000)]));
    mgr.release("never-allocated");
    assert_eq!(mgr.free_per_node().get(&0), Some(&1_000));
}

#[test]
fn release_sandbox_frees_every_container_in_it() {
    let mgr = MemoryManager::new(cap(&[(0, 1_000)]));
    mgr.allocate("sandbox-1/a", 200).unwrap();
    mgr.allocate("sandbox-1/b", 200).unwrap();
    mgr.allocate("sandbox-2/c", 200).unwrap();
    mgr.release_sandbox("sandbox-1");
    assert_eq!(mgr.free_per_node().get(&0), Some(&800)); // 1000 - 200 (sandbox-2 only)
}

#[test]
fn is_pinned_reflects_a_live_claim() {
    let mgr = MemoryManager::new(cap(&[(0, 1_000)]));
    assert!(!mgr.is_pinned("sandbox/a"));
    mgr.allocate("sandbox/a", 500).unwrap();
    assert!(mgr.is_pinned("sandbox/a"));
    mgr.release("sandbox/a");
    assert!(!mgr.is_pinned("sandbox/a"));
}

#[test]
fn allocate_preferring_tries_the_preferred_node_first() {
    let mgr = MemoryManager::new(cap(&[(0, 1_000), (1, 1_000)]));
    let node = mgr.allocate_preferring("sandbox/a", 500, Some(1)).unwrap();
    assert_eq!(node, 1);
}

#[test]
fn allocate_preferring_falls_back_when_the_preferred_node_has_no_room() {
    let mgr = MemoryManager::new(cap(&[(0, 1_000), (1, 100)]));
    let node = mgr.allocate_preferring("sandbox/a", 500, Some(1)).unwrap();
    assert_eq!(node, 0);
}

#[test]
fn allocate_preferring_with_none_behaves_like_plain_allocate() {
    let mgr = MemoryManager::new(cap(&[(0, 1_000), (1, 1_000)]));
    let node = mgr.allocate_preferring("sandbox/a", 500, None).unwrap();
    assert_eq!(node, 0);
}

#[test]
fn free_per_node_with_no_pins_equals_capacity() {
    let mgr = MemoryManager::new(cap(&[(0, 1_000), (1, 2_000)]));
    assert_eq!(mgr.free_per_node(), cap(&[(0, 1_000), (1, 2_000)]));
}
