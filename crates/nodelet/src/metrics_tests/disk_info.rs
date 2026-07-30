use super::*;

#[test]
fn real_statvfs_on_tmp_returns_sane_numbers() {
    let disk = read_disk_info("/tmp").expect("statvfs(/tmp) should succeed on Linux CI");
    assert!(disk.total_bytes > 0);
    assert!(disk.available_bytes <= disk.total_bytes);
}

#[test]
fn nonexistent_path_returns_none_not_a_panic() {
    assert!(read_disk_info("/this/path/does/not/exist/hopefully").is_none());
}

#[test]
fn read_pressure_never_panics_and_fails_open_on_a_bad_path() {
    let pressure = read_pressure("/this/path/does/not/exist/hopefully", 100_000_000, 10);
    assert!(!pressure.disk, "an unreadable path must fail open (no pressure), not assume the worst");
}
