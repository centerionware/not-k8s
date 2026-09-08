//! `DefaultTolerationSeconds` — a faithful port of real upstream's own
//! mutating admission plugin
//! (`plugin/pkg/admission/defaulttolerationseconds/admission.go`,
//! release-1.34, fetched and read directly): every `Pod` that doesn't
//! already carry its own toleration for the `node.kubernetes.io/not-ready`/
//! `node.kubernetes.io/unreachable` `NoExecute` taints (the real constants,
//! `staging/.../api/core/v1/well_known_taints.go`) gets one added, each
//! with `tolerationSeconds: 300` — real upstream's own default
//! (`--default-not-ready-toleration-seconds`/
//! `--default-unreachable-toleration-seconds`, both default `300`; this
//! crate has no admission-plugin config/flag surface yet, so only the
//! default is ported, named honestly rather than silently hard-coded as
//! if it were the only possible value upstream supports).
//!
//! This crate's **first mutating** admission plugin (`namespace_lifecycle`
//! is validating-only) — it edits the submitted object directly, so unlike
//! that plugin this one is a pure `Value -> Value` transform with no I/O
//! step needed at all: no namespace/cluster state changes whether a Pod
//! already tolerates these taints.
//!
//! "Already tolerates" is upstream's own real matching rule, ported
//! exactly: a toleration whose `key` is the taint's key (or empty — an
//! empty key matches every taint, real upstream's own wildcard-key
//! convention) **and** whose `effect` is `NoExecute` (or empty, the same
//! wildcard convention) counts, regardless of `operator`/
//! `tolerationSeconds` — upstream's own loop never inspects either of
//! those two fields when deciding whether a toleration already exists for
//! this purpose.

use serde_json::{json, Value};

const TAINT_NODE_NOT_READY: &str = "node.kubernetes.io/not-ready";
const TAINT_NODE_UNREACHABLE: &str = "node.kubernetes.io/unreachable";
const DEFAULT_TOLERATION_SECONDS: i64 = 300;

/// Whether `admission::attributes::Attributes` targets a `CREATE`/`UPDATE`
/// of a core-group `pods` object with no subresource — the only shape
/// upstream's own `Admit` runs against (`attributes.GetResource() !=
/// api.Resource("pods")` and `len(attributes.GetSubresource()) > 0` are
/// both real early-return guards, ported).
pub fn applies_to(group: &str, resource: &str, subresource: &str) -> bool {
    group.is_empty() && resource == "pods" && subresource.is_empty()
}

fn toleration_matches(toleration: &Value, taint_key: &str) -> bool {
    let key = toleration.get("key").and_then(Value::as_str).unwrap_or("");
    let effect = toleration.get("effect").and_then(Value::as_str).unwrap_or("");
    (key == taint_key || key.is_empty()) && (effect == "NoExecute" || effect.is_empty())
}

fn default_toleration(taint_key: &str) -> Value {
    json!({
        "key": taint_key,
        "operator": "Exists",
        "effect": "NoExecute",
        "tolerationSeconds": DEFAULT_TOLERATION_SECONDS,
    })
}

/// Mutates `pod`'s `spec.tolerations` in place — appends the default
/// `not-ready`/`unreachable` tolerations the pod doesn't already have one
/// for. Returns whether anything was appended (for tests/observability
/// only; the caller doesn't need to branch on it — mutating a pod that
/// already tolerates both is a correct no-op).
pub fn mutate(pod: &mut Value) -> bool {
    let tolerations = pod.get("spec").and_then(|s| s.get("tolerations")).and_then(Value::as_array).cloned().unwrap_or_default();

    let tolerates_not_ready = tolerations.iter().any(|t| toleration_matches(t, TAINT_NODE_NOT_READY));
    let tolerates_unreachable = tolerations.iter().any(|t| toleration_matches(t, TAINT_NODE_UNREACHABLE));

    if tolerates_not_ready && tolerates_unreachable {
        return false;
    }

    let spec = pod.as_object_mut().and_then(|o| o.entry("spec").or_insert_with(|| json!({})).as_object_mut());
    let Some(spec) = spec else { return false };
    let tolerations_out = spec.entry("tolerations").or_insert_with(|| json!([]));
    let Some(tolerations_out) = tolerations_out.as_array_mut() else { return false };

    let mut mutated = false;
    if !tolerates_not_ready {
        tolerations_out.push(default_toleration(TAINT_NODE_NOT_READY));
        mutated = true;
    }
    if !tolerates_unreachable {
        tolerations_out.push(default_toleration(TAINT_NODE_UNREACHABLE));
        mutated = true;
    }
    mutated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_only_to_core_pods_with_no_subresource() {
        assert!(applies_to("", "pods", ""));
        assert!(!applies_to("", "pods", "status"));
        assert!(!applies_to("apps", "pods", ""));
        assert!(!applies_to("", "deployments", ""));
    }

    #[test]
    fn a_pod_with_no_tolerations_gets_both_defaults_appended() {
        let mut pod = json!({"spec": {}});
        assert!(mutate(&mut pod));
        let tolerations = pod["spec"]["tolerations"].as_array().unwrap();
        assert_eq!(tolerations.len(), 2);
        assert_eq!(tolerations[0]["key"], "node.kubernetes.io/not-ready");
        assert_eq!(tolerations[0]["tolerationSeconds"], 300);
        assert_eq!(tolerations[1]["key"], "node.kubernetes.io/unreachable");
    }

    #[test]
    fn a_pod_with_no_spec_at_all_still_gets_both_defaults() {
        let mut pod = json!({});
        assert!(mutate(&mut pod));
        assert_eq!(pod["spec"]["tolerations"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn an_existing_matching_toleration_is_left_alone_not_duplicated() {
        let mut pod = json!({"spec": {"tolerations": [
            {"key": "node.kubernetes.io/not-ready", "operator": "Exists", "effect": "NoExecute", "tolerationSeconds": 30},
        ]}});
        assert!(mutate(&mut pod));
        let tolerations = pod["spec"]["tolerations"].as_array().unwrap();
        // Only the missing "unreachable" default was appended; the
        // existing not-ready one is untouched (still tolerationSeconds: 30).
        assert_eq!(tolerations.len(), 2);
        assert_eq!(tolerations[0]["tolerationSeconds"], 30);
        assert_eq!(tolerations[1]["key"], "node.kubernetes.io/unreachable");
    }

    #[test]
    fn an_empty_key_toleration_with_noexecute_effect_counts_as_a_wildcard_match() {
        let mut pod = json!({"spec": {"tolerations": [{"effect": "NoExecute"}]}});
        assert!(!mutate(&mut pod), "an empty-key NoExecute toleration already covers both taints");
        assert_eq!(pod["spec"]["tolerations"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_toleration_with_a_different_effect_does_not_count() {
        let mut pod = json!({"spec": {"tolerations": [
            {"key": "node.kubernetes.io/not-ready", "effect": "NoSchedule"},
        ]}});
        assert!(mutate(&mut pod));
        assert_eq!(pod["spec"]["tolerations"].as_array().unwrap().len(), 3, "the NoSchedule toleration doesn't satisfy either default, both get appended");
    }

    #[test]
    fn a_pod_tolerating_both_already_is_a_correct_no_op() {
        let mut pod = json!({"spec": {"tolerations": [
            {"key": "node.kubernetes.io/not-ready", "effect": "NoExecute"},
            {"key": "node.kubernetes.io/unreachable", "effect": "NoExecute"},
        ]}});
        assert!(!mutate(&mut pod));
        assert_eq!(pod["spec"]["tolerations"].as_array().unwrap().len(), 2);
    }
}
