use super::*;

#[test]
fn pid_pressure_when_available_percent_below_threshold() {
    let pid = PidInfo { max_pids: 100_000, current_pids: 96_000 }; // 4% available
    assert!(pid_pressure(&pid, 10));
}

#[test]
fn no_pid_pressure_when_available_percent_above_threshold() {
    let pid = PidInfo { max_pids: 100_000, current_pids: 50_000 }; // 50% available
    assert!(!pid_pressure(&pid, 10));
}

#[test]
fn pid_pressure_boundary_is_strictly_less_than() {
    let pid = PidInfo { max_pids: 100_000, current_pids: 90_000 }; // exactly 10% available
    assert!(!pid_pressure(&pid, 10), "exactly at threshold is not yet pressure");
}

#[test]
fn current_pids_above_max_does_not_underflow() {
    // Shouldn't happen in practice, but pid_max can change at runtime;
    // this must not panic on subtraction overflow.
    let pid = PidInfo { max_pids: 100, current_pids: 500 };
    assert!(pid_pressure(&pid, 10));
}

#[test]
fn zero_max_pids_does_not_divide_by_zero() {
    let pid = PidInfo { max_pids: 0, current_pids: 0 };
    assert!(!pid_pressure(&pid, 10));
}

#[test]
fn real_proc_pid_max_and_scan_succeed_on_this_host() {
    let pid = read_pid_info().expect("/proc/sys/kernel/pid_max and /proc scan should work on Linux CI");
    assert!(pid.max_pids > 0);
    assert!(pid.current_pids > 0, "this very test process should count as at least one PID");
}
