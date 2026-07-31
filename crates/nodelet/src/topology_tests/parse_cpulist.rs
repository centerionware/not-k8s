use super::*;

fn set(ids: &[u32]) -> BTreeSet<u32> {
    ids.iter().copied().collect()
}

#[test]
fn single_range() {
    assert_eq!(parse_cpulist("0-3"), set(&[0, 1, 2, 3]));
}

#[test]
fn single_cpu() {
    assert_eq!(parse_cpulist("2"), set(&[2]));
}

#[test]
fn mixed_ranges_and_singles() {
    assert_eq!(parse_cpulist("0-2,5,7-9"), set(&[0, 1, 2, 5, 7, 8, 9]));
}

#[test]
fn trailing_newline_and_whitespace_are_tolerated() {
    assert_eq!(parse_cpulist("0-3\n"), set(&[0, 1, 2, 3]));
    assert_eq!(parse_cpulist("  0-3  "), set(&[0, 1, 2, 3]));
}

#[test]
fn empty_string_is_an_empty_set() {
    assert!(parse_cpulist("").is_empty());
}

#[test]
fn garbage_entries_are_skipped_not_a_panic() {
    assert_eq!(parse_cpulist("0-1,garbage,3"), set(&[0, 1, 3]));
}

#[test]
fn a_reversed_range_is_skipped() {
    assert!(parse_cpulist("5-2").is_empty());
}

#[test]
fn round_trips_with_format_cpuset() {
    let original = set(&[0, 1, 2, 5, 7, 8, 9]);
    let rendered = crate::cpu_manager::format_cpuset(&original);
    assert_eq!(parse_cpulist(&rendered), original);
}
