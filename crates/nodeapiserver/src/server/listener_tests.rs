#[cfg(test)]
mod tests {
    use super::*;

    fn parts(path: &str) -> Vec<String> {
        path::split_path(path)
    }

    #[test]
    fn api_root_serves_api_versions() {
        let route = route_discovery(&parts("/api"), None, &[], &[]);
        let DiscoveryRoute::Found(doc) = route else {
            panic!("expected Found")
        };
        assert_eq!(doc["kind"], "APIVersions");
    }

    #[test]
    fn api_v1_serves_the_core_group_resource_list() {
        let route = route_discovery(&parts("/api/v1"), None, &[], &[]);
        let DiscoveryRoute::Found(doc) = route else {
            panic!("expected Found")
        };
        assert_eq!(doc["kind"], "APIResourceList");
        assert_eq!(doc["groupVersion"], "v1");
    }

    #[test]
    fn apis_root_serves_the_group_list() {
        let route = route_discovery(&parts("/apis"), None, &[], &[]);
        let DiscoveryRoute::Found(doc) = route else {
            panic!("expected Found")
        };
        assert_eq!(doc["kind"], "APIGroupList");
    }

    #[test]
    fn apis_root_serves_aggregated_discovery_when_the_client_asks_for_it() {
        let accept = "application/json;as=APIGroupDiscoveryList;v=v2;g=apidiscovery.k8s.io";
        let route = route_discovery(&parts("/apis"), Some(accept), &[], &[]);
        let DiscoveryRoute::Found(doc) = route else {
            panic!("expected Found")
        };
        assert_eq!(doc["kind"], "APIGroupDiscoveryList");
    }

    #[test]
    fn api_root_serves_aggregated_discovery_when_the_client_asks_for_it() {
        let accept = "application/json;as=APIGroupDiscoveryList;v=v2;g=apidiscovery.k8s.io";
        let route = route_discovery(&parts("/api"), Some(accept), &[], &[]);
        let DiscoveryRoute::Found(doc) = route else {
            panic!("expected Found")
        };
        assert_eq!(doc["kind"], "APIGroupDiscoveryList");
        assert_eq!(doc["items"][0]["metadata"]["name"], "");
        assert_eq!(
            discovery_content_type(&parts("/api"), Some(accept)),
            AGGREGATED_DISCOVERY_CONTENT_TYPE
        );
        assert_eq!(
            discovery_content_type(&parts("/api/v1"), Some(accept)),
            "application/json"
        );
    }

    #[test]
    fn a_mismatched_as_version_falls_back_to_the_legacy_shape() {
        // v2beta1 is real upstream's pre-GA aggregated-discovery shape,
        // which this build doesn't separately model — must not be served
        // the v2 shape as if it matched.
        let accept = "application/json;as=APIGroupDiscoveryList;v=v2beta1;g=apidiscovery.k8s.io";
        let route = route_discovery(&parts("/apis"), Some(accept), &[], &[]);
        let DiscoveryRoute::Found(doc) = route else {
            panic!("expected Found")
        };
        assert_eq!(
            doc["kind"], "APIGroupList",
            "an unmatched as= version must fall back to the legacy shape, not silently serve v2 anyway"
        );
    }

    #[test]
    fn apis_group_serves_the_group_document() {
        let route = route_discovery(&parts("/apis/apps"), None, &[], &[]);
        let DiscoveryRoute::Found(doc) = route else {
            panic!("expected Found")
        };
        assert_eq!(doc["kind"], "APIGroup");
        assert_eq!(doc["name"], "apps");
    }

    #[test]
    fn apis_group_version_serves_the_resource_list() {
        let route = route_discovery(&parts("/apis/apps/v1"), None, &[], &[]);
        let DiscoveryRoute::Found(doc) = route else {
            panic!("expected Found")
        };
        assert_eq!(doc["kind"], "APIResourceList");
        assert_eq!(doc["groupVersion"], "apps/v1");
    }

    #[test]
    fn aggregated_discovery_group_version_matches_a_real_apis_group_version_path() {
        let aggregated = [("metrics.k8s.io".to_string(), "v1beta1".to_string())];
        assert_eq!(
            aggregated_discovery_group_version(&parts("/apis/metrics.k8s.io/v1beta1"), &aggregated),
            Some(("metrics.k8s.io", "v1beta1"))
        );
    }

    #[test]
    fn aggregated_discovery_group_version_is_none_for_a_group_not_in_the_aggregated_list() {
        let aggregated = [("metrics.k8s.io".to_string(), "v1beta1".to_string())];
        assert_eq!(
            aggregated_discovery_group_version(&parts("/apis/apps/v1"), &aggregated),
            None
        );
    }

    #[test]
    fn aggregated_discovery_group_version_requires_exactly_three_apis_segments() {
        let aggregated = [("metrics.k8s.io".to_string(), "v1beta1".to_string())];
        assert_eq!(
            aggregated_discovery_group_version(&parts("/apis/metrics.k8s.io"), &aggregated),
            None,
            "a group-only path must not match"
        );
        assert_eq!(
            aggregated_discovery_group_version(
                &parts("/apis/metrics.k8s.io/v1beta1/nodes"),
                &aggregated
            ),
            None,
            "a resource-shaped path is handled by the resource-request aggregation branch, not this one"
        );
    }

    #[test]
    fn aggregated_discovery_group_version_ignores_a_matching_version_under_a_different_top_level_prefix()
     {
        let aggregated = [("metrics.k8s.io".to_string(), "v1beta1".to_string())];
        assert_eq!(
            aggregated_discovery_group_version(&parts("/api/metrics.k8s.io/v1beta1"), &aggregated),
            None
        );
    }

    #[test]
    fn an_unknown_group_is_a_real_not_found_not_a_fallthrough() {
        assert!(matches!(
            route_discovery(&parts("/apis/totally.made.up"), None, &[], &[]),
            DiscoveryRoute::NotFound
        ));
        assert!(matches!(
            route_discovery(&parts("/apis/apps/v999"), None, &[], &[]),
            DiscoveryRoute::NotFound
        ));
        assert!(matches!(
            route_discovery(&parts("/api/v999"), None, &[], &[]),
            DiscoveryRoute::NotFound
        ));
    }

    #[test]
    fn a_resource_shaped_path_is_not_applicable_to_discovery_routing() {
        assert!(matches!(
            route_discovery(&parts("/api/v1/namespaces/default/pods"), None, &[], &[]),
            DiscoveryRoute::NotApplicable
        ));
        assert!(matches!(
            route_discovery(
                &parts("/apis/apps/v1/namespaces/default/deployments"),
                None,
                &[],
                &[]
            ),
            DiscoveryRoute::NotApplicable
        ));
        assert!(matches!(
            route_discovery(&parts("/"), None, &[], &[]),
            DiscoveryRoute::NotApplicable
        ));
    }

    #[test]
    fn openapi_v3_root_serves_the_root_index() {
        let route = route_discovery(&parts("/openapi/v3"), None, &[], &[]);
        let DiscoveryRoute::Found(doc) = route else {
            panic!("expected Found")
        };
        assert!(
            doc["paths"]
                .as_object()
                .unwrap()
                .contains_key("apis/apps/v1")
        );
    }

    #[test]
    fn openapi_v2_serves_a_swagger_document() {
        let route = route_discovery(&parts("/openapi/v2"), None, &[], &[]);
        let DiscoveryRoute::Found(doc) = route else {
            panic!("expected Found")
        };
        assert_eq!(doc["swagger"], "2.0");
        assert!(
            doc["definitions"]
                .as_object()
                .is_some_and(|definitions| definitions.contains_key("io.k8s.api.core.v1.Pod"))
        );
    }

    #[test]
    fn openapi_v3_a_multi_segment_path_serves_the_raw_vendored_document() {
        let route = route_discovery(&parts("/openapi/v3/apis/apps/v1"), None, &[], &[]);
        let DiscoveryRoute::FoundRaw(bytes) = route else {
            panic!("expected FoundRaw")
        };
        let parsed: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        assert!(parsed.get("openapi").is_some());
    }

    #[test]
    fn openapi_v3_an_unvendored_path_is_a_real_not_found() {
        assert!(matches!(
            route_discovery(
                &parts("/openapi/v3/apis/totally.made.up/v1"),
                None,
                &[],
                &[]
            ),
            DiscoveryRoute::NotFound
        ));
    }

    #[test]
    fn version_serves_the_real_version_info_document() {
        let route = route_discovery(&parts("/version"), None, &[], &[]);
        let DiscoveryRoute::Found(doc) = route else {
            panic!("expected Found")
        };
        assert!(doc.get("gitVersion").is_some());
    }

    #[test]
    fn not_found_status_has_the_real_client_go_status_shape() {
        let status = not_found_status("/apis/totally.made.up");
        assert_eq!(status["kind"], "Status");
        assert_eq!(status["apiVersion"], "v1");
        assert_eq!(status["status"], "Failure");
        assert_eq!(status["reason"], "NotFound");
        assert_eq!(status["code"], 404);
    }

    #[test]
    fn bad_request_status_carries_the_selector_parse_detail() {
        let status = bad_request_status("/api/v1/pods", "malformed selector");
        assert_eq!(status["kind"], "Status");
        assert_eq!(status["reason"], "BadRequest");
        assert_eq!(status["code"], 400);
        assert!(
            status["message"]
                .as_str()
                .unwrap()
                .contains("malformed selector")
        );
    }

    #[test]
    fn oversized_body_status_uses_the_real_http_error_shape() {
        let status = request_entity_too_large_status("/api/v1/configmaps", 8192);
        assert_eq!(status["kind"], "Status");
        assert_eq!(status["reason"], "RequestEntityTooLarge");
        assert_eq!(status["code"], 413);
        assert!(
            status["message"]
                .as_str()
                .unwrap()
                .contains("8192-byte limit")
        );
    }

    #[test]
    fn forbidden_status_names_the_user_and_uses_the_real_rbac_denial_shape() {
        let status = forbidden_status("/api/v1/pods", "system:anonymous");
        assert_eq!(status["kind"], "Status");
        assert_eq!(status["reason"], "Forbidden");
        assert_eq!(status["code"], 403);
        assert!(
            status["message"]
                .as_str()
                .unwrap()
                .contains("system:anonymous")
        );
    }

    #[test]
    fn conflict_status_uses_the_real_already_exists_shape() {
        let status = conflict_status("/api/v1/namespaces/default/pods");
        assert_eq!(status["kind"], "Status");
        assert_eq!(status["reason"], "AlreadyExists");
        assert_eq!(status["code"], 409);
    }

    /// A create-only-if-absent race and an ordinary UPDATE/PATCH losing
    /// optimistic concurrency are two different real upstream `Status`
    /// shapes (docs/APISERVER_E2E_FIX.md, "Pure admission not re-applied
    /// on PATCH" -- the actual bug this test guards is upstream of that
    /// finding's own root cause: nodeapiserver mislabeled every
    /// UpdateOutcome::Conflict site with reason "AlreadyExists" instead
    /// of "Conflict", which real client-go's own IsConflict() would never
    /// match). Confirm the two builders stay distinguishable.
    #[test]
    fn update_conflict_status_uses_the_real_conflict_shape_not_already_exists() {
        let status = update_conflict_status("/api/v1/namespaces/default/pods/web");
        assert_eq!(status["kind"], "Status");
        assert_eq!(status["reason"], "Conflict");
        assert_eq!(status["code"], 409);
        assert_ne!(status["reason"], conflict_status("x")["reason"]);
    }

    #[test]
    fn dry_run_query_accepts_only_all() {
        assert_eq!(dry_run_query("dryRun=All").unwrap(), true);
        assert_eq!(dry_run_query("fieldManager=test").unwrap(), false);
        assert_eq!(
            dry_run_query("dryRun=Unknown").unwrap_err(),
            "dryRun must be All"
        );
    }

    #[test]
    fn authorization_reviews_bypass_resource_enforcement() {
        let sar = path::parse(
            "POST",
            "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews",
            "",
        );
        let self_review = path::parse(
            "POST",
            "/apis/authentication.k8s.io/v1/selfsubjectreviews",
            "",
        );
        let pods = path::parse("PATCH", "/api/v1/namespaces/default/pods/p1", "");

        assert!(is_authorization_review(&sar));
        assert!(is_authorization_review(&self_review));
        assert!(!is_authorization_review(&pods));
    }

    #[test]
    fn an_authorization_webhook_allow_short_circuits_local_resource_authorization() {
        let pod = path::parse("GET", "/api/v1/namespaces/default/pods/p1", "");
        let healthz = path::parse("GET", "/healthz", "");
        let review = path::parse(
            "POST",
            "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews",
            "",
        );

        assert!(!should_run_local_authorization(&pod, true, true));
        assert!(should_run_local_authorization(&pod, true, false));
        assert!(should_run_local_authorization(&healthz, true, false));
        assert!(!should_run_local_authorization(&healthz, true, true));
        assert!(!should_run_local_authorization(&review, true, false));
        assert!(!should_run_local_authorization(&pod, false, false));
    }

    #[test]
    fn aggregation_proxy_headers_replace_caller_supplied_identity_headers() {
        let mut incoming = http::HeaderMap::new();
        incoming.insert("X-Remote-User", "attacker".parse().unwrap());
        incoming.append("X-Remote-Group", "untrusted".parse().unwrap());
        incoming.insert("X-Remote-Extra-tenant", "untrusted".parse().unwrap());
        incoming.insert("X-Trace-Id", "trace-1".parse().unwrap());
        let identity = crate::authn::x509::Identity {
            name: "alice".to_string(),
            groups: vec!["developers".to_string(), "system:authenticated".to_string()],
            uid: Some("uid-1".to_string()),
            extra: Default::default(),
            credential_id: (
                "authentication.kubernetes.io/credential-id".to_string(),
                vec!["X509SHA256=abc".to_string()],
            ),
        };

        let headers = aggregation_proxy_headers(&incoming, Some(&identity), true);
        assert_eq!(
            headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("x-remote-user"))
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            ["alice"]
        );
        assert_eq!(
            headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("x-remote-group"))
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            ["developers", "system:authenticated"]
        );
        assert_eq!(
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("x-remote-uid"))
                .map(|(_, value)| value.as_str()),
            Some("uid-1")
        );
        assert_eq!(
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(
                    "x-remote-extra-authentication.kubernetes.io%2Fcredential-id"
                ))
                .map(|(_, value)| value.as_str()),
            Some("X509SHA256=abc")
        );
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "x-trace-id" && value == "trace-1")
        );
        assert!(
            !headers
                .iter()
                .any(|(_, value)| value == "attacker" || value == "untrusted")
        );
    }

    #[test]
    fn aggregation_proxy_headers_strip_identity_headers_without_a_proxy_identity() {
        let mut incoming = http::HeaderMap::new();
        incoming.insert("X-Remote-User", "attacker".parse().unwrap());
        incoming.insert("X-Remote-Group", "untrusted".parse().unwrap());
        let headers = aggregation_proxy_headers(&incoming, None, false);
        assert!(headers.is_empty());
    }

    #[test]
    fn aggregation_proxy_headers_do_not_emit_an_empty_credential_extra() {
        let identity = crate::authn::x509::Identity {
            name: "system:serviceaccount:default:builder".to_string(),
            groups: vec!["system:serviceaccounts".to_string()],
            uid: None,
            extra: Default::default(),
            credential_id: (String::new(), Vec::new()),
        };
        let headers = aggregation_proxy_headers(&http::HeaderMap::new(), Some(&identity), true);
        assert_eq!(
            headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("x-remote-user"))
                .count(),
            1
        );
        assert!(
            !headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("x-remote-extra-"))
        );
    }

    #[test]
    fn connection_upgrade_requires_upgrade_token_and_header() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::CONNECTION,
            "keep-alive, Upgrade".parse().unwrap(),
        );
        headers.insert(http::header::UPGRADE, "websocket".parse().unwrap());
        assert!(is_connection_upgrade(&headers));

        headers.insert(http::header::CONNECTION, "keep-alive".parse().unwrap());
        assert!(!is_connection_upgrade(&headers));

        headers.insert(http::header::CONNECTION, "Upgrade".parse().unwrap());
        headers.remove(http::header::UPGRADE);
        assert!(!is_connection_upgrade(&headers));
    }

    #[test]
    fn delete_preconditions_decode_resource_version_and_uid() {
        let value = serde_json::json!({"preconditions": {"resourceVersion": "7", "uid": "abc"}});
        assert_eq!(
            delete_preconditions(Some(&value)).unwrap(),
            Some(rest::DeletePreconditions {
                resource_version: Some("7".to_string()),
                uid: Some("abc".to_string())
            })
        );
    }

    #[test]
    fn delete_grace_period_decodes_non_negative_integer_values() {
        assert_eq!(delete_grace_period(None).unwrap(), None);
        assert_eq!(delete_grace_period(Some(&serde_json::json!({}))).unwrap(), None);
        assert_eq!(
            delete_grace_period(Some(&serde_json::json!({"gracePeriodSeconds": 3}))).unwrap(),
            Some(3)
        );
        assert!(delete_grace_period(Some(&serde_json::json!({"gracePeriodSeconds": -1}))).is_err());
        assert!(delete_grace_period(Some(&serde_json::json!({"gracePeriodSeconds": "3"}))).is_err());
    }

    #[test]
    fn invalid_status_joins_every_violation_into_the_message() {
        let status = invalid_status(
            "/api/v1/pods",
            &[
                "spec.containers: Required value".to_string(),
                "spec.foo: expected type string, got number".to_string(),
            ],
        );
        assert_eq!(status["kind"], "Status");
        assert_eq!(status["reason"], "Invalid");
        assert_eq!(status["code"], 422);
        let message = status["message"].as_str().unwrap();
        assert!(message.contains("spec.containers: Required value"));
        assert!(message.contains("spec.foo: expected type string, got number"));
        assert_eq!(status["details"]["causes"][0]["field"], "spec.containers");
        assert_eq!(
            status["details"]["causes"][0]["reason"],
            "FieldValueRequired"
        );
        assert_eq!(status["details"]["causes"][1]["field"], "spec.foo");
        assert_eq!(
            status["details"]["causes"][1]["reason"],
            "FieldValueInvalid"
        );
    }

    #[test]
    fn resource_expired_status_uses_the_real_gone_shape() {
        let status = resource_expired_status("/api/v1/watch/pods");
        assert_eq!(status["kind"], "Status");
        assert_eq!(status["reason"], "Gone");
        assert_eq!(status["code"], 410);
    }

    #[test]
    fn resource_version_query_reads_the_real_param() {
        assert_eq!(resource_version_query("resourceVersion=42"), 42);
        assert_eq!(
            resource_version_query("watch=true&resourceVersion=7&timeoutSeconds=30"),
            7
        );
    }

    #[test]
    fn resource_version_query_defaults_to_zero_when_absent_or_unparsable() {
        assert_eq!(resource_version_query(""), 0);
        assert_eq!(resource_version_query("watch=true"), 0);
        assert_eq!(resource_version_query("resourceVersion=not-a-number"), 0);
    }

    #[test]
    fn resource_version_query_handles_a_percent_encoded_value() {
        // Real clients never percent-encode a bare integer, but some
        // generic HTTP tooling does anyway — defensive, not a case real
        // kubectl/client-go traffic would ever hit.
        assert_eq!(resource_version_query("resourceVersion=%34%32"), 42);
    }

    #[test]
    fn watch_options_parse_bookmarks_and_timeout() {
        let options = watch_options_query(
            "watch=true&allowWatchBookmarks=true&sendInitialEvents=true&timeoutSeconds=7",
        )
        .unwrap();
        assert!(options.allow_watch_bookmarks);
        assert!(options.send_initial_events);
        assert_eq!(options.timeout, Some(std::time::Duration::from_secs(7)));
        assert_eq!(
            watch_options_query("allowWatchBookmarks=0&sendInitialEvents=0&timeoutSeconds=0")
                .unwrap(),
            WatchOptions::default()
        );
        assert!(watch_options_query("allowWatchBookmarks=maybe").is_err());
        assert!(watch_options_query("sendInitialEvents=maybe").is_err());
        assert!(watch_options_query("timeoutSeconds=-1").is_err());
        assert!(watch_options_query("timeoutSeconds=not-a-number").is_err());
    }

    #[test]
    fn is_apply_patch_content_type_recognizes_the_real_media_type_and_ignores_charset() {
        assert!(is_apply_patch_content_type("application/apply-patch+yaml"));
        assert!(is_apply_patch_content_type(
            "application/apply-patch+yaml; charset=utf-8"
        ));
        assert!(!is_apply_patch_content_type(
            "application/strategic-merge-patch+json"
        ));
        assert!(!is_apply_patch_content_type(""));
    }

    #[test]
    fn field_manager_query_reads_the_real_param() {
        assert_eq!(
            field_manager_query("fieldManager=kubectl-apply"),
            Some("kubectl-apply".to_string())
        );
        assert_eq!(
            field_manager_query("force=true&fieldManager=kubectl-apply"),
            Some("kubectl-apply".to_string())
        );
        assert_eq!(field_manager_query(""), None);
        assert_eq!(field_manager_query("force=true"), None);
    }

    #[test]
    fn force_query_reads_the_real_param() {
        assert!(force_query("force=true"));
        assert!(force_query("fieldManager=x&force=true"));
        assert!(!force_query(""));
        assert!(!force_query("force=false"));
        assert!(!force_query("force=1"));
    }

    #[test]
    fn ssa_conflict_status_names_every_conflicting_manager() {
        let mut fields = crate::patch::fieldset::Set::new();
        fields.insert(&[crate::patch::fieldset::PathElement::Field(
            "replicas".to_string(),
        )]);
        let conflicts = vec![crate::patch::updater::Conflict {
            manager: "hpa-controller".to_string(),
            fields,
        }];
        let status = ssa_conflict_status(
            "/apis/apps/v1/namespaces/default/deployments/my-app",
            &conflicts,
        );
        assert_eq!(status["code"], 409);
        assert_eq!(status["reason"], "Conflict");
        assert!(
            status["message"]
                .as_str()
                .unwrap()
                .contains("hpa-controller")
        );
    }

    #[test]
    fn encode_watch_event_produces_a_newline_terminated_json_line() {
        let event = crate::cacher::store::WatchEvent {
            kind: crate::cacher::store::EventKind::Bookmark,
            key: Vec::new(),
            value: Vec::new(),
            revision: 9,
        };
        let frame = encode_watch_event(&event, "Pod", "v1", None, "", "pods", "v1", false, false)
            .expect("Bookmark always converts")
            .expect("Bookmark conversion never fails");
        let bytes = frame.into_data().unwrap();
        assert!(bytes.ends_with(b"\n"));
        let parsed: serde_json::Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(parsed["type"], "BOOKMARK");
        assert_eq!(parsed["object"]["kind"], "Pod");
    }

    #[test]
    fn encode_watch_event_marks_the_end_of_streaming_list_initial_events() {
        let event = crate::cacher::store::WatchEvent {
            kind: crate::cacher::store::EventKind::Bookmark,
            key: Vec::new(),
            value: Vec::new(),
            revision: 9,
        };
        let frame = encode_watch_event(&event, "Pod", "v1", None, "", "pods", "v1", false, true)
            .expect("Bookmark always converts")
            .expect("Bookmark conversion never fails");
        let bytes = frame.into_data().unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(parsed["type"], "BOOKMARK");
        assert_eq!(
            parsed["object"]["metadata"]["annotations"]["k8s.io/initial-events-end"],
            "true"
        );
    }

    #[test]
    fn encode_watch_event_skips_a_deleted_event_with_no_retained_value() {
        let event = crate::cacher::store::WatchEvent {
            kind: crate::cacher::store::EventKind::Deleted,
            key: b"k".to_vec(),
            value: Vec::new(),
            revision: 9,
        };
        assert!(
            encode_watch_event(&event, "Pod", "v1", None, "", "pods", "v1", false, false).is_none()
        );
    }

    #[test]
    fn encode_watch_event_converts_objects_to_partial_metadata_when_requested() {
        let event = crate::cacher::store::WatchEvent {
            kind: crate::cacher::store::EventKind::Added,
            key: b"k".to_vec(),
            value: envelope_for("default", serde_json::json!({"app": "demo"})),
            revision: 9,
        };
        let frame = encode_watch_event(
            &event,
            "Namespace",
            "v1",
            None,
            "",
            "namespaces",
            "v1",
            true,
            false,
        )
        .expect("Added events always convert")
        .expect("the test envelope must decode");
        let bytes = frame.into_data().unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(parsed["object"]["apiVersion"], "meta.k8s.io/v1");
        assert_eq!(parsed["object"]["kind"], "PartialObjectMetadata");
        assert_eq!(parsed["object"]["metadata"]["name"], "default");
        assert!(parsed["object"].get("spec").is_none());
    }

    fn envelope_for(name: &str, labels: serde_json::Value) -> Vec<u8> {
        let schema = crate::codec::protobuf::schema_for_gvk("", "v1", "Namespace").unwrap();
        let object_bytes = crate::codec::protobuf::encode_message(
            schema,
            &serde_json::json!({"metadata": {"name": name, "labels": labels}}),
        )
        .unwrap();
        crate::codec::protobuf::wrap_unknown("v1", "Namespace", &object_bytes)
    }

    #[test]
    fn watch_event_matches_selector_passes_bookmarks_and_valueless_events_through() {
        let reqs = crate::cacher::selector::parse_label_selector("env=prod").unwrap();
        let bookmark = crate::cacher::store::WatchEvent {
            kind: crate::cacher::store::EventKind::Bookmark,
            key: Vec::new(),
            value: Vec::new(),
            revision: 1,
        };
        assert!(watch_event_matches_selector(
            &bookmark,
            &reqs,
            &[],
            None,
            "",
            ""
        ));
    }

    #[test]
    fn watch_event_matches_selector_filters_on_labels() {
        let reqs = crate::cacher::selector::parse_label_selector("env=prod").unwrap();
        let matching = crate::cacher::store::WatchEvent {
            kind: crate::cacher::store::EventKind::Added,
            key: b"a".to_vec(),
            value: envelope_for("a", serde_json::json!({"env": "prod"})),
            revision: 1,
        };
        let non_matching = crate::cacher::store::WatchEvent {
            kind: crate::cacher::store::EventKind::Added,
            key: b"b".to_vec(),
            value: envelope_for("b", serde_json::json!({"env": "dev"})),
            revision: 2,
        };
        assert!(watch_event_matches_selector(
            &matching,
            &reqs,
            &[],
            None,
            "",
            ""
        ));
        assert!(!watch_event_matches_selector(
            &non_matching,
            &reqs,
            &[],
            None,
            "",
            ""
        ));
    }

    #[test]
    fn watch_event_matches_selector_is_a_no_op_with_no_selector() {
        let event = crate::cacher::store::WatchEvent {
            kind: crate::cacher::store::EventKind::Added,
            key: b"a".to_vec(),
            value: envelope_for("a", serde_json::json!({})),
            revision: 1,
        };
        assert!(watch_event_matches_selector(&event, &[], &[], None, "", ""));
    }

    include!("listener_tests/watch.rs");

    fn test_peer() -> SocketAddr {
        "10.0.0.7:54321".parse().unwrap()
    }

    #[test]
    fn build_audit_event_carries_the_real_request_shape_for_an_anonymous_user() {
        let event = build_audit_event(
            "GET",
            "/api/v1/namespaces/default/pods/web-1",
            "",
            None,
            None,
            &test_peer(),
            200,
            &BTreeMap::new(),
        );
        assert_eq!(event["verb"], "get");
        assert_eq!(event["user"]["username"], "system:anonymous");
        assert_eq!(event["responseStatus"]["code"], 200);
        assert_eq!(event["sourceIPs"], serde_json::json!(["10.0.0.7"]));
        assert_eq!(event["objectRef"]["resource"], "pods");
        assert_eq!(event["objectRef"]["namespace"], "default");
        assert_eq!(event["objectRef"]["name"], "web-1");
    }

    #[test]
    fn build_audit_event_carries_the_real_identity_when_present() {
        let identity = crate::authn::x509::Identity {
            name: "alice".to_string(),
            groups: vec!["developers".to_string()],
            uid: None,
            extra: Default::default(),
            credential_id: (String::new(), Vec::new()),
        };
        let event = build_audit_event(
            "GET",
            "/api/v1/pods",
            "watch=true",
            None,
            Some(&identity),
            &test_peer(),
            200,
            &BTreeMap::new(),
        );
        assert_eq!(event["user"]["username"], "alice");
        assert_eq!(event["user"]["groups"], serde_json::json!(["developers"]));
        assert_eq!(event["verb"], "watch");
        assert_eq!(event["requestURI"], "/api/v1/pods?watch=true");
    }

    #[test]
    fn build_audit_event_has_no_object_ref_for_a_non_resource_request() {
        let event = build_audit_event(
            "GET",
            "/version",
            "",
            None,
            None,
            &test_peer(),
            200,
            &BTreeMap::new(),
        );
        assert!(event.get("objectRef").is_none());
    }

    #[test]
    fn build_audit_event_carries_a_denied_response_code() {
        let event = build_audit_event(
            "DELETE",
            "/api/v1/namespaces/default/pods/web-1",
            "",
            None,
            None,
            &test_peer(),
            403,
            &BTreeMap::new(),
        );
        assert_eq!(event["responseStatus"]["code"], 403);
    }

    #[test]
    fn rejected_requests_are_written_to_the_audit_sink_without_a_policy() {
        let path = std::env::temp_dir().join(format!(
            "nodeapiserver-audit-rejected-{}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let sink = crate::audit::sink::AuditSink::open(&path).unwrap();
        let info = path::parse("GET", "/version", "");
        log_audit_rejected_request(
            "audit-id",
            &info,
            "GET",
            "/version",
            "",
            None,
            None,
            &test_peer(),
            401,
            Some(&sink),
            None,
        );

        let content = std::fs::read_to_string(&path).unwrap();
        let events: Vec<serde_json::Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["auditID"], "audit-id");
        assert_eq!(events[0]["stage"], "ResponseComplete");
        assert_eq!(events[0]["responseStatus"]["code"], 401);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn long_running_requests_are_not_logged_as_response_complete() {
        assert!(is_long_running_request(
            &path::parse("GET", "/api/v1/pods", "watch=true"),
            "watch=true"
        ));
        assert!(is_long_running_request(
            &path::parse("POST", "/api/v1/namespaces/default/pods/web/exec", ""),
            ""
        ));
        assert!(is_long_running_request(
            &path::parse(
                "GET",
                "/api/v1/namespaces/default/pods/web/log",
                "follow=true"
            ),
            "follow=true"
        ));
        assert!(!is_long_running_request(
            &path::parse(
                "GET",
                "/api/v1/namespaces/default/pods/web/log",
                "follow=false"
            ),
            "follow=false"
        ));
    }

    #[test]
    fn staged_audit_events_share_the_request_audit_id() {
        let audit_id = "11111111-1111-1111-1111-111111111111";
        let received = build_audit_event_at_stage(
            audit_id,
            crate::audit::event::STAGE_REQUEST_RECEIVED,
            "GET",
            "/api/v1/pods",
            "watch=true",
            None,
            None,
            &test_peer(),
            0,
            &BTreeMap::new(),
        );
        let started = build_audit_event_at_stage(
            audit_id,
            crate::audit::event::STAGE_RESPONSE_STARTED,
            "GET",
            "/api/v1/pods",
            "watch=true",
            None,
            None,
            &test_peer(),
            200,
            &BTreeMap::new(),
        );
        assert_eq!(received["auditID"], started["auditID"]);
        assert_eq!(received["stage"], "RequestReceived");
        assert_eq!(started["stage"], "ResponseStarted");
        assert_eq!(started["responseStatus"]["code"], 200);
    }

    #[test]
    fn admission_warnings_use_warning_code_299_and_are_header_safe() {
        let mut response = Response::new(body_from_bytes(Vec::new()));
        apply_admission_warnings(&mut response, &["policy \"failed\"\nnext".to_string()]);
        assert_eq!(
            response.headers().get("warning").unwrap(),
            "299 - \"policy \\\"failed\\\" next\""
        );
    }

    #[test]
    fn proxy_suffix_supports_the_normal_subresource_form() {
        let info = path::parse(
            "GET",
            "/api/v1/namespaces/default/services/web:http/proxy/healthz",
            "",
        );
        assert_eq!(info.resource, "services");
        assert_eq!(info.name, "web:http");
        assert_eq!(info.subresource, "proxy");
        assert_eq!(proxy_suffix(&info), "/healthz");
    }

    #[test]
    fn proxy_suffix_supports_the_legacy_proxy_prefix_form() {
        let info = path::parse("GET", "/api/v1/proxy/nodes/node-a/stats/summary", "");
        assert_eq!(info.verb, "proxy");
        assert_eq!(info.resource, "nodes");
        assert_eq!(info.name, "node-a");
        assert_eq!(proxy_suffix(&info), "/stats/summary");
    }
}
