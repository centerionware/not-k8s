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
fn a_bind_only_extender_is_a_valid_upstream_configuration() {
    let cfg = parse_extenders(r#"[{"urlPrefix":"http://ext","bindVerb":"bind"}]"#).unwrap();
    assert_eq!(cfg[0].bind_verb.as_deref(), Some("bind"));
}

#[test]
fn more_than_one_bind_extender_is_rejected_like_upstream() {
    let err = parse_extenders(
        r#"[{"urlPrefix":"http://a","bindVerb":"bind"},{"urlPrefix":"http://b","bindVerb":"bind"}]"#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("only one"), "{err}");
}

#[test]
fn a_prioritizer_requires_a_positive_weight() {
    let err = parse_extenders(
        r#"[{"urlPrefix":"http://ext","prioritizeVerb":"score","weight":0}]"#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("positive weight"), "{err}");
}

#[test]
fn empty_verbs_mean_the_extension_is_not_implemented() {
    let cfg = parse_extenders(
        r#"[{"urlPrefix":"http://ext","filterVerb":"","bindVerb":"bind"}]"#,
    )
    .unwrap();
    assert_eq!(cfg[0].filter_verb, None);
    assert_eq!(cfg[0].bind_verb.as_deref(), Some("bind"));
}

#[test]
fn a_preempt_verb_is_supported() {
    let cfg = parse_extenders(
        r#"[{"urlPrefix":"http://ext","filterVerb":"filter","preemptVerb":"preempt"}]"#,
    )
    .unwrap();
    assert_eq!(cfg[0].preempt_verb.as_deref(), Some("preempt"));
}

#[test]
fn node_cache_preemption_uses_upstreams_pascal_case_uid_shape() {
    let args = ExtenderPreemptionArgs {
        pod: Pod::default(),
        node_name_to_victims: None,
        node_name_to_meta_victims: Some(BTreeMap::from([(
            "node-a".to_string(),
            WireMetaVictims {
                pods: vec![WireMetaPod { uid: "victim-uid".to_string() }],
                num_pdb_violations: 1,
            },
        )])),
    };
    let value = serde_json::to_value(args).unwrap();
    assert_eq!(
        value["NodeNameToMetaVictims"]["node-a"]["Pods"][0]["UID"],
        "victim-uid"
    );
    assert!(value.get("NodeNameToVictims").is_none());
}

#[test]
fn upstream_tls_config_and_duration_fields_parse() {
    let cfg = parse_extenders(
        r#"[{"urlPrefix":"https://ext","filterVerb":"filter","enableHTTPS":true,
             "httpTimeout":"1m30.5s","tlsConfig":{"insecure":true,"caData":"UEVN"}}]"#,
    )
    .unwrap();
    assert!(cfg[0].enable_https);
    assert_eq!(cfg[0].http_timeout, Duration::from_millis(90_500));
    let tls = cfg[0].tls_config.as_ref().unwrap();
    assert!(tls.insecure);
    assert_eq!(tls.ca_data.as_deref(), Some(b"PEM".as_slice()));
}

#[test]
fn tls_server_name_rewrites_sni_but_preserves_the_endpoint_host_header() {
    let config = parse_extenders(
        r#"[{"urlPrefix":"https://127.0.0.1:9443/base","filterVerb":"filter",
             "enableHTTPS":true,
             "tlsConfig":{"insecure":true,"serverName":"extender.internal"}}]"#,
    )
    .unwrap()
    .remove(0);
    let extender = Extender::new(config).unwrap();
    assert_eq!(extender.request_url_prefix, "https://extender.internal:9443/base");
    assert_eq!(extender.original_host_header.as_deref(), Some("127.0.0.1:9443"));
}

#[test]
fn a_zero_upstream_timeout_gets_the_five_second_default() {
    let cfg = parse_extenders(
        r#"[{"urlPrefix":"http://ext","filterVerb":"filter","httpTimeout":"0s"}]"#,
    )
    .unwrap();
    assert_eq!(cfg[0].http_timeout, Duration::from_secs(5));
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
fn a_managed_resource_cannot_belong_to_two_extenders() {
    let err = parse_extenders(
        r#"[
            {"urlPrefix":"http://a","filterVerb":"filter","managedResources":[{"name":"example.com/gpu"}]},
            {"urlPrefix":"http://b","filterVerb":"filter","managedResources":[{"name":"example.com/gpu"}]}
        ]"#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("more than one extender"), "{err}");
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
fn managed_resource_interest_includes_container_limits_like_upstream() {
    let cfgs = parse_extenders(
        r#"[{"urlPrefix":"http://ext","filterVerb":"filter",
             "managedResources":[{"name":"example.com/gpu"}]}]"#,
    )
    .unwrap();
    let api: Pod = serde_json::from_value(serde_json::json!({
        "spec": {"containers": [{
            "name": "c",
            "resources": {"limits": {"example.com/gpu": "1"}}
        }]}
    }))
    .unwrap();
    let mut pod = PodInfo::from_pod(&api, k8s_openapi::jiff::Timestamp::now());
    // Preserve the exact object just as the watch does when extenders exist.
    pod.api_object = Some(Box::new(api));
    assert!(cfgs[0].applies_to(&pod));
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

// ── The wire field names ─────────────────────────────────────────────────
//
// These pin the exact JSON spellings from upstream's
// `k8s.io/kube-scheduler/extender/v1` structs. They have no JSON tags, so Go's
// encoding/json uses the exported PascalCase field names verbatim.

#[test]
fn extender_args_serializes_upstreams_exported_go_field_names() {
    let args = ExtenderArgs {
        pod: Pod::default(),
        nodes: None,
        node_names: Some(vec!["a".to_string(), "b".to_string()]),
    };
    let v: serde_json::Value = serde_json::to_value(&args).unwrap();

    assert!(v.get("Pod").is_some(), "upstream's untagged Go field is `Pod`: {v}");
    assert_eq!(
        v.get("NodeNames").and_then(|n| n.as_array()).map(|a| a.len()),
        Some(2),
        "upstream's untagged Go field is `NodeNames`: {v}"
    );
    assert!(
        v.get("Nodes").is_none(),
        "an absent NodeList must be omitted, not serialized as null: {v}"
    );
}

#[test]
fn a_filter_reply_in_upstreams_spelling_decodes() {
    // Byte for byte what encoding/json emits for upstream's untagged Go
    // structs.
    let raw = r#"{
        "NodeNames": ["keep-me"],
        "FailedNodes": {"rejected": "not enough widgets"},
        "FailedAndUnresolvableNodes": {"hopeless": "no widgets at all"}
    }"#;
    let parsed: ExtenderFilterResult = serde_json::from_str(raw).unwrap();

    assert_eq!(parsed.node_names.as_deref(), Some(&["keep-me".to_string()][..]));
    assert_eq!(parsed.failed_nodes.get("rejected").map(String::as_str), Some("not enough widgets"));
    assert_eq!(
        parsed.failed_and_unresolvable_nodes.get("hopeless").map(String::as_str),
        Some("no widgets at all")
    );
    assert!(parsed.error.is_empty());
}

#[test]
fn a_prioritize_reply_in_upstreams_spelling_decodes() {
    // The failure this one guards is the loudest of the set: a Prioritize
    // reply that fails to decode is an error, not a missing score, so a
    // non-ignorable extender took the whole scheduling cycle down with it.
    let scores: Vec<HostPriority> =
        serde_json::from_str(r#"[{"Host":"node-a","Score":7},{"Host":"node-b","Score":0}]"#)
            .unwrap();

    assert_eq!(scores.len(), 2);
    assert_eq!(scores[0].host, "node-a");
    assert_eq!(scores[0].score, 7);
    assert_eq!(scores[1].host, "node-b");
    assert_eq!(scores[1].score, 0);
}
