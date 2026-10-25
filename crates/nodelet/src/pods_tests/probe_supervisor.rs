use super::{ProbeSupervisor, probe_supervisor_needs_restart};

#[test]
fn probe_supervisor_is_replaced_when_a_pod_ip_changes() {
    let supervisor = ProbeSupervisor {
        pod_ip: "10.42.0.11".to_string(),
        handles: Vec::new(),
    };

    assert!(!probe_supervisor_needs_restart(&supervisor, "10.42.0.11"));
    assert!(probe_supervisor_needs_restart(&supervisor, "10.42.0.103"));
}
