//! Defaulting: fills in a JSON object's absent fields from the vendored
//! OpenAPI schema's own `"default"` values, recursively.
//!
//! # What this captures, and what it honestly doesn't
//!
//! Real upstream defaulting (`pkg/apis/core/v1/defaults.go` and friends)
//! is hand-written Go with real conditional logic — "default X to Retain
//! unless Y", defaults that depend on another field's value, defaults that
//! only apply for a particular `apiVersion`. None of that is derivable
//! from a flat per-field default value, and this module doesn't attempt
//! it. What it *does* correctly handle: every **unconditional** default —
//! a field that always gets the same default value whenever it's absent,
//! which is the majority case (`ContainerPort.protocol` defaulting to
//! `"TCP"` is a real, verified example — see `codegen`'s own test). This
//! is a real, useful subset, not a full defaulting engine; conditional
//! defaults are genuinely separate, per-type work.
//!
//! # Recursion
//!
//! A field whose `FIELD_META` entry has `ref_schema` set gets recursed
//! into *after* its own default (if any) is applied — so an absent nested
//! object first materializes as its schema's structural default (usually
//! `{}`), then gets that same schema's own field defaults filled in, all
//! in one pass. An array field recurses into each element individually
//! (matches real per-container, per-port, ... defaulting behavior) rather
//! than defaulting the array itself, since no vendored field carries a
//! meaningful default *for the array as a whole* (only its elements do).

use crate::codegen;
use crate::codegen::openapi_meta::FieldMeta;
use serde_json::{Map, Value};

/// Applies `schema`'s defaults (and every nested schema's, recursively) to
/// `value`. `value` is expected to be a JSON object shaped like `schema`;
/// anything else is returned unchanged — matches
/// `patch::strategic_merge::merge`'s same "not an object, nothing to do
/// but hand it back" posture for a mismatched type.
pub fn apply_defaults(schema: &str, value: &Value) -> Value {
    let Value::Object(obj) = value else {
        return value.clone();
    };
    let mut result = obj.clone();
    let fields: Vec<&'static FieldMeta> = codegen::openapi_meta::FIELD_META.iter().filter(|m| m.schema == schema).collect();

    fill_absent_defaults(&mut result, &fields);
    recurse_into_referenced_fields(&mut result, &fields);

    Value::Object(result)
}

/// Applies the conditional defaults from the core API types that can be
/// represented faithfully in the generic JSON layer.  OpenAPI only carries
/// unconditional field defaults, while kube-apiserver's core defaulting
/// functions also derive values from the object's kind or from sibling
/// fields.  Keeping this pass separate makes that distinction explicit and
/// prevents CRD documents from accidentally receiving built-in defaults.
pub fn apply_builtin_defaults(group: &str, version: &str, kind: &str, value: Value) -> Value {
    if group != "" || version != "v1" {
        return value;
    }

    let mut value = value;
    match kind {
        "Pod" => default_pod(&mut value),
        "Service" => default_service(&mut value),
        "Secret" => default_secret(&mut value),
        "ConfigMap" => default_config_map(&mut value),
        "PersistentVolume" => default_persistent_volume(&mut value),
        "PersistentVolumeClaim" => default_persistent_volume_claim(&mut value),
        "Endpoints" => default_endpoints(&mut value),
        "Namespace" => default_namespace(&mut value),
        "ReplicationController" => default_replication_controller(&mut value),
        "PodTemplate" => default_pod_template(&mut value),
        "DaemonSet" | "Deployment" | "ReplicaSet" | "StatefulSet" => default_workload_template(&mut value),
        _ => {}
    }
    value
}

fn object_mut<'a>(value: &'a mut Value, path: &[&str]) -> Option<&'a mut serde_json::Map<String, Value>> {
    let mut current = value;
    for part in path {
        current = current.get_mut(*part)?;
    }
    current.as_object_mut()
}

fn default_string(object: &mut serde_json::Map<String, Value>, field: &str, default: &str) {
    if object.get(field).is_none_or(|value| value.is_null() || value.as_str().is_some_and(str::is_empty)) {
        object.insert(field.to_string(), Value::String(default.to_string()));
    }
}

fn default_i64(object: &mut serde_json::Map<String, Value>, field: &str, default: i64) {
    if object.get(field).is_none_or(Value::is_null) {
        object.insert(field.to_string(), Value::Number(default.into()));
    }
}

fn default_bool(object: &mut serde_json::Map<String, Value>, field: &str, default: bool) {
    if object.get(field).is_none_or(Value::is_null) {
        object.insert(field.to_string(), Value::Bool(default));
    }
}

fn default_map(object: &mut serde_json::Map<String, Value>, field: &str) {
    if object.get(field).is_none_or(Value::is_null) {
        object.insert(field.to_string(), Value::Object(serde_json::Map::new()));
    }
}

fn default_pod(value: &mut Value) {
    let Some(object) = value.as_object_mut() else { return };
    let Some(spec) = object.get_mut("spec").and_then(Value::as_object_mut) else { return };
    default_pod_spec(spec);
    default_bool(spec, "enableServiceLinks", true);
}

fn default_pod_template(value: &mut Value) {
    let Some(object) = value.as_object_mut() else { return };
    let Some(spec) = object.get_mut("spec").and_then(Value::as_object_mut) else { return };
    default_pod_spec(spec);
}

fn default_workload_template(value: &mut Value) {
    let Some(object) = value.as_object_mut() else { return };
    let Some(spec) = object.get_mut("spec").and_then(Value::as_object_mut) else { return };
    let Some(template) = spec.get_mut("template").and_then(Value::as_object_mut) else { return };
    let Some(pod_spec) = template.get_mut("spec").and_then(Value::as_object_mut) else { return };
    default_pod_spec(pod_spec);
}

fn default_pod_spec(spec: &mut serde_json::Map<String, Value>) {
    let dns_policy = if spec.get("hostNetwork").and_then(Value::as_bool) == Some(true) {
        "ClusterFirstWithHostNet"
    } else {
        "ClusterFirst"
    };
    default_string(spec, "dnsPolicy", dns_policy);
    default_string(spec, "restartPolicy", "Always");
    default_map(spec, "securityContext");
    default_i64(spec, "terminationGracePeriodSeconds", 30);
    default_string(spec, "schedulerName", "default-scheduler");

    for field in ["containers", "initContainers", "ephemeralContainers"] {
        if let Some(items) = spec.get_mut(field).and_then(Value::as_array_mut) {
            for item in items {
                default_container(item);
            }
        }
    }
    if let Some(volumes) = spec.get_mut("volumes").and_then(Value::as_array_mut) {
        for volume in volumes {
            default_volume(volume);
        }
    }
}

fn default_container(value: &mut Value) {
    let Some(container) = value.as_object_mut() else { return };
    if container.get("imagePullPolicy").is_none_or(|value| value.is_null() || value.as_str().is_some_and(str::is_empty)) {
        let image = container.get("image").and_then(Value::as_str).unwrap_or("");
        let last = image.rsplit('/').next().unwrap_or(image);
        let tag = if last.contains('@') {
            ""
        } else if let Some((_, tag)) = last.rsplit_once(':') {
            tag
        } else {
            "latest"
        };
        container.insert(
            "imagePullPolicy".to_string(),
            Value::String(if tag == "latest" { "Always" } else { "IfNotPresent" }.to_string()),
        );
    }
    default_string(container, "terminationMessagePath", "/dev/termination-log");
    default_string(container, "terminationMessagePolicy", "File");
    for field in ["livenessProbe", "readinessProbe", "startupProbe"] {
        if let Some(probe) = container.get_mut(field).and_then(Value::as_object_mut) {
            default_zero_i64(probe, "timeoutSeconds", 1);
            default_zero_i64(probe, "periodSeconds", 10);
            default_zero_i64(probe, "successThreshold", 1);
            default_zero_i64(probe, "failureThreshold", 3);
            if let Some(http_get) = probe.get_mut("httpGet").and_then(Value::as_object_mut) {
                default_string(http_get, "path", "/");
                default_string(http_get, "scheme", "HTTP");
            }
        }
    }
    if let Some(resources) = container.get_mut("resources").and_then(Value::as_object_mut) {
        let limits = resources.get("limits").and_then(Value::as_object).cloned();
        if let Some(limits) = limits {
            if resources.get("requests").is_none_or(Value::is_null) {
                resources.insert("requests".to_string(), Value::Object(serde_json::Map::new()));
            }
            if let Some(requests) = resources.get_mut("requests").and_then(Value::as_object_mut) {
                for (name, limit) in limits {
                    requests.entry(name).or_insert(limit);
                }
            }
        }
    }
}

fn default_zero_i64(object: &mut serde_json::Map<String, Value>, field: &str, default: i64) {
    if object.get(field).is_none_or(|value| value.is_null() || value.as_i64() == Some(0)) {
        object.insert(field.to_string(), Value::Number(default.into()));
    }
}

fn default_volume(value: &mut Value) {
    let Some(volume) = value.as_object_mut() else { return };
    for source in ["secret", "configMap", "downwardAPI", "projected"] {
        if let Some(source) = volume.get_mut(source).and_then(Value::as_object_mut) {
            if source.get("defaultMode").is_none_or(Value::is_null) {
                source.insert("defaultMode".to_string(), Value::Number(420.into()));
            }
        }
    }
    if let Some(projected) = volume.get_mut("projected").and_then(Value::as_object_mut) {
        if let Some(sources) = projected.get_mut("sources").and_then(Value::as_array_mut) {
            for source in sources {
                if let Some(token) = source.get_mut("serviceAccountToken").and_then(Value::as_object_mut) {
                    default_i64(token, "expirationSeconds", 3600);
                }
            }
        }
    }
}

fn default_service(value: &mut Value) {
    let Some(spec) = object_mut(value, &["spec"]) else { return };
    default_string(spec, "sessionAffinity", "None");
    if spec.get("sessionAffinity").and_then(Value::as_str) == Some("ClientIP") {
        if spec.get("sessionAffinityConfig").is_none_or(Value::is_null) {
            spec.insert(
                "sessionAffinityConfig".to_string(),
                Value::Object(serde_json::Map::new()),
            );
        }
        if let Some(config) = spec
            .get_mut("sessionAffinityConfig")
            .and_then(Value::as_object_mut)
        {
            if config.get("clientIP").is_none_or(Value::is_null) {
                config.insert("clientIP".to_string(), Value::Object(serde_json::Map::new()));
            }
            if let Some(client_ip) = config.get_mut("clientIP").and_then(Value::as_object_mut) {
                default_i64(client_ip, "timeoutSeconds", 10800);
            }
        }
    }
    default_string(spec, "type", "ClusterIP");
    if let Some(ports) = spec.get_mut("ports").and_then(Value::as_array_mut) {
        for port in ports {
            if let Some(port) = port.as_object_mut() {
                default_string(port, "protocol", "TCP");
                if port.get("targetPort").is_none_or(|value| value.is_null() || value.as_i64() == Some(0) || value.as_str().is_some_and(str::is_empty)) {
                    if let Some(number) = port.get("port").and_then(Value::as_i64) {
                        port.insert("targetPort".to_string(), Value::Number(number.into()));
                    }
                }
            }
        }
    }
    let service_type = spec
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if ["ClusterIP", "NodePort", "LoadBalancer"].contains(&service_type.as_str())
        && spec.get("internalTrafficPolicy").is_none_or(Value::is_null)
    {
        default_string(spec, "internalTrafficPolicy", "Cluster");
    }
    if ["NodePort", "LoadBalancer"].contains(&service_type.as_str())
        && spec.get("externalTrafficPolicy").is_none_or(|value| value.is_null() || value.as_str().is_some_and(str::is_empty))
    {
        default_string(spec, "externalTrafficPolicy", "Cluster");
    }
    if service_type == "LoadBalancer" {
        default_bool(spec, "allocateLoadBalancerNodePorts", true);
    }
}

fn default_secret(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        default_string(object, "type", "Opaque");
    }
}

fn default_config_map(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        default_map(object, "data");
    }
}

fn default_persistent_volume(value: &mut Value) {
    let Some(object) = value.as_object_mut() else { return };
    let Some(spec) = object.get_mut("spec").and_then(Value::as_object_mut) else { return };
    default_string(spec, "persistentVolumeReclaimPolicy", "Retain");
    default_string(spec, "volumeMode", "Filesystem");
    if let Some(status) = object.get_mut("status").and_then(Value::as_object_mut) {
        default_string(status, "phase", "Pending");
    } else {
        object.insert("status".to_string(), serde_json::json!({"phase": "Pending"}));
    }
}

fn default_persistent_volume_claim(value: &mut Value) {
    let Some(object) = value.as_object_mut() else { return };
    if let Some(spec) = object.get_mut("spec").and_then(Value::as_object_mut) {
        default_string(spec, "volumeMode", "Filesystem");
    }
    if let Some(status) = object.get_mut("status").and_then(Value::as_object_mut) {
        default_string(status, "phase", "Pending");
    } else {
        object.insert("status".to_string(), serde_json::json!({"phase": "Pending"}));
    }
}

fn default_endpoints(value: &mut Value) {
    let Some(object) = value.as_object_mut() else { return };
    if let Some(subsets) = object.get_mut("subsets").and_then(Value::as_array_mut) {
        for subset in subsets {
            if let Some(ports) = subset.get_mut("ports").and_then(Value::as_array_mut) {
                for port in ports {
                    if let Some(port) = port.as_object_mut() {
                        default_string(port, "protocol", "TCP");
                    }
                }
            }
        }
    }
}

fn default_namespace(value: &mut Value) {
    let Some(object) = value.as_object_mut() else { return };
    let name = object
        .get("metadata")
        .and_then(|metadata| metadata.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    if let Some(name) = name {
        if let Some(metadata) = object.get_mut("metadata").and_then(Value::as_object_mut) {
            if metadata.get("labels").is_none_or(Value::is_null) {
                metadata.insert("labels".to_string(), Value::Object(serde_json::Map::new()));
            }
            if let Some(labels) = metadata.get_mut("labels").and_then(Value::as_object_mut) {
                labels
                    .entry("kubernetes.io/metadata.name")
                    .or_insert_with(|| Value::String(name));
            }
        }
    }
    if let Some(status) = object.get_mut("status").and_then(Value::as_object_mut) {
        default_string(status, "phase", "Active");
    }
    // Issue #541: real kube-apiserver's namespace strategy
    // (`PrepareForCreate`) unconditionally stamps `spec.finalizers =
    // ["kubernetes"]` on every Namespace. Without it, `server/rest/
    // delete.rs`'s `has_finalizers()` check sees an empty list on a
    // just-created namespace and deletes it immediately instead of
    // deferring for namespace-controller's two-phase
    // delete-contents-then-finalize flow -- live-reproduced as a
    // Namespace deleted right after creation racing (and losing to) this
    // missing default: the object vanished from storage before
    // namespace-controller's watch ever saw a `deletionTimestamp`, so its
    // contents (a ConfigMap in the failing test) were never cleaned up at
    // all, and a concurrent default-ServiceAccount creation hit a genuine
    // 404 against the already-gone namespace.
    if object.get("spec").is_none_or(Value::is_null) {
        object.insert("spec".to_string(), Value::Object(serde_json::Map::new()));
    }
    if let Some(spec) = object.get_mut("spec").and_then(Value::as_object_mut) {
        if spec.get("finalizers").is_none_or(Value::is_null) {
            spec.insert(
                "finalizers".to_string(),
                Value::Array(vec![Value::String("kubernetes".to_string())]),
            );
        }
    }
}

fn default_replication_controller(value: &mut Value) {
    let Some(object) = value.as_object_mut() else { return };
    if let Some(spec) = object.get_mut("spec").and_then(Value::as_object_mut) {
        if let Some(template) = spec.get_mut("template").and_then(Value::as_object_mut) {
            if let Some(pod_spec) = template.get_mut("spec").and_then(Value::as_object_mut) {
                default_pod_spec(pod_spec);
            }
        }
    }
    let Some(labels) = object
        .get("spec")
        .and_then(|spec| spec.get("template"))
        .and_then(|template| template.get("metadata"))
        .and_then(|metadata| metadata.get("labels"))
        .and_then(Value::as_object)
        .cloned()
    else {
        return;
    };
    let spec = object.entry("spec").or_insert_with(|| Value::Object(serde_json::Map::new()));
    let spec = spec.as_object_mut().expect("spec was created as an object");
    spec.entry("selector").or_insert_with(|| Value::Object(labels.clone()));
    object.entry("metadata").or_insert_with(|| Value::Object(serde_json::Map::new()));
    let metadata = object.get_mut("metadata").and_then(Value::as_object_mut).expect("metadata was created as an object");
    metadata.entry("labels").or_insert_with(|| Value::Object(labels));
}

fn fill_absent_defaults(result: &mut Map<String, Value>, fields: &[&'static FieldMeta]) {
    for meta in fields {
        if result.contains_key(meta.field) {
            continue;
        }
        let Some(default_json) = meta.default_json else { continue };
        let Ok(default_value) = serde_json::from_str::<Value>(default_json) else {
            // A malformed default in the vendored spec would be a real
            // upstream data problem, not something to panic the apiserver
            // over — skip it, same fail-open posture the rest of this
            // crate's parsers take on unexpected input.
            continue;
        };
        result.insert(meta.field.to_string(), default_value);
    }
}

fn recurse_into_referenced_fields(result: &mut Map<String, Value>, fields: &[&'static FieldMeta]) {
    for meta in fields {
        let Some(ref_schema) = meta.ref_schema else { continue };
        let Some(current) = result.get(meta.field) else { continue };
        let defaulted = match current {
            Value::Object(_) => apply_defaults(ref_schema, current),
            Value::Array(items) => Value::Array(items.iter().map(|item| apply_defaults(ref_schema, item)).collect()),
            _ => continue,
        };
        result.insert(meta.field.to_string(), defaulted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_absent_scalar_field_gets_its_real_default() {
        let value = json!({"containerPort": 8080});
        let defaulted = apply_defaults("io.k8s.api.core.v1.ContainerPort", &value);
        assert_eq!(defaulted["protocol"], json!("TCP"));
        assert_eq!(defaulted["containerPort"], json!(8080), "an already-present field must be left alone");
    }

    #[test]
    fn a_present_scalar_field_is_never_overwritten_by_its_default() {
        let value = json!({"containerPort": 8080, "protocol": "UDP"});
        let defaulted = apply_defaults("io.k8s.api.core.v1.ContainerPort", &value);
        assert_eq!(defaulted["protocol"], json!("UDP"));
    }

    #[test]
    fn a_non_object_value_is_returned_unchanged() {
        let value = json!("not an object");
        assert_eq!(apply_defaults("io.k8s.api.core.v1.ContainerPort", &value), value);
    }

    /// Proves the two-pass design actually cascades: an absent nested
    /// object first materializes from its own structural default (`{}`),
    /// then gets *that* schema's field defaults filled in — not just a
    /// bare `{}` left as-is.
    #[test]
    fn an_absent_nested_object_field_materializes_and_then_gets_its_own_defaults() {
        // Container.resources defaults to {} (verified against real
        // vendored data), and ResourceRequirements itself is not expected
        // to carry further unconditional scalar defaults — this test
        // proves the materialization half; the recursion mechanism itself
        // is proven end-to-end by the array case below, which does have a
        // real nested scalar default two levels deep.
        let value = json!({"name": "app"});
        let defaulted = apply_defaults("io.k8s.api.core.v1.Container", &value);
        assert_eq!(defaulted["resources"], json!({}), "an absent object-typed field with a {{}} default must materialize");
    }

    /// The real end-to-end case: a list field's *elements* get defaulted
    /// individually, not the list itself — proven with `Container.ports`,
    /// whose element schema (`ContainerPort`) has a real scalar default
    /// two levels down from where `apply_defaults` was first called.
    #[test]
    fn each_element_of_an_array_field_is_defaulted_individually() {
        let value = json!({
            "name": "app",
            "ports": [
                {"containerPort": 80},
                {"containerPort": 443, "protocol": "SCTP"},
            ],
        });
        let defaulted = apply_defaults("io.k8s.api.core.v1.Container", &value);
        let ports = defaulted["ports"].as_array().unwrap();
        assert_eq!(ports[0]["protocol"], json!("TCP"), "an absent element field gets its own default");
        assert_eq!(ports[1]["protocol"], json!("SCTP"), "an already-set element field is left alone");
    }

    #[test]
    fn a_schema_with_no_known_fields_returns_the_object_unchanged() {
        let value = json!({"anything": "goes"});
        assert_eq!(apply_defaults("totally.unknown.schema", &value), value);
    }

    #[test]
    fn core_pod_defaults_include_conditional_and_nested_defaults() {
        let value = json!({
            "metadata": {"name": "demo"},
            "spec": {
                "containers": [{
                    "name": "app",
                    "image": "example/app",
                    "resources": {"limits": {"cpu": "100m"}},
                    "readinessProbe": {"httpGet": {"port": 8080}}
                }],
                "volumes": [{"name": "config", "configMap": {"name": "settings"}}]
            }
        });
        let defaulted = apply_builtin_defaults("", "v1", "Pod", value);
        assert_eq!(defaulted["spec"]["dnsPolicy"], json!("ClusterFirst"));
        assert_eq!(defaulted["spec"]["restartPolicy"], json!("Always"));
        assert_eq!(defaulted["spec"]["enableServiceLinks"], json!(true));
        assert_eq!(defaulted["spec"]["terminationGracePeriodSeconds"], json!(30));
        assert_eq!(defaulted["spec"]["containers"][0]["imagePullPolicy"], json!("Always"));
        assert_eq!(defaulted["spec"]["containers"][0]["terminationMessagePath"], json!("/dev/termination-log"));
        assert_eq!(defaulted["spec"]["containers"][0]["resources"]["requests"]["cpu"], json!("100m"));
        assert_eq!(defaulted["spec"]["containers"][0]["readinessProbe"]["timeoutSeconds"], json!(1));
        assert_eq!(defaulted["spec"]["volumes"][0]["configMap"]["defaultMode"], json!(420));
    }

    #[test]
    fn core_service_defaults_derive_target_port_without_overwriting_explicit_values() {
        let value = json!({
            "metadata": {"name": "web"},
            "spec": {
                "ports": [
                    {"port": 80},
                    {"port": 443, "targetPort": "https", "protocol": "UDP"}
                ]
            }
        });
        let defaulted = apply_builtin_defaults("", "v1", "Service", value);
        assert_eq!(defaulted["spec"]["type"], json!("ClusterIP"));
        assert_eq!(defaulted["spec"]["sessionAffinity"], json!("None"));
        assert_eq!(defaulted["spec"]["internalTrafficPolicy"], json!("Cluster"));
        assert_eq!(defaulted["spec"]["ports"][0]["protocol"], json!("TCP"));
        assert_eq!(defaulted["spec"]["ports"][0]["targetPort"], json!(80));
        assert_eq!(defaulted["spec"]["ports"][1]["targetPort"], json!("https"));
        assert_eq!(defaulted["spec"]["ports"][1]["protocol"], json!("UDP"));
    }

    #[test]
    fn a_new_namespace_gets_the_kubernetes_finalizer_defaulted_onto_it() {
        // Issue #541: without this, server/rest/delete.rs's has_finalizers()
        // sees an empty list on a just-created Namespace and deletes it
        // immediately instead of deferring for namespace-controller's real
        // delete-contents-then-finalize flow -- so its contents are never
        // cleaned up at all. Real kube-apiserver's namespace strategy
        // stamps this unconditionally on create.
        let value = json!({"metadata": {"name": "example"}});
        let defaulted = apply_builtin_defaults("", "v1", "Namespace", value);
        assert_eq!(defaulted["spec"]["finalizers"], json!(["kubernetes"]));
    }

    #[test]
    fn an_explicit_namespace_finalizers_list_is_left_alone() {
        let value = json!({"metadata": {"name": "example"}, "spec": {"finalizers": []}});
        let defaulted = apply_builtin_defaults("", "v1", "Namespace", value);
        assert_eq!(defaulted["spec"]["finalizers"], json!([]), "an explicit (even empty) finalizers list must not be overwritten");
    }

    #[test]
    fn core_defaults_do_not_modify_crds_or_other_api_groups() {
        let value = json!({"spec": {"containers": [{"image": "example/app"}]} });
        assert_eq!(apply_builtin_defaults("apps", "v1", "Deployment", value.clone()), value);
        assert_eq!(apply_builtin_defaults("", "v1beta1", "Pod", value.clone()), value);
    }

    #[test]
    fn workload_template_defaults_mutate_the_actual_nested_template() {
        let value = json!({
            "spec": {"template": {"spec": {"containers": [{"name": "app", "image": "example/app:1"}]}}}
        });
        let defaulted = apply_builtin_defaults("", "v1", "Deployment", value);
        assert_eq!(defaulted["spec"]["template"]["spec"]["dnsPolicy"], json!("ClusterFirst"));
        assert_eq!(defaulted["spec"]["template"]["spec"]["containers"][0]["imagePullPolicy"], json!("IfNotPresent"));
    }
}
