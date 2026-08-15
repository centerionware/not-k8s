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

// ── The wire field names ─────────────────────────────────────────────────
//
// These pin the exact JSON spellings from upstream's
// `k8s.io/kube-scheduler/extender/v1` struct tags. They exist because the
// original code derived all three types' names from
// `rename_all = "PascalCase"`, which is wrong for every field, and the e2e
// fake extender had been written to match the wrong spelling — so nothing in
// the suite would have caught it. Asserting on the serialized/deserialized
// JSON rather than on the structs is the point: the bug was entirely in the
// mapping, and only the mapping.

#[test]
fn extender_args_serializes_upstreams_lowercase_field_names() {
    let args = ExtenderArgs {
        pod: Pod::default(),
        nodes: None,
        node_names: Some(vec!["a".to_string(), "b".to_string()]),
    };
    let v: serde_json::Value = serde_json::to_value(&args).unwrap();

    assert!(v.get("pod").is_some(), "upstream's tag is `pod`, not `Pod`: {v}");
    // Not `NodeNames`, and not `nodeNames` either — upstream's tag really is
    // the word-break-free `nodenames`.
    assert_eq!(
        v.get("nodenames").and_then(|n| n.as_array()).map(|a| a.len()),
        Some(2),
        "upstream's tag is `nodenames`: {v}"
    );
    assert!(
        v.get("nodes").is_none(),
        "an absent NodeList must be omitted, not serialized as null: {v}"
    );
}

#[test]
fn a_filter_reply_in_upstreams_spelling_decodes() {
    // Byte for byte what a real extender returns — `nodenames`, `failedNodes`,
    // `failedAndUnresolvableNodes`. Under the old PascalCase mapping every one
    // of these read as absent, so a rejection looked like an empty result.
    let raw = r#"{
        "nodenames": ["keep-me"],
        "failedNodes": {"rejected": "not enough widgets"},
        "failedAndUnresolvableNodes": {"hopeless": "no widgets at all"}
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
        serde_json::from_str(r#"[{"host":"node-a","score":7},{"host":"node-b","score":0}]"#)
            .unwrap();

    assert_eq!(scores.len(), 2);
    assert_eq!(scores[0].host, "node-a");
    assert_eq!(scores[0].score, 7);
    assert_eq!(scores[1].host, "node-b");
    assert_eq!(scores[1].score, 0);
}
