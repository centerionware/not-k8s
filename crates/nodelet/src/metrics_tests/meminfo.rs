use super::*;

const SAMPLE: &str = "\
MemTotal:        8149316 kB
MemFree:          423456 kB
MemAvailable:    3821904 kB
Buffers:          123456 kB
Cached:          2934567 kB
";

#[test]
fn parses_total_and_available_as_bytes() {
    let mem = parse_meminfo(SAMPLE).unwrap();
    assert_eq!(mem.total_bytes, 8149316 * 1024);
    assert_eq!(mem.available_bytes, 3821904 * 1024);
}

#[test]
fn ignores_unrelated_fields() {
    // MemFree must not be picked up in place of MemAvailable.
    let mem = parse_meminfo(SAMPLE).unwrap();
    assert_ne!(mem.available_bytes, 423456 * 1024);
}

#[test]
fn missing_required_field_returns_none() {
    assert!(parse_meminfo("MemTotal: 8149316 kB\n").is_none());
    assert!(parse_meminfo("MemAvailable: 3821904 kB\n").is_none());
    assert!(parse_meminfo("").is_none());
}

#[test]
fn garbage_value_returns_none() {
    assert!(parse_meminfo("MemTotal: not-a-number kB\nMemAvailable: 100 kB\n").is_none());
}

#[test]
fn real_proc_meminfo_parses_on_this_host() {
    // Sanity check against the real file this ships against in production.
    let mem = read_mem_info().expect("/proc/meminfo should parse on Linux CI");
    assert!(mem.total_bytes > 0);
    assert!(mem.available_bytes <= mem.total_bytes);
}
