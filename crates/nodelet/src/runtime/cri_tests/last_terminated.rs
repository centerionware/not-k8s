//! clear_last_terminated_in(): the pure sandbox-scoped sweep backing
//! lastState tracking (round 75), same pattern as restart_count.rs's
//! clear_restart_counts_in() tests.
use super::*;
use crate::runtime::TerminatedInfo;

fn info(exit_code: i32) -> TerminatedInfo {
    TerminatedInfo { container_id: None, exit_code, reason: "Error".to_string(), finished_at: None, message: String::new() }
}

#[test]
fn clearing_a_sandbox_removes_only_its_own_entries() {
    let mut table = HashMap::new();
    table.insert("sb-1/app".to_string(), info(1));
    table.insert("sb-2/app".to_string(), info(2));
    clear_last_terminated_in(&mut table, "sb-1");
    assert!(!table.contains_key("sb-1/app"));
    assert!(table.contains_key("sb-2/app"), "clearing sb-1 must not touch sb-2's entry");
}

#[test]
fn clearing_an_unknown_sandbox_is_a_harmless_no_op() {
    let mut table = HashMap::new();
    table.insert("sb-1/app".to_string(), info(1));
    clear_last_terminated_in(&mut table, "sb-does-not-exist");
    assert!(table.contains_key("sb-1/app"));
}

#[test]
fn clearing_an_empty_table_does_not_panic() {
    let mut table: HashMap<String, TerminatedInfo> = HashMap::new();
    clear_last_terminated_in(&mut table, "sb-1");
    assert!(table.is_empty());
}
