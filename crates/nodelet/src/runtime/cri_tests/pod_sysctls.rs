//! pod_sysctls(): spec.securityContext.sysctls -> CRI's sysctls map
//! (Round 41; found in round 39's re-audit).
use super::*;
use k8s_openapi::api::core::v1::Sysctl;

#[test]
fn no_pod_security_context_means_an_empty_map() {
    assert!(pod_sysctls(None).is_empty());
}

#[test]
fn no_sysctls_field_means_an_empty_map() {
    let psc = PodSecurityContext { sysctls: None, ..Default::default() };
    assert!(pod_sysctls(Some(&psc)).is_empty());
}

#[test]
fn sysctls_are_translated_into_a_name_value_map() {
    let psc = PodSecurityContext {
        sysctls: Some(vec![
            Sysctl { name: "net.core.somaxconn".to_string(), value: "1024".to_string() },
            Sysctl { name: "kernel.shm_rmid_forced".to_string(), value: "1".to_string() },
        ]),
        ..Default::default()
    };
    let map = pod_sysctls(Some(&psc));
    assert_eq!(map.get("net.core.somaxconn"), Some(&"1024".to_string()));
    assert_eq!(map.get("kernel.shm_rmid_forced"), Some(&"1".to_string()));
    assert_eq!(map.len(), 2);
}
