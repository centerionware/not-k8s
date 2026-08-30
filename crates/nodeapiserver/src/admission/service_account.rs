//! `ServiceAccount` — a faithful port of real upstream's own admission
//! plugin (`plugin/pkg/admission/serviceaccount/admission.go`, release-1.34,
//! fetched and read directly). Real upstream's `Admit` + `Validate` do five
//! things on a `Pod` `CREATE`; this module ports four of them:
//!
//! 1. Defaults `spec.serviceAccountName` to `"default"` when unset
//!    ([`default_service_account_name`]).
//! 2. Requires the referenced `ServiceAccount` to actually exist —
//!    forbidden if it doesn't (upstream's own `getServiceAccount`
//!    error path; this port skips upstream's retry-with-backoff loop for
//!    the `default` SA specifically, since that loop exists to ride out a
//!    controller that hasn't auto-created it yet, and this crate has no
//!    such controller — named honestly, not silently dropped).
//! 3. Auto-mounts a projected `kube-api-access-*` token volume into every
//!    container that doesn't already have its own mount at
//!    `/var/run/secrets/kubernetes.io/serviceaccount`, when neither the
//!    pod nor its `ServiceAccount` opts out (`shouldAutomount`, ported
//!    exactly — pod's own `automountServiceAccountToken` wins, then the
//!    `ServiceAccount`'s, defaulting to `true`).
//! 4. Copies the `ServiceAccount`'s own `imagePullSecrets` onto the pod
//!    when the pod specifies none of its own.
//!
//! The opt-in `LimitSecretReferences`/`enforceMountableSecrets` check is
//! supported too: a `ServiceAccount` annotated with
//! `kubernetes.io/enforce-mountable-secrets: "true"` may only be used by a
//! pod that references secrets and image-pull secrets listed on that
//! account. The `pods/ephemeralcontainers` subresource validation path
//! remains separate because that subresource has its own update strategy.
//!
//! Mirror-pod handling is ported too: a pod carrying real upstream's own
//! `kubernetes.io/config.mirror` annotation is never mutated (mutating a
//! kubelet-owned mirror pod's spec makes the kubelet immediately delete
//! it, per upstream's own comment) and is instead validated against three
//! real restrictions (`Validate`'s own mirror-pod branch, ported exactly):
//! it may not reference a `ServiceAccount`, a `Secret` (env/envFrom/volume),
//! or a projected `ServiceAccountToken` volume source.
//!
//! Split the same way `namespace_lifecycle` is: a pure decision
//! ([`quick_decision`]/[`mutate_with_service_account`], unit tested with no
//! I/O) plus the one real I/O step a caller performs in between
//! (`server::listener` calls `server::rest::get` for the `ServiceAccount`
//! only when [`Decision::NeedsServiceAccountLookup`] says to).

use serde_json::{json, Value};
use std::collections::BTreeSet;

pub const DEFAULT_SERVICE_ACCOUNT_NAME: &str = "default";
/// `pub` — `server::listener`'s own real random-name generator needs this
/// to build the *full* name it hands to [`mutate_with_service_account`]'s
/// `generate_volume_name` (that closure returns a complete name, not just
/// a suffix — real upstream's own `s.generateName(ServiceAccountVolumeName
/// + "-")` does the concatenation before this plugin ever sees the
/// result, and this port keeps that same division of responsibility).
pub const SERVICE_ACCOUNT_VOLUME_PREFIX: &str = "kube-api-access-";
pub const ENFORCE_MOUNTABLE_SECRETS_ANNOTATION: &str = "kubernetes.io/enforce-mountable-secrets";
const DEFAULT_API_TOKEN_MOUNT_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount";
const MIRROR_POD_ANNOTATION_KEY: &str = "kubernetes.io/config.mirror";
/// Real upstream's own `serviceaccount.WarnOnlyBoundTokenExpirationSeconds`
/// (`pkg/serviceaccount/claims.go`): `60*60 + 7`.
const TOKEN_EXPIRATION_SECONDS: i64 = 60 * 60 + 7;
/// Real upstream's own `corev1.ProjectedVolumeSourceDefaultMode`.
const PROJECTED_VOLUME_DEFAULT_MODE: i64 = 0o644;

pub fn applies_to(group: &str, resource: &str, subresource: &str) -> bool {
    group.is_empty() && resource == "pods" && subresource.is_empty()
}

fn is_mirror_pod(pod: &Value) -> bool {
    pod.get("metadata").and_then(|m| m.get("annotations")).and_then(|a| a.get(MIRROR_POD_ANNOTATION_KEY)).is_some()
}

fn service_account_name(pod: &Value) -> &str {
    pod.get("spec").and_then(|s| s.get("serviceAccountName")).and_then(Value::as_str).unwrap_or("")
}

/// Real upstream's own default-assignment step — only for a non-mirror
/// pod (a mirror pod's spec is never mutated by this plugin at all; see
/// this module's own doc comment).
pub fn default_service_account_name(pod: &mut Value) {
    if is_mirror_pod(pod) {
        return;
    }
    if service_account_name(pod).is_empty() {
        if let Some(spec) = pod.as_object_mut().and_then(|o| o.entry("spec").or_insert_with(|| json!({})).as_object_mut()) {
            spec.insert("serviceAccountName".to_string(), json!(DEFAULT_SERVICE_ACCOUNT_NAME));
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Decision {
    Allow,
    Forbidden(String),
    /// The caller must fetch `server::rest::get` on `("", "serviceaccounts",
    /// Some(namespace), <spec.serviceAccountName>)` and call
    /// [`mutate_with_service_account`] with the result.
    NeedsServiceAccountLookup,
}

/// Whether any container (`initContainers`+`containers`) in `pod`
/// references `secret_name` via an env var or `envFrom`, or any volume is
/// a `Secret` volume named `secret_name` — real upstream's own
/// `podutil.VisitPodSecretNames` equivalent, used here only for the
/// mirror-pod "may not reference secrets" check (the full
/// `limitSecretReferences` enforcement is not ported — see this module's
/// own doc comment).
fn references_any_secret(pod: &Value) -> bool {
    let spec = pod.get("spec");
    let volumes_reference_secret = spec.and_then(|s| s.get("volumes")).and_then(Value::as_array).is_some_and(|vols| vols.iter().any(|v| v.get("secret").is_some()));
    if volumes_reference_secret {
        return true;
    }
    let containers = |key: &str| spec.and_then(|s| s.get(key)).and_then(Value::as_array).cloned().unwrap_or_default();
    for container in containers("initContainers").iter().chain(containers("containers").iter()) {
        let env_references_secret = container.get("env").and_then(Value::as_array).is_some_and(|envs| envs.iter().any(|e| e.get("valueFrom").and_then(|v| v.get("secretKeyRef")).is_some()));
        if env_references_secret {
            return true;
        }
        let env_from_references_secret = container.get("envFrom").and_then(Value::as_array).is_some_and(|refs| refs.iter().any(|e| e.get("secretRef").is_some()));
        if env_from_references_secret {
            return true;
        }
    }
    false
}

fn references_service_account_token_projection(pod: &Value) -> bool {
    pod.get("spec")
        .and_then(|s| s.get("volumes"))
        .and_then(Value::as_array)
        .is_some_and(|vols| vols.iter().any(|v| v.get("projected").and_then(|p| p.get("sources")).and_then(Value::as_array).is_some_and(|srcs| srcs.iter().any(|src| src.get("serviceAccountToken").is_some()))))
}

fn service_account_name_for_errors(service_account: &Value) -> &str {
    service_account
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn enforce_mountable_secrets(service_account: &Value) -> bool {
    let Some(value) = service_account
        .pointer("/metadata/annotations/kubernetes.io~1enforce-mountable-secrets")
        .and_then(Value::as_str)
    else {
        return false;
    };
    matches!(value, "1" | "t" | "T" | "TRUE" | "True" | "true")
}

fn mountable_secret_names(service_account: &Value) -> BTreeSet<&str> {
    service_account
        .get("secrets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|secret| secret.get("name").and_then(Value::as_str))
        .collect()
}

fn mountable_image_pull_secret_names(service_account: &Value) -> BTreeSet<&str> {
    service_account
        .get("imagePullSecrets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|secret| secret.get("name").and_then(Value::as_str))
        .collect()
}

fn validate_container_secret_references(
    service_account: &Value,
    containers: &[Value],
    description: &str,
    allowed: &BTreeSet<&str>,
) -> Result<(), String> {
    let service_account_name = service_account_name_for_errors(service_account);
    for container in containers {
        let container_name = container.get("name").and_then(Value::as_str).unwrap_or("");
        if let Some(envs) = container.get("env").and_then(Value::as_array) {
            for env in envs {
                let Some(secret_name) = env
                    .pointer("/valueFrom/secretKeyRef/name")
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                if !allowed.contains(secret_name) {
                    return Err(format!(
                        "{description} {container_name} with envVar {} referencing secret.secretName=\"{secret_name}\" is not allowed because service account {service_account_name} does not reference that secret",
                        env.get("name").and_then(Value::as_str).unwrap_or("")
                    ));
                }
            }
        }
        if let Some(env_from) = container.get("envFrom").and_then(Value::as_array) {
            for reference in env_from {
                let Some(secret_name) = reference
                    .pointer("/secretRef/name")
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                if !allowed.contains(secret_name) {
                    return Err(format!(
                        "{description} {container_name} with envFrom referencing secret.secretName=\"{secret_name}\" is not allowed because service account {service_account_name} does not reference that secret"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Enforces the upstream opt-in mountable-secret check for a Pod `CREATE`.
/// The account annotation controls the check; an absent annotation or any
/// value other than a Go `strconv.ParseBool` true spelling leaves the pod
/// unrestricted.
pub fn validate_secret_references(service_account: &Value, pod: &Value) -> Result<(), String> {
    if !enforce_mountable_secrets(service_account) {
        return Ok(());
    }
    let allowed = mountable_secret_names(service_account);
    let service_account_name = service_account_name_for_errors(service_account);
    let spec = pod.get("spec");
    if let Some(volumes) = spec.and_then(|spec| spec.get("volumes")).and_then(Value::as_array) {
        for volume in volumes {
            let Some(secret_name) = volume.pointer("/secret/secretName").and_then(Value::as_str) else {
                continue;
            };
            if !allowed.contains(secret_name) {
                return Err(format!(
                    "volume with secret.secretName=\"{secret_name}\" is not allowed because service account {service_account_name} does not reference that secret"
                ));
            }
        }
    }
    for key in ["initContainers", "containers"] {
        let containers = spec
            .and_then(|spec| spec.get(key))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let description = if key == "initContainers" {
            "init container"
        } else {
            "container"
        };
        validate_container_secret_references(service_account, containers, description, &allowed)?;
    }

    let allowed_pull_secrets = mountable_image_pull_secret_names(service_account);
    if let Some(pull_secrets) = spec
        .and_then(|spec| spec.get("imagePullSecrets"))
        .and_then(Value::as_array)
    {
        for (index, pull_secret) in pull_secrets.iter().enumerate() {
            let secret_name = pull_secret.get("name").and_then(Value::as_str).unwrap_or("");
            if !allowed_pull_secrets.contains(secret_name) {
                return Err(format!(
                    "imagePullSecrets[{index}].name=\"{secret_name}\" is not allowed because service account {service_account_name} does not reference that imagePullSecret"
                ));
            }
        }
    }
    Ok(())
}

/// The pure decision, with no I/O: mirror-pod validation needs none (its
/// three restrictions are all readable straight off `pod`); every other
/// `Pod` `CREATE` needs a `ServiceAccount` lookup before this plugin can
/// finish (even one that already names a service account still needs it
/// fetched, for the automount/imagePullSecrets steps — real upstream's
/// own `Admit` always calls `getServiceAccount`, unconditionally).
/// `operation` other than `Create` is always `Allow` — real upstream's own
/// plugin only mutates/validates on `CREATE`; the separate
/// `ephemeralcontainers` subresource `UPDATE` path is handled by its own Pod
/// strategy rather than this plugin — see this module's own doc comment.
pub fn quick_decision(pod: &Value, operation: crate::admission::attributes::Operation) -> Decision {
    if operation != crate::admission::attributes::Operation::Create {
        return Decision::Allow;
    }

    if is_mirror_pod(pod) {
        if !service_account_name(pod).is_empty() {
            return Decision::Forbidden("a mirror pod may not reference service accounts".to_string());
        }
        if references_any_secret(pod) {
            return Decision::Forbidden("a mirror pod may not reference secrets".to_string());
        }
        if references_service_account_token_projection(pod) {
            return Decision::Forbidden("a mirror pod may not use ServiceAccountToken volume projections".to_string());
        }
        return Decision::Allow;
    }

    Decision::NeedsServiceAccountLookup
}

fn should_automount(service_account: &Value, pod: &Value) -> bool {
    if let Some(pod_pref) = pod.get("spec").and_then(|s| s.get("automountServiceAccountToken")).and_then(Value::as_bool) {
        return pod_pref;
    }
    if let Some(sa_pref) = service_account.get("automountServiceAccountToken").and_then(Value::as_bool) {
        return sa_pref;
    }
    true
}

fn has_mount_at_default_path(container: &Value) -> bool {
    container.get("volumeMounts").and_then(Value::as_array).is_some_and(|mounts| mounts.iter().any(|m| m.get("mountPath").and_then(Value::as_str) == Some(DEFAULT_API_TOKEN_MOUNT_PATH)))
}

fn projected_token_volume_source() -> Value {
    json!({
        "defaultMode": PROJECTED_VOLUME_DEFAULT_MODE,
        "sources": [
            {"serviceAccountToken": {"path": "token", "expirationSeconds": TOKEN_EXPIRATION_SECONDS}},
            {"configMap": {"name": "kube-root-ca.crt", "items": [{"key": "ca.crt", "path": "ca.crt"}]}},
            {"downwardAPI": {"items": [{"path": "namespace", "fieldRef": {"apiVersion": "v1", "fieldPath": "metadata.namespace"}}]}},
        ],
    })
}

/// Appends a `volumeMounts` entry to every container in `containers` that
/// doesn't already have one at the default token mount path, using
/// `volume_mount`. Returns whether any container actually needed it
/// (real upstream's own `needsTokenVolume`) — the caller only adds the
/// backing `Volume` if this is true.
fn mount_into_containers(containers: &mut Vec<Value>, volume_mount: &Value) -> bool {
    let mut needs_token_volume = false;
    for container in containers.iter_mut() {
        if has_mount_at_default_path(container) {
            continue;
        }
        let Some(obj) = container.as_object_mut() else { continue };
        obj.entry("volumeMounts").or_insert_with(|| json!([])).as_array_mut().expect("volumeMounts is always an array here").push(volume_mount.clone());
        needs_token_volume = true;
    }
    needs_token_volume
}

/// The one real I/O-dependent half: mutates `pod` given the real
/// `service_account` object [`Decision::NeedsServiceAccountLookup`]
/// resolved to. `generate_volume_name` supplies a **complete** new token
/// volume name (`SERVICE_ACCOUNT_VOLUME_PREFIX` + a random suffix — real
/// upstream's own `names.SimpleNameGenerator.GenerateName` does this same
/// concatenation before handing back a name) — injected so tests are
/// deterministic; `server::listener` passes a real random generator.
pub fn mutate_with_service_account(pod: &mut Value, service_account: &Value, generate_volume_name: impl FnOnce() -> String) {
    if should_automount(service_account, pod) {
        mount_service_account_token(pod, generate_volume_name);
    }

    let has_pull_secrets = pod.get("spec").and_then(|s| s.get("imagePullSecrets")).and_then(Value::as_array).is_some_and(|arr| !arr.is_empty());
    if !has_pull_secrets {
        let pull_secrets: Vec<Value> = service_account.get("imagePullSecrets").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().map(|s| json!({"name": s.get("name").and_then(Value::as_str).unwrap_or("")})).collect();
        if let Some(spec) = pod.as_object_mut().and_then(|o| o.entry("spec").or_insert_with(|| json!({})).as_object_mut()) {
            spec.insert("imagePullSecrets".to_string(), json!(pull_secrets));
        }
    }
}

fn mount_service_account_token(pod: &mut Value, generate_volume_name: impl FnOnce() -> String) {
    let existing_token_volume_name = pod
        .get("spec")
        .and_then(|s| s.get("volumes"))
        .and_then(Value::as_array)
        .and_then(|vols| vols.iter().find_map(|v| v.get("name").and_then(Value::as_str).filter(|n| n.starts_with(SERVICE_ACCOUNT_VOLUME_PREFIX)).map(str::to_string)));
    let has_token_volume = existing_token_volume_name.is_some();
    let token_volume_name = existing_token_volume_name.unwrap_or_else(generate_volume_name);

    let volume_mount = json!({"name": token_volume_name, "readOnly": true, "mountPath": DEFAULT_API_TOKEN_MOUNT_PATH});

    let Some(spec) = pod.as_object_mut().and_then(|o| o.entry("spec").or_insert_with(|| json!({})).as_object_mut()) else { return };

    let mut needs_token_volume = false;
    for key in ["initContainers", "containers"] {
        if let Some(containers) = spec.get_mut(key).and_then(Value::as_array_mut) {
            needs_token_volume |= mount_into_containers(containers, &volume_mount);
        }
    }

    if !has_token_volume && needs_token_volume {
        let volumes = spec.entry("volumes").or_insert_with(|| json!([])).as_array_mut().expect("volumes is always an array here");
        volumes.push(json!({"name": token_volume_name, "projected": projected_token_volume_source()}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::attributes::Operation;

    fn suffix() -> String {
        format!("{SERVICE_ACCOUNT_VOLUME_PREFIX}abcde")
    }

    #[test]
    fn default_service_account_name_fills_in_the_default_when_unset() {
        let mut pod = json!({"spec": {}});
        default_service_account_name(&mut pod);
        assert_eq!(pod["spec"]["serviceAccountName"], "default");
    }

    #[test]
    fn default_service_account_name_leaves_an_explicit_name_alone() {
        let mut pod = json!({"spec": {"serviceAccountName": "custom-sa"}});
        default_service_account_name(&mut pod);
        assert_eq!(pod["spec"]["serviceAccountName"], "custom-sa");
    }

    #[test]
    fn default_service_account_name_does_not_touch_a_mirror_pod() {
        let mut pod = json!({"metadata": {"annotations": {"kubernetes.io/config.mirror": "abc"}}, "spec": {}});
        default_service_account_name(&mut pod);
        assert!(pod["spec"].get("serviceAccountName").is_none());
    }

    #[test]
    fn a_non_create_operation_is_always_allowed_with_no_lookup() {
        let pod = json!({"spec": {}});
        assert_eq!(quick_decision(&pod, Operation::Update), Decision::Allow);
        assert_eq!(quick_decision(&pod, Operation::Delete), Decision::Allow);
    }

    #[test]
    fn an_ordinary_pod_create_needs_a_service_account_lookup() {
        let pod = json!({"spec": {"serviceAccountName": "default"}});
        assert_eq!(quick_decision(&pod, Operation::Create), Decision::NeedsServiceAccountLookup);
    }

    #[test]
    fn a_mirror_pod_referencing_a_service_account_is_forbidden() {
        let pod = json!({"metadata": {"annotations": {"kubernetes.io/config.mirror": "x"}}, "spec": {"serviceAccountName": "default"}});
        assert!(matches!(quick_decision(&pod, Operation::Create), Decision::Forbidden(_)));
    }

    #[test]
    fn a_mirror_pod_referencing_a_secret_env_var_is_forbidden() {
        let pod = json!({
            "metadata": {"annotations": {"kubernetes.io/config.mirror": "x"}},
            "spec": {"containers": [{"env": [{"name": "X", "valueFrom": {"secretKeyRef": {"name": "s"}}}]}]},
        });
        assert!(matches!(quick_decision(&pod, Operation::Create), Decision::Forbidden(_)));
    }

    #[test]
    fn a_mirror_pod_referencing_a_secret_volume_is_forbidden() {
        let pod = json!({
            "metadata": {"annotations": {"kubernetes.io/config.mirror": "x"}},
            "spec": {"volumes": [{"name": "v", "secret": {"secretName": "s"}}]},
        });
        assert!(matches!(quick_decision(&pod, Operation::Create), Decision::Forbidden(_)));
    }

    #[test]
    fn a_mirror_pod_with_a_service_account_token_projection_is_forbidden() {
        let pod = json!({
            "metadata": {"annotations": {"kubernetes.io/config.mirror": "x"}},
            "spec": {"volumes": [{"name": "v", "projected": {"sources": [{"serviceAccountToken": {}}]}}]},
        });
        assert!(matches!(quick_decision(&pod, Operation::Create), Decision::Forbidden(_)));
    }

    #[test]
    fn a_clean_mirror_pod_is_allowed() {
        let pod = json!({"metadata": {"annotations": {"kubernetes.io/config.mirror": "x"}}, "spec": {}});
        assert_eq!(quick_decision(&pod, Operation::Create), Decision::Allow);
    }

    #[test]
    fn automount_defaults_to_true_when_neither_pod_nor_sa_opts_out() {
        assert!(should_automount(&json!({}), &json!({"spec": {}})));
    }

    #[test]
    fn pod_preference_wins_over_service_account_preference() {
        let pod = json!({"spec": {"automountServiceAccountToken": false}});
        let sa = json!({"automountServiceAccountToken": true});
        assert!(!should_automount(&sa, &pod));
    }

    #[test]
    fn service_account_preference_is_used_when_pod_has_none() {
        let pod = json!({"spec": {}});
        let sa = json!({"automountServiceAccountToken": false});
        assert!(!should_automount(&sa, &pod));
    }

    #[test]
    fn mutate_adds_a_token_volume_mount_to_every_container_and_the_backing_volume() {
        let mut pod = json!({"spec": {"containers": [{"name": "c1"}, {"name": "c2"}]}});
        mutate_with_service_account(&mut pod, &json!({}), suffix);
        let containers = pod["spec"]["containers"].as_array().unwrap();
        for c in containers {
            let mounts = c["volumeMounts"].as_array().unwrap();
            assert_eq!(mounts.len(), 1);
            assert_eq!(mounts[0]["mountPath"], DEFAULT_API_TOKEN_MOUNT_PATH);
            assert_eq!(mounts[0]["name"], "kube-api-access-abcde");
        }
        let volumes = pod["spec"]["volumes"].as_array().unwrap();
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0]["name"], "kube-api-access-abcde");
        assert!(volumes[0]["projected"]["sources"].as_array().unwrap().iter().any(|s| s.get("serviceAccountToken").is_some()));
    }

    #[test]
    fn a_container_with_its_own_mount_at_the_default_path_is_left_alone() {
        let mut pod = json!({"spec": {"containers": [{"name": "c1", "volumeMounts": [{"name": "custom", "mountPath": DEFAULT_API_TOKEN_MOUNT_PATH}]}]}});
        mutate_with_service_account(&mut pod, &json!({}), suffix);
        let mounts = pod["spec"]["containers"][0]["volumeMounts"].as_array().unwrap();
        assert_eq!(mounts.len(), 1, "no second mount should be added");
        // No container needed the token volume, so no backing Volume either.
        assert!(pod["spec"].get("volumes").is_none());
    }

    #[test]
    fn an_existing_token_volume_is_reused_by_name_not_duplicated() {
        let mut pod = json!({"spec": {
            "containers": [{"name": "c1"}],
            "volumes": [{"name": "kube-api-access-xyz12", "projected": {}}],
        }});
        mutate_with_service_account(&mut pod, &json!({}), suffix);
        assert_eq!(pod["spec"]["volumes"].as_array().unwrap().len(), 1, "must reuse the existing token volume, not add a second one");
        assert_eq!(pod["spec"]["containers"][0]["volumeMounts"][0]["name"], "kube-api-access-xyz12");
    }

    #[test]
    fn automount_disabled_mounts_nothing() {
        let mut pod = json!({"spec": {"automountServiceAccountToken": false, "containers": [{"name": "c1"}]}});
        mutate_with_service_account(&mut pod, &json!({}), suffix);
        assert!(pod["spec"]["containers"][0].get("volumeMounts").is_none());
        assert!(pod["spec"].get("volumes").is_none());
    }

    #[test]
    fn image_pull_secrets_are_copied_from_the_service_account_when_the_pod_has_none() {
        let mut pod = json!({"spec": {"automountServiceAccountToken": false}});
        let sa = json!({"imagePullSecrets": [{"name": "regcred"}]});
        mutate_with_service_account(&mut pod, &sa, suffix);
        assert_eq!(pod["spec"]["imagePullSecrets"], json!([{"name": "regcred"}]));
    }

    #[test]
    fn image_pull_secrets_already_set_on_the_pod_are_not_overwritten() {
        let mut pod = json!({"spec": {"automountServiceAccountToken": false, "imagePullSecrets": [{"name": "own-secret"}]}});
        let sa = json!({"imagePullSecrets": [{"name": "regcred"}]});
        mutate_with_service_account(&mut pod, &sa, suffix);
        assert_eq!(pod["spec"]["imagePullSecrets"], json!([{"name": "own-secret"}]));
    }

    #[test]
    fn mountable_secret_references_are_unrestricted_without_the_opt_in_annotation() {
        let service_account = json!({"metadata": {"name": "builder"}});
        let pod = json!({
            "spec": {
                "volumes": [{"name": "credentials", "secret": {"secretName": "not-listed"}}],
                "containers": [{
                    "name": "app",
                    "env": [{"name": "TOKEN", "valueFrom": {"secretKeyRef": {"name": "not-listed", "key": "token"}}}],
                    "envFrom": [{"secretRef": {"name": "also-not-listed"}}]
                }],
                "imagePullSecrets": [{"name": "pull-not-listed"}]
            }
        });
        assert!(validate_secret_references(&service_account, &pod).is_ok());
    }

    #[test]
    fn annotated_service_account_rejects_unlisted_volume_secret() {
        let service_account = json!({
            "metadata": {"name": "builder", "annotations": {ENFORCE_MOUNTABLE_SECRETS_ANNOTATION: "true"}},
            "secrets": [{"name": "allowed"}],
            "imagePullSecrets": [{"name": "pull-allowed"}]
        });
        let pod = json!({"spec": {"volumes": [{"name": "credentials", "secret": {"secretName": "not-listed"}}]}});
        let error = validate_secret_references(&service_account, &pod).unwrap_err();
        assert!(error.contains("volume with secret.secretName=\"not-listed\""));
        assert!(error.contains("service account builder"));
    }

    #[test]
    fn annotated_service_account_rejects_unlisted_container_secret_and_pull_secret() {
        let service_account = json!({
            "metadata": {"name": "builder", "annotations": {ENFORCE_MOUNTABLE_SECRETS_ANNOTATION: "true"}},
            "secrets": [{"name": "allowed"}],
            "imagePullSecrets": [{"name": "pull-allowed"}]
        });
        let pod = json!({
            "spec": {
                "containers": [{
                    "name": "app",
                    "env": [{"name": "TOKEN", "valueFrom": {"secretKeyRef": {"name": "not-listed", "key": "token"}}}],
                    "envFrom": [{"secretRef": {"name": "also-not-listed"}}]
                }],
                "imagePullSecrets": [{"name": "pull-not-listed"}]
            }
        });
        let error = validate_secret_references(&service_account, &pod).unwrap_err();
        assert!(error.contains("container app with envVar TOKEN"));

        let allowed_pod = json!({
            "spec": {
                "containers": [{
                    "name": "app",
                    "env": [{"name": "TOKEN", "valueFrom": {"secretKeyRef": {"name": "allowed", "key": "token"}}}],
                    "envFrom": [{"secretRef": {"name": "allowed"}}]
                }],
                "imagePullSecrets": [{"name": "pull-not-listed"}]
            }
        });
        let error = validate_secret_references(&service_account, &allowed_pod).unwrap_err();
        assert!(error.contains("imagePullSecrets[0]"));
    }

    #[test]
    fn all_secret_reference_forms_listed_on_the_account_are_allowed() {
        let service_account = json!({
            "metadata": {"name": "builder", "annotations": {ENFORCE_MOUNTABLE_SECRETS_ANNOTATION: "TRUE"}},
            "secrets": [{"name": "allowed"}],
            "imagePullSecrets": [{"name": "pull-allowed"}]
        });
        let pod = json!({
            "spec": {
                "volumes": [{"name": "credentials", "secret": {"secretName": "allowed"}}],
                "initContainers": [{
                    "name": "init",
                    "env": [{"name": "TOKEN", "valueFrom": {"secretKeyRef": {"name": "allowed", "key": "token"}}}],
                    "envFrom": [{"secretRef": {"name": "allowed"}}]
                }],
                "containers": [{"name": "app", "env": [{"name": "TOKEN", "valueFrom": {"secretKeyRef": {"name": "allowed", "key": "token"}}}]}],
                "imagePullSecrets": [{"name": "pull-allowed"}]
            }
        });
        assert!(validate_secret_references(&service_account, &pod).is_ok());
    }
}
