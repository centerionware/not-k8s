
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn continue_token_round_trips_the_resume_key_and_revision() {
        let token = encode_continue_token(b"/registry/pods/default/my-pod\x00", 42);
        let (key, revision) = decode_continue_token(&token).expect("a token this module encoded must decode");
        assert_eq!(key, b"/registry/pods/default/my-pod\x00");
        assert_eq!(revision, 42);
    }

    #[test]
    fn continue_token_rejects_invalid_base64() {
        assert!(decode_continue_token("not valid base64!!!").is_none());
    }

    #[test]
    fn continue_token_rejects_a_missing_separator() {
        use base64::Engine;
        let no_separator = base64::engine::general_purpose::STANDARD.encode(b"no-null-byte-here");
        assert!(decode_continue_token(&no_separator).is_none());
    }

    #[test]
    fn continue_token_rejects_a_non_numeric_revision() {
        use base64::Engine;
        let mut buf = b"/registry/pods/default/x".to_vec();
        buf.push(0);
        buf.extend_from_slice(b"not-a-number");
        let bad = base64::engine::general_purpose::STANDARD.encode(buf);
        assert!(decode_continue_token(&bad).is_none());
    }

    #[test]
    fn resolve_kind_finds_a_real_known_resource() {
        assert_eq!(resolve_kind("", "v1", "pods"), Some("Pod"));
        assert_eq!(resolve_kind("apps", "v1", "deployments"), Some("Deployment"));
    }

    #[test]
    fn resolve_kind_finds_namespaced_rbac_resources() {
        for (resource, kind, schema) in [
            ("roles", "Role", "io.k8s.api.rbac.v1.Role"),
            ("rolebindings", "RoleBinding", "io.k8s.api.rbac.v1.RoleBinding"),
        ] {
            assert_eq!(resolve_kind("rbac.authorization.k8s.io", "v1", resource), Some(kind));
            assert_eq!(
                protobuf::schema_for_gvk("rbac.authorization.k8s.io", "v1", kind),
                Some(schema)
            );
        }
    }

    #[test]
    fn resolve_kind_is_none_for_an_unknown_resource_or_group_version() {
        assert_eq!(resolve_kind("", "v1", "totally-made-up"), None);
        assert_eq!(resolve_kind("totally.made.up", "v1", "pods"), None);
    }

    #[test]
    fn split_api_version_handles_core_and_grouped_forms() {
        assert_eq!(split_api_version("v1"), ("", "v1"));
        assert_eq!(split_api_version("apps/v1"), ("apps", "v1"));
    }

    /// The real round trip: encode a Namespace object the same way a
    /// write path would (`encode_message` + `wrap_unknown`), then prove
    /// `decode_stored_object` gets the exact same JSON back out.
    #[test]
    fn decode_stored_object_round_trips_a_real_encoded_object() {
        let schema = protobuf::schema_for_gvk("", "v1", "Namespace").expect("core/v1 Namespace should be a known schema");
        let value = json!({"metadata": {"name": "default"}});
        let object_bytes = protobuf::encode_message(schema, &value).unwrap();
        let envelope = protobuf::wrap_unknown("v1", "Namespace", &object_bytes);

        let decoded = decode_stored_object(&envelope).unwrap();
        assert_eq!(decoded["metadata"]["name"], "default");
    }

    #[test]
    fn decode_stored_object_rejects_a_non_envelope_payload() {
        assert!(decode_stored_object(b"not an envelope at all").is_err());
    }

    #[test]
    fn list_kind_appends_list_to_the_real_kind() {
        assert_eq!(list_kind("Pod"), "PodList");
        assert_eq!(list_kind("Deployment"), "DeploymentList");
    }

    #[test]
    fn name_format_violations_enforces_the_real_namespace_rule() {
        assert!(name_format_violations("", "namespaces", "my-namespace").is_empty());
        assert!(!name_format_violations("", "namespaces", "My_Namespace").is_empty());
    }

    #[test]
    fn name_format_violations_enforces_the_real_serviceaccount_rule() {
        assert!(name_format_violations("", "serviceaccounts", "my.sa-name").is_empty());
        assert!(!name_format_violations("", "serviceaccounts", "My_SA").is_empty());
    }

    #[test]
    fn patch_kind_for_content_type_recognizes_all_three_real_media_types() {
        assert_eq!(patch_kind_for_content_type("application/json-patch+json"), Some(PatchKind::Json));
        assert_eq!(patch_kind_for_content_type("application/merge-patch+json"), Some(PatchKind::Merge));
        assert_eq!(patch_kind_for_content_type("application/strategic-merge-patch+json"), Some(PatchKind::StrategicMerge));
    }

    #[test]
    fn patch_kind_for_content_type_ignores_charset_parameters() {
        assert_eq!(patch_kind_for_content_type("application/merge-patch+json; charset=utf-8"), Some(PatchKind::Merge));
    }

    #[test]
    fn patch_kind_for_content_type_rejects_unknown_or_ssa_media_types() {
        assert_eq!(patch_kind_for_content_type("application/json"), None);
        // Server-Side Apply has a separate listener path because its
        // media type carries field-manager semantics rather than being one
        // of the three ordinary patch kinds.
        assert_eq!(patch_kind_for_content_type("application/apply-patch+yaml"), None);
        assert_eq!(patch_kind_for_content_type(""), None);
    }

    #[test]
    fn omitted_content_type_uses_strategic_merge_for_builtins_and_merge_for_crds() {
        assert_eq!(default_patch_kind(false), PatchKind::StrategicMerge);
        assert_eq!(default_patch_kind(true), PatchKind::Merge);
    }

    #[test]
    fn server_side_apply_prunes_unknown_crd_fields_before_ownership() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {"type": "object", "properties": {"color": {"type": "string"}}}
            }
        });
        let result = prune_runtime_schema(
            Some(&schema),
            json!({
                "apiVersion": "example.test/v1",
                "kind": "Widget",
                "metadata": {"name": "widget", "labels": {"kept": "yes"}},
                "spec": {"color": "blue", "unknown": true},
                "unknown": "dropped"
            }),
        );
        assert_eq!(result["spec"], json!({"color": "blue"}));
        assert!(result.get("unknown").is_none());
        assert_eq!(result["metadata"]["labels"]["kept"], "yes");
    }

    #[test]
    fn converted_crd_objects_are_revalidated_against_the_storage_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "required": ["storageOnly"],
                    "properties": {"storageOnly": {"type": "string"}}
                }
            }
        });
        let invalid = revalidate_storage_object(Some(&schema), json!({
            "apiVersion": "example.com/v1",
            "kind": "Widget",
            "spec": {}
        }))
        .expect_err("a conversion result missing a storage-required field must be rejected");
        assert!(invalid.iter().any(|violation| violation == "spec.storageOnly: Required value"), "unexpected violations: {invalid:?}");

        let valid = revalidate_storage_object(Some(&schema), json!({
            "apiVersion": "example.com/v1",
            "kind": "Widget",
            "spec": {"storageOnly": "present", "unknown": true}
        }))
        .expect("a valid storage representation must pass");
        assert_eq!(valid["spec"], json!({"storageOnly": "present"}));
    }

    #[test]
    fn name_format_violations_is_empty_for_a_resource_with_no_verified_rule() {
        // events has no verified per-type name rule wired in yet -- must
        // not invent a check for it.
        assert!(name_format_violations("", "events", "Not-A-Valid-DNS-Label-But-Unchecked").is_empty());
    }

    #[test]
    fn name_format_violations_enforces_the_real_dns_subdomain_rule_on_each_verified_resource() {
        for resource in ["pods", "replicationcontrollers", "nodes", "limitranges", "resourcequotas", "secrets", "endpoints", "persistentvolumes", "configmaps"] {
            assert!(name_format_violations("", resource, "my-name.example").is_empty(), "{resource} should accept a valid DNS subdomain");
            assert!(!name_format_violations("", resource, "My_Bad_Name").is_empty(), "{resource} should reject an invalid DNS subdomain");
        }
    }

    #[test]
    fn name_format_violations_enforces_the_real_dns_subdomain_rule_on_each_verified_non_core_resource() {
        for (group, resource) in [
            ("scheduling.k8s.io", "priorityclasses"),
            ("resource.k8s.io", "resourceclaims"),
            ("resource.k8s.io", "resourceclaimtemplates"),
            ("storage.k8s.io", "storageclasses"),
        ] {
            assert!(name_format_violations(group, resource, "my-name.example").is_empty(), "{group}/{resource} should accept a valid DNS subdomain");
            assert!(!name_format_violations(group, resource, "My_Bad_Name").is_empty(), "{group}/{resource} should reject an invalid DNS subdomain");
        }
        // The same resource name under the wrong group must not match --
        // this table is keyed on (group, resource), not resource alone.
        assert!(name_format_violations("", "priorityclasses", "My_Bad_Name").is_empty());
    }

    #[test]
    fn name_format_violations_enforces_the_real_dns_subdomain_rule_on_each_newly_verified_resource() {
        for (group, resource) in [
            ("apps", "controllerrevisions"),
            ("apps", "daemonsets"),
            ("apps", "deployments"),
            ("apps", "replicasets"),
            ("networking.k8s.io", "ingresses"),
            ("networking.k8s.io", "ingressclasses"),
            ("networking.k8s.io", "servicecidrs"),
            ("discovery.k8s.io", "endpointslices"),
            ("flowcontrol.apiserver.k8s.io", "flowschemas"),
            ("flowcontrol.apiserver.k8s.io", "prioritylevelconfigurations"),
            ("node.k8s.io", "runtimeclasses"),
            ("coordination.k8s.io", "leases"),
        ] {
            assert!(name_format_violations(group, resource, "my-name.example").is_empty(), "{group}/{resource} should accept a valid DNS subdomain");
            assert!(!name_format_violations(group, resource, "My_Bad_Name").is_empty(), "{group}/{resource} should reject an invalid DNS subdomain");
        }
    }

    #[test]
    fn name_format_violations_enforces_the_real_service_dns1035_rule() {
        // DNS1035Label: must start with a letter, no leading digit and no
        // '.' (both allowed in a DNS1123 subdomain) -- proves this isn't
        // silently sharing the subdomain check.
        assert!(name_format_violations("", "services", "my-svc").is_empty());
        assert!(!name_format_violations("", "services", "1-starts-with-digit").is_empty());
        assert!(!name_format_violations("", "services", "has.a.dot").is_empty());
    }

    #[test]
    fn now_rfc3339_has_no_subsecond_precision_and_a_z_suffix() {
        let ts = now_rfc3339();
        assert!(ts.ends_with('Z'), "got {ts:?}");
        assert!(!ts.contains('.'), "must be second-precision only, got {ts:?}");
        // A real, parseable RFC3339 timestamp round-trips through chrono.
        assert!(chrono::DateTime::parse_from_rfc3339(&ts).is_ok(), "not valid RFC3339: {ts:?}");
    }

    #[test]
    fn set_metadata_field_creates_metadata_when_absent() {
        let mut obj = json!({});
        set_metadata_field(&mut obj, "uid", Value::String("abc".to_string()));
        assert_eq!(obj["metadata"]["uid"], "abc");
    }

    #[test]
    fn set_metadata_field_preserves_existing_metadata_fields() {
        let mut obj = json!({"metadata": {"name": "web-1"}});
        set_metadata_field(&mut obj, "uid", Value::String("abc".to_string()));
        assert_eq!(obj["metadata"]["name"], "web-1");
        assert_eq!(obj["metadata"]["uid"], "abc");
    }

    #[test]
    fn ephemeral_container_update_keeps_the_pod_and_only_appends() {
        let existing = json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "debuggable", "generation": 4, "uid": "uid-1"},
            "spec": {
                "nodeName": "node-a",
                "containers": [{"name": "app", "image": "busybox"}],
                "ephemeralContainers": [{"name": "old-debugger", "image": "busybox", "command": ["sleep", "3600"]}]
            },
            "status": {"phase": "Running"}
        });
        let candidate = json!({
            "metadata": {"name": "different", "uid": "different"},
            "spec": {
                "nodeName": "different-node",
                "containers": [{"name": "different", "image": "different"}],
                "ephemeralContainers": [
                    {"name": "old-debugger", "image": "busybox", "command": ["sleep", "3600"]},
                    {"name": "new-debugger", "image": "busybox", "targetContainerName": "app"}
                ]
            },
            "status": {"phase": "Failed"}
        });

        let result = restrict_ephemeral_container_update(&existing, &candidate).expect("valid append");
        assert_eq!(result["metadata"]["name"], "debuggable");
        assert_eq!(result["metadata"]["uid"], "uid-1");
        assert_eq!(result["metadata"]["generation"], 5);
        assert_eq!(result["spec"]["nodeName"], "node-a");
        assert_eq!(result["spec"]["containers"], existing["spec"]["containers"]);
        assert_eq!(result["status"], existing["status"]);
        assert_eq!(result["spec"]["ephemeralContainers"], candidate["spec"]["ephemeralContainers"]);
    }

    #[test]
    fn ephemeral_container_update_rejects_removing_or_changing_existing_entries() {
        let existing = json!({
            "metadata": {"generation": 1},
            "spec": {
                "containers": [{"name": "app"}],
                "ephemeralContainers": [{"name": "debugger", "image": "busybox"}]
            }
        });
        let removed = json!({"spec": {"ephemeralContainers": []}});
        let changed = json!({"spec": {"ephemeralContainers": [{"name": "debugger", "image": "other"}]}});

        let removed_errors = restrict_ephemeral_container_update(&existing, &removed).expect_err("removal must be rejected");
        assert!(removed_errors.iter().any(|error| error.contains("may not be removed")));
        let changed_errors = restrict_ephemeral_container_update(&existing, &changed).expect_err("mutation must be rejected");
        assert!(changed_errors.iter().any(|error| error.contains("may not be modified")));
    }

    #[test]
    fn ephemeral_container_update_rejects_invalid_new_entries() {
        let existing = json!({"spec": {"containers": [{"name": "app"}]}});
        let candidate = json!({
            "spec": {
                "ephemeralContainers": [
                    {"name": "app", "image": "busybox"},
                    {"name": "debugger", "image": "busybox", "ports": [{"containerPort": 80}]},
                    {"name": "debugger", "image": "busybox"}
                ]
            }
        });

        let errors = restrict_ephemeral_container_update(&existing, &candidate).expect_err("invalid entries must be rejected");
        assert!(errors.iter().any(|error| error.contains("duplicate a regular or init container")));
        assert!(errors.iter().any(|error| error.contains("ports")));
        assert!(errors.iter().any(|error| error.contains("must be unique")));
    }

    #[test]
    fn generated_name_appends_a_unique_five_character_suffix() {
        let first = generate_name("job-");
        let second = generate_name("job-");
        assert!(first.starts_with("job-"));
        assert_eq!(first.len(), "job-".len() + 5);
        assert_ne!(first, second);
    }

    #[test]
    fn protobuf_request_decodes_a_built_in_object_envelope() {
        let schema = protobuf::schema_for_gvk("", "v1", "ConfigMap").expect("ConfigMap has a generated schema");
        let encoded = protobuf::encode_message(schema, &json!({
            "metadata": {"name": "from-protobuf"},
            "data": {"key": "value"}
        })).unwrap();
        let envelope = protobuf::wrap_unknown("v1", "ConfigMap", &encoded);
        let resolved = ResolvedResource {
            kind: "ConfigMap".to_string(),
            schema: Some(schema),
            open_api_schema: None,
            storage_open_api_schema: None,
            has_status_subresource: true,
            conversion_webhook: None,
        };
        let decoded = decode_protobuf_object(&resolved, "configmaps", &envelope).unwrap();
        assert_eq!(decoded["apiVersion"], "v1");
        assert_eq!(decoded["kind"], "ConfigMap");
        assert_eq!(decoded["metadata"]["name"], "from-protobuf");
        assert_eq!(decoded["data"]["key"], "value");
    }

    #[test]
    fn protobuf_request_rejects_a_kind_that_does_not_match_the_resource() {
        let resolved = ResolvedResource {
            kind: "ConfigMap".to_string(),
            schema: None,
            open_api_schema: None,
            storage_open_api_schema: None,
            has_status_subresource: true,
            conversion_webhook: None,
        };
        let envelope = protobuf::wrap_unknown("v1", "Secret", br#"{}"#);
        assert!(matches!(decode_protobuf_object(&resolved, "configmaps", &envelope), Err(Error::InvalidProtobufRequest(_))));
    }
}
