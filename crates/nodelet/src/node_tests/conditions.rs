//! conditions(): the Node.status.conditions nodelet reports. The Ready
//! condition here is what turns a taint/toleration story into a real
//! schedulable node — a wrong status string ("true" vs "True") would make
//! the node look NotReady to every controller that reads it.
use super::*;

#[test]
fn ready_true_sets_ready_condition_true() {
    let c = conditions(true);
    let ready = c.iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "True");
    assert_eq!(ready.reason.as_deref(), Some("NodeletReady"));
}

#[test]
fn ready_false_sets_ready_condition_false() {
    let c = conditions(false);
    let ready = c.iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "False");
}

#[test]
fn pressure_conditions_are_always_false() {
    // nodelet doesn't track real memory/disk/PID pressure yet — this pins
    // that as known, deliberate behavior (see build_pod_status's
    // restart_count comment for the same pattern) rather than something
    // that silently varies.
    for ready in [true, false] {
        let c = conditions(ready);
        for t in ["MemoryPressure", "DiskPressure", "PIDPressure"] {
            let cond = c.iter().find(|c| c.type_ == t).unwrap();
            assert_eq!(cond.status, "False", "{t} should always report False");
        }
    }
}

#[test]
fn all_four_standard_condition_types_are_present() {
    let c = conditions(true);
    let types: Vec<&str> = c.iter().map(|c| c.type_.as_str()).collect();
    assert_eq!(types, vec!["Ready", "MemoryPressure", "DiskPressure", "PIDPressure"]);
}

#[test]
fn every_condition_has_timestamps_set() {
    let c = conditions(true);
    for cond in &c {
        assert!(cond.last_heartbeat_time.is_some());
        assert!(cond.last_transition_time.is_some());
    }
}
