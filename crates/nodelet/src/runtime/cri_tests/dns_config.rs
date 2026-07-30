//! dns_config_for(): before this, PodSandboxConfig.dns_config was never set
//! at all — dnsPolicy: ClusterFirst (the pod-spec default) had no actual
//! effect and pods got whatever resolv.conf containerd defaulted to, not
//! cluster DNS.
use super::*;
use k8s_openapi::api::core::v1::{PodDNSConfig, PodDNSConfigOption, PodSpec};

fn pod(namespace: &str, dns_policy: Option<&str>, dns_config: Option<PodDNSConfig>) -> Pod {
    Pod {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: Some(PodSpec {
            dns_policy: dns_policy.map(|s| s.to_string()),
            dns_config,
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn cluster_first_with_no_cluster_dns_configured_falls_back_to_host_resolv_conf() {
    let p = pod("default", Some("ClusterFirst"), None);
    assert_eq!(dns_config_for(&p, &[], "cluster.local"), None);
}

#[test]
fn cluster_first_with_cluster_dns_configured_sets_servers_and_search() {
    let p = pod("myns", Some("ClusterFirst"), None);
    let dns = dns_config_for(&p, &["10.96.0.10".to_string()], "cluster.local").unwrap();
    assert_eq!(dns.servers, vec!["10.96.0.10".to_string()]);
    assert_eq!(
        dns.searches,
        vec!["myns.svc.cluster.local".to_string(), "svc.cluster.local".to_string(), "cluster.local".to_string()]
    );
    assert_eq!(dns.options, vec!["ndots:5".to_string()]);
}

#[test]
fn missing_dns_policy_defaults_to_cluster_first() {
    let p = pod("default", None, None);
    let dns = dns_config_for(&p, &["10.96.0.10".to_string()], "cluster.local");
    assert!(dns.is_some(), "unset dnsPolicy must behave like the ClusterFirst default");
}

#[test]
fn dns_policy_default_always_returns_none_even_with_cluster_dns_configured() {
    let p = pod("default", Some("Default"), None);
    assert_eq!(dns_config_for(&p, &["10.96.0.10".to_string()], "cluster.local"), None);
}

#[test]
fn custom_dns_config_servers_and_searches_are_appended() {
    let custom = PodDNSConfig {
        nameservers: Some(vec!["8.8.8.8".to_string()]),
        searches: Some(vec!["example.com".to_string()]),
        options: Some(vec![PodDNSConfigOption { name: Some("ndots".to_string()), value: Some("2".to_string()) }]),
    };
    let p = pod("default", Some("ClusterFirst"), Some(custom));
    let dns = dns_config_for(&p, &["10.96.0.10".to_string()], "cluster.local").unwrap();
    assert!(dns.servers.contains(&"10.96.0.10".to_string()));
    assert!(dns.servers.contains(&"8.8.8.8".to_string()));
    assert!(dns.searches.contains(&"example.com".to_string()));
    assert!(dns.options.contains(&"ndots:2".to_string()));
}

#[test]
fn custom_dns_config_alone_with_dns_policy_none_still_produces_a_config() {
    let custom = PodDNSConfig {
        nameservers: Some(vec!["8.8.8.8".to_string()]),
        searches: None,
        options: None,
    };
    let p = pod("default", Some("None"), Some(custom));
    let dns = dns_config_for(&p, &[], "cluster.local").unwrap();
    assert_eq!(dns.servers, vec!["8.8.8.8".to_string()]);
}

#[test]
fn dns_option_without_a_value_is_a_bare_flag() {
    let custom = PodDNSConfig {
        nameservers: None,
        searches: None,
        options: Some(vec![PodDNSConfigOption { name: Some("single-request-reopen".to_string()), value: None }]),
    };
    let p = pod("default", Some("None"), Some(custom));
    let dns = dns_config_for(&p, &[], "cluster.local").unwrap();
    assert_eq!(dns.options, vec!["single-request-reopen".to_string()]);
}
