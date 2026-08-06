//! dns_config_for(): before this, PodSandboxConfig.dns_config was never set
//! at all — dnsPolicy: ClusterFirst (the pod-spec default) had no actual
//! effect and pods got whatever resolv.conf containerd defaulted to, not
//! cluster DNS. Round 123: dnsPolicy: Default also needed the host's own
//! resolv.conf resolved explicitly (see env.rs's own doc comment on
//! effective_host_resolv_conf_path() — a systemd-resolved host's
//! /etc/resolv.conf is a loopback stub, and CoreDNS deliberately
//! crash-loops rather than risk forwarding through it) rather than just
//! trusting containerd's own default — found live crash-looping CoreDNS
//! for real on a systemd-resolved CI runner.
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
    assert_eq!(dns_config_for(&p, &[], "cluster.local", None), None);
}

#[test]
fn cluster_first_with_cluster_dns_configured_sets_servers_and_search() {
    let p = pod("myns", Some("ClusterFirst"), None);
    let dns = dns_config_for(&p, &["10.96.0.10".to_string()], "cluster.local", None).unwrap();
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
    let dns = dns_config_for(&p, &["10.96.0.10".to_string()], "cluster.local", None);
    assert!(dns.is_some(), "unset dnsPolicy must behave like the ClusterFirst default");
}

#[test]
fn dns_policy_default_with_no_readable_host_resolv_conf_returns_none() {
    let p = pod("default", Some("Default"), None);
    assert_eq!(dns_config_for(&p, &["10.96.0.10".to_string()], "cluster.local", None), None);
}

#[test]
fn dns_policy_default_parses_the_hosts_own_resolv_conf() {
    // Round 123: dnsPolicy: Default means "use the host's resolv.conf" —
    // now actually parsed and passed through explicitly (the caller reads
    // it via read_host_resolv_conf(), resolving the systemd-resolved stub
    // case) rather than left to containerd's own default.
    let p = pod("default", Some("Default"), None);
    let host_resolv_conf = "nameserver 1.1.1.1\nsearch example.com\noptions ndots:2\n";
    let dns = dns_config_for(&p, &["10.96.0.10".to_string()], "cluster.local", Some(host_resolv_conf)).unwrap();
    assert_eq!(dns.servers, vec!["1.1.1.1".to_string()], "Default must use the host's own resolv.conf, not cluster DNS");
    assert_eq!(dns.searches, vec!["example.com".to_string()]);
    assert_eq!(dns.options, vec!["ndots:2".to_string()]);
}

#[test]
fn custom_dns_config_servers_and_searches_are_appended() {
    let custom = PodDNSConfig {
        nameservers: Some(vec!["8.8.8.8".to_string()]),
        searches: Some(vec!["example.com".to_string()]),
        options: Some(vec![PodDNSConfigOption { name: Some("ndots".to_string()), value: Some("2".to_string()) }]),
    };
    let p = pod("default", Some("ClusterFirst"), Some(custom));
    let dns = dns_config_for(&p, &["10.96.0.10".to_string()], "cluster.local", None).unwrap();
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
    let dns = dns_config_for(&p, &[], "cluster.local", None).unwrap();
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
    let dns = dns_config_for(&p, &[], "cluster.local", None).unwrap();
    assert_eq!(dns.options, vec!["single-request-reopen".to_string()]);
}

// --- parse_resolv_conf() ---

#[test]
fn parse_resolv_conf_reads_nameserver_search_and_options_lines() {
    let contents = "nameserver 1.1.1.1\nnameserver 8.8.8.8\nsearch example.com other.com\noptions ndots:2 timeout:1\n";
    let (servers, searches, options) = parse_resolv_conf(contents);
    assert_eq!(servers, vec!["1.1.1.1".to_string(), "8.8.8.8".to_string()]);
    assert_eq!(searches, vec!["example.com".to_string(), "other.com".to_string()]);
    assert_eq!(options, vec!["ndots:2".to_string(), "timeout:1".to_string()]);
}

#[test]
fn parse_resolv_conf_ignores_comments_and_blank_lines() {
    let contents = "# a comment\n\nnameserver 1.1.1.1 # trailing comment\n";
    let (servers, _, _) = parse_resolv_conf(contents);
    assert_eq!(servers, vec!["1.1.1.1".to_string()]);
}

#[test]
fn parse_resolv_conf_empty_input_yields_empty_everything() {
    let (servers, searches, options) = parse_resolv_conf("");
    assert!(servers.is_empty());
    assert!(searches.is_empty());
    assert!(options.is_empty());
}
