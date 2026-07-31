use super::*;

fn set(ids: &[u32]) -> BTreeSet<u32> {
    ids.iter().copied().collect()
}

#[test]
fn empty_set_is_an_empty_string() {
    assert_eq!(format_cpuset(&BTreeSet::new()), "");
}

#[test]
fn single_cpu() {
    assert_eq!(format_cpuset(&set(&[3])), "3");
}

#[test]
fn contiguous_range_collapses_to_a_dash_range() {
    assert_eq!(format_cpuset(&set(&[0, 1, 2])), "0-2");
}

#[test]
fn mixed_ranges_and_singles() {
    assert_eq!(format_cpuset(&set(&[0, 1, 2, 5, 7, 8, 9])), "0-2,5,7-9");
}

#[test]
fn reserved_cpu_count_rounds_up_to_a_whole_core() {
    assert_eq!(reserved_cpu_count(0), 0);
    assert_eq!(reserved_cpu_count(1), 1); // any nonzero reservation reserves at least one whole core
    assert_eq!(reserved_cpu_count(1000), 1);
    assert_eq!(reserved_cpu_count(1500), 2);
    assert_eq!(reserved_cpu_count(2000), 2);
}
