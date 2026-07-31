use super::*;

fn nodes(pairs: &[(u32, &[u32])]) -> BTreeMap<u32, BTreeSet<u32>> {
    pairs.iter().map(|(n, cpus)| (*n, cpus.iter().copied().collect())).collect()
}

fn set(ids: &[u32]) -> BTreeSet<u32> {
    ids.iter().copied().collect()
}

// --- cpu_hint ---

#[test]
fn cpu_hint_picks_nodes_with_enough_free_cpus() {
    let topo = nodes(&[(0, &[0, 1, 2, 3]), (1, &[4, 5, 6, 7])]);
    let available = set(&[0, 1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(cpu_hint(&topo, &available, 2), set(&[0, 1]));
}

#[test]
fn cpu_hint_excludes_nodes_without_enough_free_cpus() {
    let topo = nodes(&[(0, &[0, 1, 2, 3]), (1, &[4, 5, 6, 7])]);
    // Only 1 free CPU left on node 0 (cpus 1,2,3 already claimed elsewhere).
    let available = set(&[0, 4, 5, 6, 7]);
    assert_eq!(cpu_hint(&topo, &available, 2), set(&[1]));
}

#[test]
fn cpu_hint_is_empty_when_no_node_can_satisfy_it() {
    let topo = nodes(&[(0, &[0, 1]), (1, &[2, 3])]);
    let available = set(&[0, 1, 2, 3]);
    assert!(cpu_hint(&topo, &available, 3).is_empty());
}

// --- device_hint ---

#[test]
fn device_hint_picks_nodes_with_enough_devices() {
    let all = set(&[0, 1]);
    let devices = [Some(0), Some(0), Some(1)];
    assert_eq!(device_hint(&devices, &all, 2), set(&[0]));
}

#[test]
fn device_hint_treats_untagged_devices_as_compatible_with_every_node() {
    let all = set(&[0, 1]);
    let devices = [None, None]; // driver reports no TopologyInfo at all
    assert_eq!(device_hint(&devices, &all, 2), set(&[0, 1]));
}

#[test]
fn device_hint_combines_untagged_and_tagged_devices_per_node() {
    let all = set(&[0, 1]);
    let devices = [Some(0), None]; // 1 tagged to node 0, 1 usable anywhere
    assert_eq!(device_hint(&devices, &all, 2), set(&[0])); // node 1 only has the 1 untagged device
}

#[test]
fn device_hint_is_empty_when_fewer_devices_exist_than_requested() {
    let all = set(&[0, 1]);
    let devices = [Some(0)];
    assert!(device_hint(&devices, &all, 2).is_empty());
}

// --- memory_hint ---

fn mem_map(pairs: &[(u32, u64)]) -> BTreeMap<u32, u64> {
    pairs.iter().copied().collect()
}

#[test]
fn memory_hint_picks_nodes_with_enough_free_bytes() {
    let free = mem_map(&[(0, 1_000_000), (1, 500_000)]);
    assert_eq!(memory_hint(&free, 800_000), set(&[0]));
}

#[test]
fn memory_hint_includes_every_node_that_qualifies() {
    let free = mem_map(&[(0, 1_000_000), (1, 2_000_000)]);
    assert_eq!(memory_hint(&free, 800_000), set(&[0, 1]));
}

#[test]
fn memory_hint_is_empty_when_no_node_has_enough() {
    let free = mem_map(&[(0, 100), (1, 200)]);
    assert!(memory_hint(&free, 1_000).is_empty());
}

// --- align ---

#[test]
fn align_with_no_hints_at_all_is_none() {
    assert_eq!(align(&[]), None);
}

#[test]
fn align_with_one_hint_picks_its_lowest_node() {
    assert_eq!(align(&[set(&[1, 2])]), Some(1));
}

#[test]
fn align_intersects_multiple_hints() {
    assert_eq!(align(&[set(&[0, 1, 2]), set(&[1, 2, 3])]), Some(1));
}

#[test]
fn align_is_none_when_hints_share_no_common_node() {
    assert_eq!(align(&[set(&[0]), set(&[1])]), None);
}

#[test]
fn align_with_an_empty_hint_in_the_mix_is_none() {
    // A provider that found nothing satisfying at all means no alignment
    // is possible, full stop — even if other providers found plenty.
    assert_eq!(align(&[set(&[0, 1]), BTreeSet::new()]), None);
}

// --- spread ---

#[test]
fn spread_with_no_hints_at_all_is_an_empty_placement() {
    assert_eq!(spread(&[]), Some(vec![]));
}

#[test]
fn spread_picks_each_hints_own_lowest_node_independently() {
    // No common node between {0} and {1}, but spread doesn't need one —
    // each provider gets its own best node.
    assert_eq!(spread(&[set(&[0]), set(&[1])]), Some(vec![0, 1]));
}

#[test]
fn spread_still_prefers_the_lowest_node_when_multiple_qualify() {
    assert_eq!(spread(&[set(&[2, 1]), set(&[3, 0])]), Some(vec![1, 0]));
}

#[test]
fn spread_is_none_when_any_hint_is_completely_empty() {
    // A resource with nowhere on the node it could go at all is still a
    // hard reject, same as align() — spreading the others doesn't help.
    assert_eq!(spread(&[set(&[0, 1]), BTreeSet::new()]), None);
}
