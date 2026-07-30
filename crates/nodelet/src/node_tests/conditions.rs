//! conditions(): the Node.status.conditions nodelet reports. The Ready
//! condition here is what turns a taint/toleration story into a real
//! schedulable node — a wrong status string ("true" vs "True") would make
//! the node look NotReady to every controller that reads it.
use super::*;
use crate::metrics::Pressure;

fn no_pressure() -> Pressure {
    Pressure { memory: false, disk: false, pid: false }
}

#[test]
fn ready_true_sets_ready_condition_true() {
    let c = conditions(true, &no_pressure());
    let ready = c.iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "True");
    assert_eq!(ready.reason.as_deref(), Some("NodeletReady"));
}

#[test]
fn ready_false_sets_ready_condition_false() {
    let c = conditions(false, &no_pressure());
    let ready = c.iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "False");
}

#[test]
fn pressure_conditions_reflect_the_measured_pressure_argument() {
    // This is the fix for the known gap: MemoryPressure/DiskPressure used to
    // be hardcoded "False" regardless of input. Now they must track whatever
    // metrics::read_pressure() (real /proc + statvfs reads) actually says.
    let c = conditions(true, &Pressure { memory: true, disk: false, pid: false });
    assert_eq!(c.iter().find(|c| c.type_ == "MemoryPressure").unwrap().status, "True");
    assert_eq!(c.iter().find(|c| c.type_ == "DiskPressure").unwrap().status, "False");

    let c = conditions(true, &Pressure { memory: false, disk: true, pid: false });
    assert_eq!(c.iter().find(|c| c.type_ == "MemoryPressure").unwrap().status, "False");
    assert_eq!(c.iter().find(|c| c.type_ == "DiskPressure").unwrap().status, "True");

    let c = conditions(true, &Pressure { memory: false, disk: false, pid: true });
    assert_eq!(c.iter().find(|c| c.type_ == "PIDPressure").unwrap().status, "True");
}

#[test]
fn pid_pressure_false_reports_sufficient() {
    let c = conditions(true, &no_pressure());
    let cond = c.iter().find(|c| c.type_ == "PIDPressure").unwrap();
    assert_eq!(cond.status, "False");
    assert_eq!(cond.reason.as_deref(), Some("KubeletHasSufficientPID"));
}

#[test]
fn all_four_standard_condition_types_are_present() {
    let c = conditions(true, &no_pressure());
    let types: Vec<&str> = c.iter().map(|c| c.type_.as_str()).collect();
    assert_eq!(types, vec!["Ready", "MemoryPressure", "DiskPressure", "PIDPressure"]);
}

#[test]
fn every_condition_has_timestamps_set() {
    let c = conditions(true, &no_pressure());
    for cond in &c {
        assert!(cond.last_heartbeat_time.is_some());
        assert!(cond.last_transition_time.is_some());
    }
}
