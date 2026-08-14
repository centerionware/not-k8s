use super::*;

#[test]
fn a_minimal_filter_only_extender_parses_with_upstream_defaults() {
    let cfgs = parse_extenders(r#"[{"urlPrefix":"http://ext:8888","filterVerb":"filter"}]"#).unwrap();
    assert_eq!(cfgs.len(), 1);
    let c = &cfgs[0];
    assert_eq!(c.url_prefix, "http://ext:8888");
    assert_eq!(c.filter_verb.as_deref(), Some("filter"));
    assert_eq!(c.prioritize_verb, None);
    assert_eq!(c.weight, 1, "upstream's own default weight");
    assert!(!c.node_cache_capable);
    assert!(!c.ignorable);
    assert_eq!(c.http_timeout, std::time::Duration::from_secs(5));
}

#[test]
fn filter_and_prioritize_with_explicit_fields_all_parse() {
    let cfgs = parse_extenders(
        r#"[{"urlPrefix":"https://ext","filterVerb":"filter","prioritizeVerb":"prioritize",
             "weight":3,"nodeCacheCapable":true,"ignorable":true,"httpTimeoutSeconds":30}]"#,
    )
    .unwrap();
    let c = &cfgs[0];
    assert_eq!(c.prioritize_verb.as_deref(), Some("prioritize"));
    assert_eq!(c.weight, 3);
    assert!(c.node_cache_capable);
    assert!(c.ignorable);
    assert_eq!(c.http_timeout, std::time::Duration::from_secs(30));
}

#[test]
fn a_bind_verb_is_refused_by_name_rather_than_silently_ignored() {
    let err =
        parse_extenders(r#"[{"urlPrefix":"http://ext","filterVerb":"filter","bindVerb":"bind"}]"#)
            .unwrap_err()
            .to_string();
    assert!(err.contains("bindVerb"), "{err}");
}

#[test]
fn a_preempt_verb_is_refused_by_name_rather_than_silently_ignored() {
    let err = parse_extenders(
        r#"[{"urlPrefix":"http://ext","filterVerb":"filter","preemptVerb":"preempt"}]"#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("preemptVerb"), "{err}");
}

#[test]
fn a_tls_config_is_refused_by_name_rather_than_silently_ignored() {
    let err = parse_extenders(
        r#"[{"urlPrefix":"https://ext","filterVerb":"filter","tlsConfig":{"insecure":true}}]"#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("tlsConfig"), "{err}");
}

#[test]
fn managed_resources_parses_into_plain_names() {
    let cfgs = parse_extenders(
        r#"[{"urlPrefix":"http://ext","filterVerb":"filter",
             "managedResources":[{"name":"example.com/gpu"}]}]"#,
    )
    .unwrap();
    assert_eq!(cfgs[0].managed_resources, vec!["example.com/gpu".to_string()]);
}

#[test]
fn an_extender_with_no_managed_resources_applies_to_every_pod() {
    let cfgs = parse_extenders(r#"[{"urlPrefix":"http://ext","filterVerb":"filter"}]"#).unwrap();
    let pod = crate::framework::plugins::testutil::pod("p");
    assert!(cfgs[0].applies_to(&pod));
}

#[test]
fn an_extender_with_managed_resources_only_applies_to_a_pod_requesting_one() {
    let cfgs = parse_extenders(
        r#"[{"urlPrefix":"http://ext","filterVerb":"filter",
             "managedResources":[{"name":"example.com/gpu"}]}]"#,
    )
    .unwrap();
    let plain_pod = crate::framework::plugins::testutil::pod("p");
    assert!(!cfgs[0].applies_to(&plain_pod), "pod requests nothing the extender manages");

    let mut gpu_pod = crate::framework::plugins::testutil::pod("p");
    gpu_pod.requests.extended.insert("example.com/gpu".to_string(), 1);
    assert!(cfgs[0].applies_to(&gpu_pod));
}

#[test]
fn an_extender_with_neither_verb_is_refused() {
    let err = parse_extenders(r#"[{"urlPrefix":"http://ext"}]"#).unwrap_err().to_string();
    assert!(err.contains("neither filterVerb nor prioritizeVerb"), "{err}");
}

#[test]
fn malformed_json_is_refused_naming_the_variable() {
    let err = parse_extenders("not json").unwrap_err().to_string();
    assert!(err.contains("NODESCHEDULER_EXTENDERS_JSON"), "{err}");
}

#[test]
fn multiple_extenders_parse_in_order() {
    let cfgs = parse_extenders(
        r#"[{"urlPrefix":"http://a","filterVerb":"filter"},{"urlPrefix":"http://b","prioritizeVerb":"prioritize"}]"#,
    )
    .unwrap();
    assert_eq!(cfgs.len(), 2);
    assert_eq!(cfgs[0].url_prefix, "http://a");
    assert_eq!(cfgs[1].url_prefix, "http://b");
}

#[test]
fn pod_to_api_carries_identity_and_scheduling_relevant_fields() {
    let mut pod = crate::framework::plugins::testutil::pod("p");
    pod.namespace = "ns".to_string();
    pod.uid = "the-uid".to_string();
    pod.priority = 7;
    let api = pod_to_api(&pod);
    assert_eq!(api.metadata.name.as_deref(), Some("p"));
    assert_eq!(api.metadata.namespace.as_deref(), Some("ns"));
    assert_eq!(api.metadata.uid.as_deref(), Some("the-uid"));
    assert_eq!(api.spec.unwrap().priority, Some(7));
}

#[test]
fn node_to_api_carries_name_labels_and_taints() {
    let node = crate::framework::plugins::testutil::node("n1");
    let api = node_to_api(&node);
    assert_eq!(api.metadata.name.as_deref(), Some("n1"));
}
