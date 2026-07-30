use super::*;

#[test]
fn memory_pressure_when_available_below_threshold() {
    let mem = MemInfo { total_bytes: 8_000_000_000, available_bytes: 50_000_000 };
    assert!(memory_pressure(&mem, 100_000_000));
}

#[test]
fn no_memory_pressure_when_available_above_threshold() {
    let mem = MemInfo { total_bytes: 8_000_000_000, available_bytes: 500_000_000 };
    assert!(!memory_pressure(&mem, 100_000_000));
}

#[test]
fn memory_pressure_boundary_is_strictly_less_than() {
    let mem = MemInfo { total_bytes: 8_000_000_000, available_bytes: 100_000_000 };
    assert!(!memory_pressure(&mem, 100_000_000), "exactly at threshold is not yet pressure");
}

#[test]
fn disk_pressure_when_available_percent_below_threshold() {
    let disk = DiskInfo { total_bytes: 100_000, available_bytes: 5_000 }; // 5%
    assert!(disk_pressure(&disk, 10));
}

#[test]
fn no_disk_pressure_when_available_percent_above_threshold() {
    let disk = DiskInfo { total_bytes: 100_000, available_bytes: 50_000 }; // 50%
    assert!(!disk_pressure(&disk, 10));
}

#[test]
fn disk_pressure_zero_total_does_not_divide_by_zero() {
    let disk = DiskInfo { total_bytes: 0, available_bytes: 0 };
    assert!(!disk_pressure(&disk, 10));
}
