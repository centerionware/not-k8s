use super::*;

#[test]
fn zero_millicores_means_unlimited() {
    assert_eq!(cpu_max_line(0), "max");
}

#[test]
fn one_core_is_the_full_period_as_quota() {
    // 1000 millicores * 100_000us period / 1000 = 100_000 (one full core).
    assert_eq!(cpu_max_line(1000), "100000 100000");
}

#[test]
fn half_a_core() {
    assert_eq!(cpu_max_line(500), "50000 100000");
}

#[test]
fn multiple_cores() {
    assert_eq!(cpu_max_line(4000), "400000 100000");
}

#[test]
fn zero_bytes_memory_max_means_unlimited() {
    assert_eq!(memory_max_line(0), "max");
}

#[test]
fn nonzero_memory_max_is_the_raw_byte_count() {
    assert_eq!(memory_max_line(1_073_741_824), "1073741824");
}
