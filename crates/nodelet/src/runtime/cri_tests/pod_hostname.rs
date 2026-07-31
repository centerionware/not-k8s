//! resolve_pod_hostname(): spec.hostname/subdomain/setHostnameAsFQDN
//! (Round 38; found in round 35's re-audit).
use super::*;

#[test]
fn defaults_to_pod_name_when_hostname_unset() {
    let h = resolve_pod_hostname(None, None, false, "myapp", "ns", "cluster.local").unwrap();
    assert_eq!(h, "myapp");
}

#[test]
fn explicit_hostname_overrides_pod_name() {
    let h = resolve_pod_hostname(Some("custom"), None, false, "myapp", "ns", "cluster.local").unwrap();
    assert_eq!(h, "custom");
}

#[test]
fn subdomain_alone_does_not_change_the_hostname() {
    // subdomain only affects the headless-Service DNS search domain, not
    // the sandbox's own hostname, unless setHostnameAsFQDN is also true.
    let h = resolve_pod_hostname(None, Some("web"), false, "myapp", "ns", "cluster.local").unwrap();
    assert_eq!(h, "myapp");
}

#[test]
fn set_hostname_as_fqdn_without_subdomain_is_a_no_op() {
    // Matches real kubelet: there's no domain to form an FQDN with.
    let h = resolve_pod_hostname(None, None, true, "myapp", "ns", "cluster.local").unwrap();
    assert_eq!(h, "myapp");
}

#[test]
fn set_hostname_as_fqdn_with_subdomain_produces_the_full_fqdn() {
    let h = resolve_pod_hostname(Some("myapp"), Some("web"), true, "myapp", "ns", "cluster.local").unwrap();
    assert_eq!(h, "myapp.web.ns.svc.cluster.local");
}

#[test]
fn set_hostname_as_fqdn_over_64_bytes_is_rejected() {
    let long_subdomain = "a".repeat(60);
    let err = resolve_pod_hostname(Some("myapp"), Some(&long_subdomain), true, "myapp", "ns", "cluster.local")
        .unwrap_err();
    assert!(err.to_string().contains("64"));
}
