    use super::*;
    use serde_json::json;

    /// The real bug that motivated `is_inline_embedded_field`: found live
    /// via a `ValidatingAdmissionPolicy` round trip against a real
    /// `nodestore` -- `apiGroups`/`apiVersions`/`resources`/`operations`/
    /// `scope` all silently vanished on write because they belong to two
    /// levels of real upstream Go struct embedding
    /// (`NamedRuleWithOperations` -> `RuleWithOperations` -> `Rule`) that
    /// JSON flattens but the vendored proto keeps as nested message
    /// fields with no JSON key of their own.
    #[test]
    fn named_rule_with_operations_round_trips_its_doubly_embedded_fields() {
        let message = "io.k8s.api.admissionregistration.v1.NamedRuleWithOperations";
        let value = json!({
            "resourceNames": ["a", "b"],
            "operations": ["CREATE", "UPDATE"],
            "apiGroups": ["apps"],
            "apiVersions": ["v1"],
            "resources": ["deployments"],
            "scope": "Namespaced",
        });
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded, value);
    }

    /// The second real bug the same live round trip caught, right after the
    /// first: `Validation.Expression` and `Variable.Name`/`.Expression` are
    /// capitalized in the vendored proto (a go-to-protobuf quirk, see
    /// `real_json_name_override`'s own doc comment in `build/proto_parse.rs`)
    /// but real upstream's actual JSON keys are lowercase -- `update` failed
    /// validation with "spec.validations[0].expression: Required value"
    /// because the codec wrote the object with a capitalized key nothing
    /// downstream recognized.
    #[test]
    fn validation_and_variable_round_trip_their_real_lowercase_json_keys() {
        let validation = "io.k8s.api.admissionregistration.v1.Validation";
        let value = json!({
            "expression": "object.spec.replicas <= 5",
            "message": "too many replicas",
        });
        let encoded = encode_message(validation, &value).unwrap();
        let decoded = decode_message(validation, &encoded).unwrap();
        assert_eq!(decoded, value);

        let variable = "io.k8s.api.admissionregistration.v1.Variable";
        let value = json!({
            "name": "replicas",
            "expression": "object.spec.replicas",
        });
        let encoded = encode_message(variable, &value).unwrap();
        let decoded = decode_message(variable, &encoded).unwrap();
        assert_eq!(decoded, value);
    }

    /// Same class of bug as above, on `MutatingWebhookConfiguration`'s and
    /// `ValidatingWebhookConfiguration`'s own `Webhooks` field -- real
    /// upstream's JSON key is lowercase `webhooks`. Uses a non-empty
    /// webhooks list: an empty `repeated` field genuinely produces no wire
    /// bytes at all (true of every `repeated` field in this codec, not
    /// specific to this bug), so it can never round-trip as a present-but-
    /// empty array -- that's not what this test is checking.
    #[test]
    fn validating_webhook_configuration_round_trips_lowercase_webhooks_key() {
        let message = "io.k8s.api.admissionregistration.v1.ValidatingWebhookConfiguration";
        let value = json!({
            "metadata": {"name": "my-config"},
            "webhooks": [{"name": "my-webhook.example.com"}],
        });
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn pod_volume_round_trips_its_flattened_volume_source() {
        let message = "io.k8s.api.core.v1.Volume";
        let value = json!({
            "name": "config-volume",
            "configMap": {
                "name": "coredns",
                "items": [{"key": "Corefile", "path": "Corefile"}],
            },
        });
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn a_simple_object_meta_round_trips() {
        let message = "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta";
        let value = json!({
            "name": "my-pod",
            "namespace": "default",
            "uid": "abc-123",
            "generation": 5,
            "labels": {"app": "web", "tier": "frontend"},
        });
        let encoded = encode_message(message, &value).unwrap();
        assert!(!encoded.is_empty());
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded.get("name").unwrap(), "my-pod");
        assert_eq!(decoded.get("namespace").unwrap(), "default");
        assert_eq!(decoded.get("uid").unwrap(), "abc-123");
        assert_eq!(decoded.get("generation").unwrap(), &json!(5));
        assert_eq!(decoded.get("labels").unwrap(), &json!({"app": "web", "tier": "frontend"}));
    }

    /// Found live: nothing exercised `creationTimestamp` through a real
    /// protobuf round trip before `tests/encryption_roundtrip.rs`'s own
    /// live datastore round trip did — every `rest::create`/`update`
    /// call sets this field, so this was a real, previously-undiscovered
    /// bug blocking every single object this crate ever persisted to a
    /// real nodestore, silently uncaught because no prior test (unit or
    /// otherwise) happened to send an `ObjectMeta` through
    /// `encode_message` with this field actually populated.
    #[test]
    fn object_meta_with_a_real_creation_timestamp_round_trips() {
        let message = "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta";
        let value = json!({"name": "my-pod", "creationTimestamp": "2024-01-15T10:30:00Z"});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["creationTimestamp"], "2024-01-15T10:30:00Z");
    }

    #[test]
    fn time_message_round_trips_through_the_real_seconds_nanos_wire_shape() {
        let bytes = encode_time_string("Time", "2024-01-15T10:30:00Z").unwrap();
        // Not empty and not a bare string on the wire -- proving this
        // really did take the {seconds, nanos} message path, not fall
        // back to string encoding.
        assert!(!bytes.is_empty());
        let decoded = decode_time_message("Time", &bytes).unwrap();
        assert_eq!(decoded, json!("2024-01-15T10:30:00Z"));
    }

    /// Real bug, found live: `tests/aggregator_proxy_roundtrip.rs`'s own
    /// live `Service.spec.ports[].targetPort: 8443` (a plain JSON number)
    /// failed to decode with `NotAnObject("...IntOrString")` -- nothing
    /// in this codec had a special case for `intstr.IntOrString` at all
    /// before this fix, so `encode_field`'s generic `encode_message`
    /// fallback tried (and `decode_one`'s `decode_message` fallback would
    /// have tried) to treat the plain scalar as a fields-shaped object.
    #[test]
    fn service_port_with_a_real_numeric_target_port_round_trips() {
        let message = "io.k8s.api.core.v1.ServicePort";
        let value = json!({"name": "https", "port": 443, "targetPort": 8443});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["targetPort"], json!(8443));
    }

    #[test]
    fn service_port_with_a_real_named_target_port_round_trips() {
        let message = "io.k8s.api.core.v1.ServicePort";
        let value = json!({"name": "https", "port": 443, "targetPort": "https"});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["targetPort"], json!("https"));
    }

    #[test]
    fn int_or_string_encodes_a_number_with_no_type_tag_written() {
        // type == 0 (Int) is proto2 optional's own zero value -- omitted
        // on the wire, same "don't write an explicit zero" posture every
        // other optional scalar field in this codec already takes.
        let bytes = encode_int_or_string("ServicePort", &ProtoField { message: "ServicePort", json_name: "targetPort", number: 4, repeated: false, map: false, proto_type: "k8s.io.apimachinery.pkg.util.intstr.IntOrString" }, &json!(8443)).unwrap();
        let decoded = decode_int_or_string_message("targetPort", &bytes).unwrap();
        assert_eq!(decoded, json!(8443));
    }

    #[test]
    fn int_or_string_rejects_a_value_that_is_neither_int_nor_string() {
        let field = ProtoField { message: "ServicePort", json_name: "targetPort", number: 4, repeated: false, map: false, proto_type: "k8s.io.apimachinery.pkg.util.intstr.IntOrString" };
        assert!(encode_int_or_string("ServicePort", &field, &json!(true)).is_err());
    }

    /// Found via a deliberate audit pass, not a live failure this time —
    /// `is_quantity_message`'s own doc comment covers why this needed the
    /// same special case as `IntOrString` before it.
    #[test]
    fn resource_field_selector_with_a_real_quantity_divisor_round_trips() {
        let message = "io.k8s.api.core.v1.ResourceFieldSelector";
        let value = json!({"resource": "limits.cpu", "divisor": "100m"});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["divisor"], json!("100m"));
    }

    #[test]
    fn quantity_round_trips_a_real_binary_si_value() {
        let field = ProtoField { message: "ResourceFieldSelector", json_name: "divisor", number: 3, repeated: false, map: false, proto_type: "k8s.io.apimachinery.pkg.api.resource.Quantity" };
        let bytes = encode_quantity("ResourceFieldSelector", &field, &json!("1.5Gi")).unwrap();
        assert_eq!(decode_quantity_message("divisor", &bytes).unwrap(), json!("1.5Gi"));
    }

    /// `runtime.RawExtension` shares `apiextensions.v1.JSON`'s exact wire
    /// shape and JSON semantics -- `is_json_message`'s own doc comment
    /// covers why. Verified against a real message that actually embeds
    /// one: `admissionreview.k8s.io/v1`'s own request/response carries
    /// `object`/`oldObject` as `runtime.RawExtension` upstream, though
    /// this crate doesn't have `AdmissionReview` compiled yet -- proven
    /// here with a synthetic message name instead, exercising exactly the
    /// same `encode_json_value`/`decode_json_message` code path either
    /// way (both are driven purely by `is_json_message`'s own match, not
    /// by anything specific to which real message uses it).
    #[test]
    fn raw_extension_is_recognized_by_the_same_json_wrapper_special_case() {
        assert!(is_json_message("io.k8s.apimachinery.pkg.runtime.RawExtension"));
        let encoded = encode_json_value(&json!({"kind": "PluginA", "aOption": "foo"}));
        let decoded = decode_json_message("raw", &encoded).unwrap();
        assert_eq!(decoded, json!({"kind": "PluginA", "aOption": "foo"}));
    }

    #[test]
    fn time_message_handles_the_unix_epoch_with_no_fields_written() {
        // seconds == 0 and nanos == 0 are both real proto2 "optional"
        // defaults -- neither field gets written on the wire at all,
        // matching every other scalar field's own "absent means default"
        // convention this codec already established elsewhere.
        let bytes = encode_time_string("Time", "1970-01-01T00:00:00Z").unwrap();
        assert!(bytes.is_empty());
        let decoded = decode_time_message("Time", &bytes).unwrap();
        assert_eq!(decoded, json!("1970-01-01T00:00:00Z"));
    }

    #[test]
    fn encode_time_string_rejects_a_non_rfc3339_value() {
        assert!(encode_time_string("Time", "not a timestamp").is_err());
    }

    /// Found live, the same way `object_meta_with_a_real_creation_
    /// timestamp_round_trips` was: nothing exercised a real
    /// `CustomResourceDefinition`'s own schema `default` through
    /// `encode_message`/`decode_message` until `tests/crd_roundtrip.rs`'s
    /// live round trip did, and it hit exactly this same class of bug —
    /// `JSONSchemaProps.default` is an `apiextensions.v1.JSON` message,
    /// not a plain scalar, and this codec had no special case for it yet.
    #[test]
    fn json_schema_props_default_round_trips_through_the_real_json_wire_shape() {
        let message = "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaProps";
        let value = json!({"type": "string", "default": "small"});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["default"], "small");
        assert_eq!(decoded["type"], "string");
    }

    #[test]
    fn json_message_round_trips_a_non_scalar_default() {
        let bytes = encode_json_value(&json!({"a": 1, "b": [true, null]}));
        assert!(!bytes.is_empty());
        let decoded = decode_json_message("default", &bytes).unwrap();
        assert_eq!(decoded, json!({"a": 1, "b": [true, null]}));
    }

    #[test]
    fn json_message_with_no_raw_field_decodes_to_null() {
        assert_eq!(decode_json_message("default", &[]).unwrap(), Value::Null);
    }

    /// The real bug this whole trio of helpers exists to fix: `items` on
    /// a real `JSONSchemaProps` (used by every CRD list field) round
    /// trips as the plain, unwrapped schema object real JSON uses — not
    /// the `JSONSchemaPropsOrArray` wrapper shape. Found live: a CRD's
    /// own `items` schema (and therefore every field inside it,
    /// including `x-kubernetes-list-map-keys` on a nested list) silently
    /// decoded to an empty object on every real read until
    /// `tests/crd_roundtrip.rs`'s own strategic-merge-patch test actually
    /// recursed into one.
    #[test]
    fn json_schema_props_items_round_trips_as_a_single_unwrapped_schema() {
        let message = "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaProps";
        let value = json!({"type": "array", "items": {"type": "object", "properties": {"name": {"type": "string"}}}});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["items"], json!({"type": "object", "properties": {"name": {"type": "string"}}}));
    }

    #[test]
    fn json_schema_props_items_round_trips_as_an_array_of_schemas() {
        let message = "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaProps";
        let value = json!({"type": "array", "items": [{"type": "string"}, {"type": "integer"}]});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["items"], json!([{"type": "string"}, {"type": "integer"}]));
    }

    #[test]
    fn json_schema_props_additional_properties_round_trips_a_bare_bool() {
        let message = "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaProps";
        let value = json!({"type": "object", "additionalProperties": true});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["additionalProperties"], json!(true));
    }

    #[test]
    fn json_schema_props_additional_properties_false_round_trips() {
        // `allows: false` is proto2's own zero value for that inner
        // scalar (never written inside the JSONSchemaPropsOrBool
        // submessage itself), but the *outer* additionalProperties field
        // is still present (an empty submessage, not an absent one) --
        // decode must still come back `false`, not `null` or `true`.
        let message = "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaProps";
        let value = json!({"type": "object", "additionalProperties": false});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["additionalProperties"], json!(false));
    }

    #[test]
    fn an_absent_additional_properties_key_stays_absent_after_a_round_trip() {
        let message = "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaProps";
        let value = json!({"type": "object"});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded.get("additionalProperties"), None, "a key the client never submitted at all must not appear after a round trip");
    }

    #[test]
    fn json_schema_props_additional_properties_round_trips_a_nested_schema() {
        let message = "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaProps";
        let value = json!({"type": "object", "additionalProperties": {"type": "string"}});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded["additionalProperties"], json!({"type": "string"}));
    }

    #[test]
    fn a_nested_message_field_round_trips() {
        // ListMeta has no nested message fields; ObjectMeta does not
        // either in a simple way, so exercise a real cross-package
        // reference: DaemonSetSpec.selector -> LabelSelector.
        let message = "io.k8s.api.apps.v1.DaemonSetSpec";
        let value = json!({
            "selector": {"matchLabels": {"app": "web"}},
            "minReadySeconds": 30,
        });
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded.get("selector").unwrap(), &json!({"matchLabels": {"app": "web"}}));
        assert_eq!(decoded.get("minReadySeconds").unwrap(), &json!(30));
    }

    #[test]
    fn a_repeated_scalar_field_round_trips_as_an_array() {
        // ServiceAccount.imagePullSecrets is repeated
        // LocalObjectReference (message), but PodSpec.hostAliases's IPs
        // are a plainer example — use Container.args, repeated string.
        let message = "io.k8s.api.core.v1.Container";
        let value = json!({"name": "app", "args": ["--flag", "value"]});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded.get("args").unwrap(), &json!(["--flag", "value"]));
    }

    #[test]
    fn a_repeated_message_field_round_trips() {
        let message = "io.k8s.api.core.v1.PodSpec";
        let value = json!({
            "containers": [
                {"name": "a", "image": "nginx"},
                {"name": "b", "image": "redis"},
            ],
        });
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        let containers = decoded.get("containers").unwrap().as_array().unwrap();
        assert_eq!(containers.len(), 2);
        assert_eq!(containers[0].get("name").unwrap(), "a");
        assert_eq!(containers[1].get("image").unwrap(), "redis");
    }

    #[test]
    fn a_bytes_field_round_trips_through_base64() {
        // ConfigMap.binaryData is map<string, bytes>; Secret.data too.
        // Use Secret.data to exercise both bytes and map in one field.
        let message = "io.k8s.api.core.v1.Secret";
        let value = json!({"data": {"password": base64_encode(b"hunter2")}});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        let got = decoded.get("data").unwrap().get("password").unwrap().as_str().unwrap();
        assert_eq!(base64_decode(got).unwrap(), b"hunter2");
    }

    #[test]
    fn a_map_string_string_field_round_trips() {
        let message = "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta";
        let value = json!({"annotations": {"a": "1", "b": "2", "c": "3"}});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded.get("annotations").unwrap(), &json!({"a": "1", "b": "2", "c": "3"}));
    }

    #[test]
    fn a_negative_int64_round_trips() {
        // ObjectMeta has no signed-negative-friendly field; use
        // DeleteOptions.gracePeriodSeconds (int64) is not on ObjectMeta —
        // use PodSpec.terminationGracePeriodSeconds (int64) instead, which
        // can legitimately be negative in upstream's own semantics is not
        // true, but the wire format must still round-trip a negative value
        // correctly regardless of whether real callers send one.
        let message = "io.k8s.api.core.v1.PodSpec";
        let value = json!({"terminationGracePeriodSeconds": -1});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert_eq!(decoded.get("terminationGracePeriodSeconds").unwrap(), &json!(-1));
    }

    #[test]
    fn null_fields_are_omitted_not_encoded_as_present() {
        let message = "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta";
        let value = json!({"name": "x", "namespace": null});
        let encoded = encode_message(message, &value).unwrap();
        let decoded = decode_message(message, &encoded).unwrap();
        assert!(decoded.get("namespace").is_none(), "a null field must not round-trip as present");
    }

    #[test]
    fn unknown_json_fields_are_skipped_not_errors() {
        let message = "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta";
        let value = json!({"name": "x", "totallyMadeUpField": "whatever"});
        // Must not error — forward/backward compatibility with a differently
        // versioned client is the whole point of tolerating unknown fields.
        encode_message(message, &value).unwrap();
    }

    #[test]
    fn the_envelope_round_trips_api_version_kind_and_body() {
        let object = json!({"metadata": {"name": "my-pod"}});
        let body = encode_message("io.k8s.api.core.v1.Pod", &object).unwrap();
        let wrapped = wrap_unknown("v1", "Pod", &body);
        assert_eq!(&wrapped[..MAGIC.len()], &MAGIC);

        let (api_version, kind, object_bytes) = unwrap_unknown(&wrapped).unwrap();
        assert_eq!(api_version, "v1");
        assert_eq!(kind, "Pod");
        let decoded = decode_message("io.k8s.api.core.v1.Pod", &object_bytes).unwrap();
        assert_eq!(decoded.get("metadata").unwrap().get("name").unwrap(), "my-pod");
    }

    #[test]
    fn a_missing_magic_prefix_is_rejected() {
        let err = unwrap_unknown(b"not-a-k8s-payload").unwrap_err();
        assert!(matches!(err, Error::BadMagic));
    }

    #[test]
    fn schema_for_gvk_resolves_core_v1_pod() {
        assert_eq!(schema_for_gvk("", "v1", "Pod"), Some("io.k8s.api.core.v1.Pod"));
        assert_eq!(schema_for_gvk("apps", "v1", "Deployment"), Some("io.k8s.api.apps.v1.Deployment"));
    }
