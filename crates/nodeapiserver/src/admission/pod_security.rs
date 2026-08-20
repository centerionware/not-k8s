//! `PodSecurity` — a faithful-but-scoped port of real upstream's built-in
//! Pod Security Standards admission plugin
//! (`staging/src/k8s.io/pod-security-admission/policy/*.go`, release-1.34,
//! fetched and read directly): validates a `Pod` `CREATE` against
//! whichever Pod Security Standards level its namespace's
//! `pod-security.kubernetes.io/enforce` label requests
//! (`privileged`/`baseline`/`restricted` — real upstream's own label,
//! ported exactly; an absent label means `privileged`, real upstream's
//! own "no restriction" default).
//!
//! **Six of the twelve real `baseline`-level checks are ported** (each a
//! faithful port of its own real upstream file, current — i.e. latest —
//! `MinimumVersion` variant only, no version-pinned-check history
//! modeled): `privileged`, `hostNamespaces` (`hostNetwork`/`hostPID`/
//! `hostIPC`), `hostPorts`, `hostPathVolumes`, `capabilities_baseline`
//! (the real default-capability allowlist), `seccompProfile_baseline`
//! (the 1.19+ `securityContext.seccompProfile.type` field form only — the
//! pre-1.19 alpha-annotation form is real but long obsolete, no real
//! cluster targets it, skipped rather than silently glossed over). **Not
//! yet ported, named honestly**: `sysctls`, `procMount`,
//! `hostProbesAndHostLifecycle`, `windowsHostProcess`, `appArmorProfile`,
//! `seLinuxOptions` (the other six real baseline checks), and every real
//! `restricted`-level check (`runAsNonRoot`, `runAsUser`,
//! `allowPrivilegeEscalation`, `capabilities_restricted`,
//! `seccompProfile_restricted`, `restrictedVolumes`) — a namespace labeled
//! `restricted` gets only the baseline checks above enforced today, *not*
//! full restricted enforcement; a real, named under-enforcement, not a
//! silently-assumed-complete one.
//!
//! Also not modeled: `pod-security.kubernetes.io/enforce-version` (pins
//! enforcement to a specific Kubernetes minor version's check semantics —
//! this crate has no version-gated check history to pin to in the first
//! place), and the `warn`/`audit` labels (upstream's own non-enforcing
//! modes — they only affect response warnings/audit annotations, never
//! block a request, so they're out of scope for an *admission* check by
//! definition).
//!
//! Same split as every other Group J plugin: [`applies_to`]/[`validate`]
//! are pure and unit tested with no I/O; `server::listener` performs the
//! one real I/O step (`server::rest::get` on the target namespace) in
//! between, then calls [`enforcement_level`] on the result.

use serde_json::Value;

pub const ENFORCE_LABEL: &str = "pod-security.kubernetes.io/enforce";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Privileged,
    Baseline,
    Restricted,
}

pub fn applies_to(group: &str, resource: &str, subresource: &str, operation: crate::admission::attributes::Operation) -> bool {
    group.is_empty() && resource == "pods" && subresource.is_empty() && operation == crate::admission::attributes::Operation::Create
}

/// Real upstream's own label, read off the target namespace object —
/// `None`/anything but `"baseline"`/`"restricted"` means `Privileged`
/// (upstream's own "no restriction" default, including a genuinely
/// unrecognized value — same "don't fail open into a stricter mode than
/// requested, but don't silently reject an unrecognized label either"
/// posture as leaving it unenforced).
pub fn enforcement_level(namespace: &Value) -> Level {
    match namespace.get("metadata").and_then(|m| m.get("labels")).and_then(|l| l.get(ENFORCE_LABEL)).and_then(Value::as_str) {
        Some("baseline") => Level::Baseline,
        Some("restricted") => Level::Restricted,
        _ => Level::Privileged,
    }
}

fn containers(pod: &Value) -> impl Iterator<Item = &Value> {
    let spec = pod.get("spec");
    let containers = spec.and_then(|s| s.get("containers")).and_then(Value::as_array).into_iter().flatten();
    let init_containers = spec.and_then(|s| s.get("initContainers")).and_then(Value::as_array).into_iter().flatten();
    containers.chain(init_containers)
}

fn container_name(container: &Value) -> &str {
    container.get("name").and_then(Value::as_str).unwrap_or("")
}

fn check_privileged(pod: &Value) -> Option<String> {
    let bad: Vec<&str> = containers(pod)
        .filter(|c| c.get("securityContext").and_then(|sc| sc.get("privileged")).and_then(Value::as_bool) == Some(true))
        .map(container_name)
        .collect();
    if bad.is_empty() {
        None
    } else {
        Some(format!("privileged: container(s) {} must not set securityContext.privileged=true", bad.join(", ")))
    }
}

fn check_host_namespaces(pod: &Value) -> Option<String> {
    let spec = pod.get("spec");
    let mut bad = Vec::new();
    if spec.and_then(|s| s.get("hostNetwork")).and_then(Value::as_bool) == Some(true) {
        bad.push("hostNetwork=true");
    }
    if spec.and_then(|s| s.get("hostPID")).and_then(Value::as_bool) == Some(true) {
        bad.push("hostPID=true");
    }
    if spec.and_then(|s| s.get("hostIPC")).and_then(Value::as_bool) == Some(true) {
        bad.push("hostIPC=true");
    }
    if bad.is_empty() {
        None
    } else {
        Some(format!("host namespaces: {}", bad.join(", ")))
    }
}

fn check_host_ports(pod: &Value) -> Option<String> {
    let mut bad_containers = Vec::new();
    let mut bad_ports = std::collections::BTreeSet::new();
    for container in containers(pod) {
        let ports = container.get("ports").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut valid = true;
        for port in &ports {
            if let Some(host_port) = port.get("hostPort").and_then(Value::as_i64) {
                if host_port != 0 {
                    valid = false;
                    bad_ports.insert(host_port.to_string());
                }
            }
        }
        if !valid {
            bad_containers.push(container_name(container).to_string());
        }
    }
    if bad_containers.is_empty() {
        None
    } else {
        Some(format!("hostPort: container(s) {} use hostPort(s) {}", bad_containers.join(", "), bad_ports.into_iter().collect::<Vec<_>>().join(", ")))
    }
}

fn check_host_path_volumes(pod: &Value) -> Option<String> {
    let bad: Vec<String> = pod
        .get("spec")
        .and_then(|s| s.get("volumes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|v| v.get("hostPath").is_some())
        .map(|v| v.get("name").and_then(Value::as_str).unwrap_or("").to_string())
        .collect();
    if bad.is_empty() {
        None
    } else {
        Some(format!("hostPath volumes: {}", bad.join(", ")))
    }
}

/// Real upstream's own `capabilities_allowed_1_0` set, ported exactly.
const ALLOWED_CAPABILITIES: &[&str] = &["AUDIT_WRITE", "CHOWN", "DAC_OVERRIDE", "FOWNER", "FSETID", "KILL", "MKNOD", "NET_BIND_SERVICE", "SETFCAP", "SETGID", "SETPCAP", "SETUID", "SYS_CHROOT"];

fn check_capabilities_baseline(pod: &Value) -> Option<String> {
    let mut bad_containers = Vec::new();
    let mut bad_caps = std::collections::BTreeSet::new();
    for container in containers(pod) {
        let Some(add) = container.get("securityContext").and_then(|sc| sc.get("capabilities")).and_then(|c| c.get("add")).and_then(Value::as_array) else { continue };
        let mut valid = true;
        for cap in add {
            if let Some(name) = cap.as_str() {
                if !ALLOWED_CAPABILITIES.contains(&name) {
                    valid = false;
                    bad_caps.insert(name.to_string());
                }
            }
        }
        if !valid {
            bad_containers.push(container_name(container).to_string());
        }
    }
    if bad_containers.is_empty() {
        None
    } else {
        Some(format!("non-default capabilities: container(s) {} must not include {} in securityContext.capabilities.add", bad_containers.join(", "), bad_caps.into_iter().collect::<Vec<_>>().join(", ")))
    }
}

fn valid_seccomp_type(t: &str) -> bool {
    t == "RuntimeDefault" || t == "Localhost"
}

fn check_seccomp_profile_baseline(pod: &Value) -> Option<String> {
    let mut bad_setters = Vec::new();
    let mut bad_values = std::collections::BTreeSet::new();

    if let Some(t) = pod.get("spec").and_then(|s| s.get("securityContext")).and_then(|sc| sc.get("seccompProfile")).and_then(|sp| sp.get("type")).and_then(Value::as_str) {
        if !valid_seccomp_type(t) {
            bad_setters.push("pod".to_string());
            bad_values.insert(t.to_string());
        }
    }

    let bad_containers: Vec<String> = containers(pod)
        .filter_map(|c| {
            let t = c.get("securityContext").and_then(|sc| sc.get("seccompProfile")).and_then(|sp| sp.get("type")).and_then(Value::as_str)?;
            if valid_seccomp_type(t) {
                None
            } else {
                bad_values.insert(t.to_string());
                Some(container_name(c).to_string())
            }
        })
        .collect();
    if !bad_containers.is_empty() {
        bad_setters.push(format!("container(s) {}", bad_containers.join(", ")));
    }

    if bad_setters.is_empty() {
        None
    } else {
        Some(format!("seccompProfile: {} must not set securityContext.seccompProfile.type to {}", bad_setters.join(" and "), bad_values.into_iter().collect::<Vec<_>>().join(", ")))
    }
}

/// Every landed `baseline`-level check, run in real upstream's own file
/// order — collects every failing check's message rather than stopping
/// at the first (matching `PodValidateLimitFunc`'s own "aggregate every
/// violation" posture, and real PSA's own `AggregateCheckResults`).
fn baseline_violations(pod: &Value) -> Vec<String> {
    [check_privileged(pod), check_host_namespaces(pod), check_host_ports(pod), check_host_path_volumes(pod), check_capabilities_baseline(pod), check_seccomp_profile_baseline(pod)].into_iter().flatten().collect()
}

/// `level` is [`enforcement_level`]'s own output for the pod's namespace.
/// `Restricted` currently enforces only the same baseline checks
/// `Baseline` does — see this module's own doc comment for why that's a
/// real, named under-enforcement, not full restricted-level checking.
pub fn validate(pod: &Value, level: Level) -> Vec<String> {
    match level {
        Level::Privileged => Vec::new(),
        Level::Baseline | Level::Restricted => baseline_violations(pod),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ns_with_level(level: &str) -> Value {
        json!({"metadata": {"labels": {"pod-security.kubernetes.io/enforce": level}}})
    }

    #[test]
    fn enforcement_level_reads_the_real_label() {
        assert_eq!(enforcement_level(&ns_with_level("baseline")), Level::Baseline);
        assert_eq!(enforcement_level(&ns_with_level("restricted")), Level::Restricted);
        assert_eq!(enforcement_level(&ns_with_level("privileged")), Level::Privileged);
    }

    #[test]
    fn enforcement_level_defaults_to_privileged_when_absent_or_unrecognized() {
        assert_eq!(enforcement_level(&json!({})), Level::Privileged);
        assert_eq!(enforcement_level(&ns_with_level("something-future")), Level::Privileged);
    }

    #[test]
    fn privileged_level_enforces_nothing() {
        let pod = json!({"spec": {"hostNetwork": true, "containers": [{"name": "c1", "securityContext": {"privileged": true}}]}});
        assert!(validate(&pod, Level::Privileged).is_empty());
    }

    #[test]
    fn baseline_rejects_a_privileged_container() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"privileged": true}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("privileged"));
    }

    #[test]
    fn baseline_rejects_host_namespaces() {
        for field in ["hostNetwork", "hostPID", "hostIPC"] {
            let pod = json!({"spec": {field: true, "containers": []}});
            let violations = validate(&pod, Level::Baseline);
            assert_eq!(violations.len(), 1, "{field} must be rejected");
        }
    }

    #[test]
    fn baseline_rejects_a_host_port() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "ports": [{"hostPort": 8080}]}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("8080"));
    }

    #[test]
    fn baseline_allows_a_container_port_with_no_host_port() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "ports": [{"containerPort": 8080}]}]}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_rejects_a_hostpath_volume() {
        let pod = json!({"spec": {"containers": [], "volumes": [{"name": "v1", "hostPath": {"path": "/etc"}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("v1"));
    }

    #[test]
    fn baseline_allows_a_non_hostpath_volume() {
        let pod = json!({"spec": {"containers": [], "volumes": [{"name": "v1", "emptyDir": {}}]}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_rejects_a_non_default_capability() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"capabilities": {"add": ["NET_ADMIN"]}}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("NET_ADMIN"));
    }

    #[test]
    fn baseline_allows_a_default_capability() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"capabilities": {"add": ["CHOWN"]}}}]}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_rejects_an_unconfined_seccomp_profile() {
        let pod = json!({"spec": {"containers": [], "securityContext": {"seccompProfile": {"type": "Unconfined"}}}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("Unconfined"));
    }

    #[test]
    fn baseline_allows_runtime_default_seccomp() {
        let pod = json!({"spec": {"containers": [], "securityContext": {"seccompProfile": {"type": "RuntimeDefault"}}}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_allows_a_clean_pod() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "image": "nginx"}]}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn multiple_violations_are_all_collected_not_just_the_first() {
        let pod = json!({"spec": {
            "hostNetwork": true,
            "containers": [{"name": "c1", "securityContext": {"privileged": true}}],
        }});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 2);
    }

    #[test]
    fn init_containers_are_checked_too() {
        let pod = json!({"spec": {"initContainers": [{"name": "init1", "securityContext": {"privileged": true}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("init1"));
    }

    #[test]
    fn applies_to_pod_create_only() {
        use crate::admission::attributes::Operation;
        assert!(applies_to("", "pods", "", Operation::Create));
        assert!(!applies_to("", "pods", "", Operation::Update));
        assert!(!applies_to("", "pods", "status", Operation::Create));
        assert!(!applies_to("apps", "pods", "", Operation::Create));
    }
}
