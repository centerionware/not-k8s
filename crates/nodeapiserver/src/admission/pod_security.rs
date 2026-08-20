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
//! **All twelve real `baseline`-level checks are now ported** (each a
//! faithful port of its own real upstream file, current — i.e. latest —
//! `MinimumVersion` variant only, no version-pinned-check history
//! modeled, so this always enforces whatever the *newest* variant of each
//! check requires): `privileged`, `hostNamespaces` (`hostNetwork`/
//! `hostPID`/`hostIPC`), `hostPorts`, `hostPathVolumes`,
//! `capabilities_baseline` (the real default-capability allowlist),
//! `seccompProfile_baseline` (the 1.19+ `securityContext.seccompProfile.type`
//! field form only — the pre-1.19 alpha-annotation form is real but long
//! obsolete, no real cluster targets it, skipped rather than silently
//! glossed over), `sysctls` (the 1.32+ allowed set — the widest one
//! upstream has defined), `procMount` (the `UserNamespacesPodSecurityStandards`
//! relaxation for `hostUsers: false` pods is ported too — real upstream's
//! own comment notes pod validation already checks for a well-formed
//! `procMount` type there, so this deliberately allows everything in that
//! case rather than double-validating), `hostProbesAndHostLifecycle`
//! (upstream's own newest check, 1.34+ — no
//! `SkipProbeHostEnforcement`/emulated-older-minor-version opt-out
//! modeled, this crate has no minor-version emulation concept),
//! `windowsHostProcess`, `appArmorProfile` (both the deprecated
//! annotation form and the real `securityContext.appArmorProfile.type`
//! field form — `spec.ephemeralContainers` is real upstream scope this
//! doesn't cover, since this crate serves no subresources/ephemeral
//! containers at all), `seLinuxOptions` (the 1.31+ allowed-type set,
//! which is the widest one upstream has defined).
//!
//! **Not yet ported, named honestly**: every real `restricted`-level
//! check (`runAsNonRoot`, `runAsUser`, `allowPrivilegeEscalation`,
//! `capabilities_restricted`, `seccompProfile_restricted`,
//! `restrictedVolumes`) — a namespace labeled `restricted` gets only the
//! full baseline check set above enforced today, *not* full restricted
//! enforcement; a real, named under-enforcement, not a
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

/// Real upstream's own `sysctlsAllowedV1Dot32` — the widest (newest)
/// allowed set upstream has defined, since this crate models no
/// version-pinned check history.
const ALLOWED_SYSCTLS: &[&str] = &[
    "kernel.shm_rmid_forced",
    "net.ipv4.ip_local_port_range",
    "net.ipv4.tcp_syncookies",
    "net.ipv4.ping_group_range",
    "net.ipv4.ip_unprivileged_port_start",
    "net.ipv4.ip_local_reserved_ports",
    "net.ipv4.tcp_keepalive_time",
    "net.ipv4.tcp_fin_timeout",
    "net.ipv4.tcp_keepalive_intvl",
    "net.ipv4.tcp_keepalive_probes",
    "net.ipv4.tcp_rmem",
    "net.ipv4.tcp_wmem",
];

fn check_sysctls(pod: &Value) -> Option<String> {
    let forbidden: Vec<String> = pod
        .get("spec")
        .and_then(|s| s.get("securityContext"))
        .and_then(|sc| sc.get("sysctls"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("name").and_then(Value::as_str))
        .filter(|name| !ALLOWED_SYSCTLS.contains(name))
        .map(str::to_string)
        .collect();
    if forbidden.is_empty() {
        None
    } else {
        Some(format!("forbidden sysctls: {}", forbidden.join(", ")))
    }
}

/// Real upstream's own `relaxPolicyForUserNamespacePod` +
/// `procMount_1_0`: a pod opting into a user namespace (`hostUsers:
/// false`) has every `procMount` value allowed (upstream's own comment:
/// pod validation already checks for a well-formed type there, so this
/// deliberately doesn't double-validate) — this crate has no
/// `UserNamespacesPodSecurityStandards` feature-gate machinery, so the
/// relaxation is unconditional on `hostUsers`, not gated behind a
/// feature flag that doesn't exist here.
fn check_proc_mount(pod: &Value) -> Option<String> {
    if pod.get("spec").and_then(|s| s.get("hostUsers")).and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let mut bad_containers = Vec::new();
    let mut bad_types = std::collections::BTreeSet::new();
    for container in containers(pod) {
        let Some(t) = container.get("securityContext").and_then(|sc| sc.get("procMount")).and_then(Value::as_str) else { continue };
        if t != "Default" {
            bad_containers.push(container_name(container).to_string());
            bad_types.insert(t.to_string());
        }
    }
    if bad_containers.is_empty() {
        None
    } else {
        Some(format!("procMount: container(s) {} must not set securityContext.procMount to {}", bad_containers.join(", "), bad_types.into_iter().collect::<Vec<_>>().join(", ")))
    }
}

fn forbidden_probe_hosts(probe: &Value) -> Vec<String> {
    let mut hosts = Vec::new();
    for kind in ["httpGet", "tcpSocket"] {
        if let Some(host) = probe.get(kind).and_then(|g| g.get("host")).and_then(Value::as_str) {
            if !host.is_empty() {
                hosts.push(host.to_string());
            }
        }
    }
    hosts
}

fn check_host_probes_and_host_lifecycle(pod: &Value) -> Option<String> {
    let mut bad_containers = std::collections::BTreeSet::new();
    let mut forbidden = std::collections::BTreeSet::new();
    for container in containers(pod) {
        for probe_field in ["livenessProbe", "readinessProbe", "startupProbe"] {
            if let Some(probe) = container.get(probe_field) {
                let hosts = forbidden_probe_hosts(probe);
                if !hosts.is_empty() {
                    bad_containers.insert(container_name(container).to_string());
                    forbidden.extend(hosts);
                }
            }
        }
        if let Some(lifecycle) = container.get("lifecycle") {
            for handler_field in ["postStart", "preStop"] {
                if let Some(handler) = lifecycle.get(handler_field) {
                    let hosts = forbidden_probe_hosts(handler);
                    if !hosts.is_empty() {
                        bad_containers.insert(container_name(container).to_string());
                        forbidden.extend(hosts);
                    }
                }
            }
        }
    }
    if bad_containers.is_empty() {
        None
    } else {
        Some(format!(
            "probe or lifecycle host: container(s) {} use probe or lifecycle host(s) {}",
            bad_containers.into_iter().collect::<Vec<_>>().join(", "),
            forbidden.into_iter().collect::<Vec<_>>().join(", ")
        ))
    }
}

fn check_windows_host_process(pod: &Value) -> Option<String> {
    let host_process = |sc: &Value| sc.get("windowsOptions").and_then(|w| w.get("hostProcess")).and_then(Value::as_bool) == Some(true);

    let pod_forbidden = pod.get("spec").and_then(|s| s.get("securityContext")).is_some_and(host_process);
    let bad_containers: Vec<String> = containers(pod).filter(|c| c.get("securityContext").is_some_and(host_process)).map(|c| container_name(c).to_string()).collect();

    let mut setters = Vec::new();
    if pod_forbidden {
        setters.push("pod".to_string());
    }
    if !bad_containers.is_empty() {
        setters.push(format!("container(s) {}", bad_containers.join(", ")));
    }
    if setters.is_empty() {
        None
    } else {
        Some(format!("hostProcess: {} must not set securityContext.windowsOptions.hostProcess=true", setters.join(" and ")))
    }
}

fn allowed_apparmor_profile_type(t: &str) -> bool {
    t == "RuntimeDefault" || t == "Localhost"
}

const APPARMOR_ANNOTATION_PREFIX: &str = "container.apparmor.security.beta.kubernetes.io/";

fn allowed_apparmor_annotation_value(v: &str) -> bool {
    v.is_empty() || v == "runtime/default" || v.starts_with("localhost/")
}

fn check_apparmor_profile(pod: &Value) -> Option<String> {
    let apparmor_type = |sc: &Value| sc.get("appArmorProfile").and_then(|p| p.get("type")).and_then(Value::as_str);

    let mut bad_setters = Vec::new();
    let mut bad_values = std::collections::BTreeSet::new();

    if let Some(t) = pod.get("spec").and_then(|s| s.get("securityContext")).and_then(apparmor_type) {
        if !allowed_apparmor_profile_type(t) {
            bad_setters.push("pod".to_string());
            bad_values.insert(t.to_string());
        }
    }

    let bad_containers: Vec<String> = containers(pod)
        .filter_map(|c| {
            let t = c.get("securityContext").and_then(apparmor_type)?;
            if allowed_apparmor_profile_type(t) {
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

    let mut forbidden_annotations: Vec<String> = pod
        .get("metadata")
        .and_then(|m| m.get("annotations"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(k, _)| k.starts_with(APPARMOR_ANNOTATION_PREFIX))
        .filter_map(|(k, v)| {
            let v = v.as_str()?;
            (!allowed_apparmor_annotation_value(v)).then(|| format!("{k}={v:?}"))
        })
        .collect();
    if !forbidden_annotations.is_empty() {
        forbidden_annotations.sort();
        bad_setters.push(if forbidden_annotations.len() == 1 { "annotation".to_string() } else { "annotations".to_string() });
        bad_values.extend(forbidden_annotations);
    }

    if bad_setters.is_empty() {
        None
    } else {
        Some(format!("forbidden AppArmor profile: {} must not set AppArmor profile type to {}", bad_setters.join(" and "), bad_values.into_iter().collect::<Vec<_>>().join(", ")))
    }
}

/// Real upstream's own `selinuxAllowedTypes1_31` — the widest (newest)
/// allowed set upstream has defined.
const ALLOWED_SELINUX_TYPES: &[&str] = &["", "container_t", "container_init_t", "container_kvm_t", "container_engine_t"];

fn check_selinux_options(pod: &Value) -> Option<String> {
    let mut bad_setters = Vec::new();
    let mut bad_types = std::collections::BTreeSet::new();
    let mut set_user = false;
    let mut set_role = false;

    let mut check_one = |opts: &Value, setter: &str, setters: &mut Vec<String>| {
        let t = opts.get("type").and_then(Value::as_str).unwrap_or("");
        let mut valid = true;
        if !ALLOWED_SELINUX_TYPES.contains(&t) {
            valid = false;
            bad_types.insert(t.to_string());
        }
        if opts.get("user").and_then(Value::as_str).is_some_and(|u| !u.is_empty()) {
            valid = false;
            set_user = true;
        }
        if opts.get("role").and_then(Value::as_str).is_some_and(|r| !r.is_empty()) {
            valid = false;
            set_role = true;
        }
        if !valid {
            setters.push(setter.to_string());
        }
    };

    if let Some(opts) = pod.get("spec").and_then(|s| s.get("securityContext")).and_then(|sc| sc.get("seLinuxOptions")) {
        check_one(opts, "pod", &mut bad_setters);
    }
    let mut bad_containers = Vec::new();
    for container in containers(pod) {
        if let Some(opts) = container.get("securityContext").and_then(|sc| sc.get("seLinuxOptions")) {
            check_one(opts, container_name(container), &mut bad_containers);
        }
    }
    if !bad_containers.is_empty() {
        bad_setters.push(format!("container(s) {}", bad_containers.join(", ")));
    }

    if bad_setters.is_empty() {
        return None;
    }
    let mut detail = Vec::new();
    if !bad_types.is_empty() {
        detail.push(format!("type(s) {}", bad_types.into_iter().collect::<Vec<_>>().join(", ")));
    }
    if set_user {
        detail.push("user may not be set".to_string());
    }
    if set_role {
        detail.push("role may not be set".to_string());
    }
    Some(format!("seLinuxOptions: {} set forbidden securityContext.seLinuxOptions: {}", bad_setters.join(" and "), detail.join("; ")))
}

/// Every landed `baseline`-level check, run in real upstream's own file
/// order — collects every failing check's message rather than stopping
/// at the first (matching `PodValidateLimitFunc`'s own "aggregate every
/// violation" posture, and real PSA's own `AggregateCheckResults`).
fn baseline_violations(pod: &Value) -> Vec<String> {
    [
        check_privileged(pod),
        check_host_namespaces(pod),
        check_host_ports(pod),
        check_host_path_volumes(pod),
        check_capabilities_baseline(pod),
        check_seccomp_profile_baseline(pod),
        check_sysctls(pod),
        check_proc_mount(pod),
        check_host_probes_and_host_lifecycle(pod),
        check_windows_host_process(pod),
        check_apparmor_profile(pod),
        check_selinux_options(pod),
    ]
    .into_iter()
    .flatten()
    .collect()
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
    fn baseline_rejects_a_forbidden_sysctl() {
        let pod = json!({"spec": {"containers": [], "securityContext": {"sysctls": [{"name": "kernel.msgmax", "value": "1"}]}}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("kernel.msgmax"));
    }

    #[test]
    fn baseline_allows_a_safe_sysctl() {
        let pod = json!({"spec": {"containers": [], "securityContext": {"sysctls": [{"name": "net.ipv4.tcp_syncookies", "value": "1"}]}}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_rejects_a_non_default_proc_mount() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"procMount": "Unmasked"}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("Unmasked"));
    }

    #[test]
    fn proc_mount_is_unenforced_for_a_user_namespace_pod() {
        let pod = json!({"spec": {"hostUsers": false, "containers": [{"name": "c1", "securityContext": {"procMount": "Unmasked"}}]}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_rejects_a_probe_with_a_host_field() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "livenessProbe": {"httpGet": {"host": "169.254.169.254", "path": "/"}}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("169.254.169.254"));
    }

    #[test]
    fn baseline_allows_a_probe_with_no_host_field() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "livenessProbe": {"httpGet": {"path": "/"}}}]}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_rejects_a_lifecycle_handler_with_a_host_field() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "lifecycle": {"preStop": {"httpGet": {"host": "evil", "path": "/"}}}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn baseline_rejects_windows_host_process() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"windowsOptions": {"hostProcess": true}}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("hostProcess"));
    }

    #[test]
    fn baseline_rejects_a_pod_level_windows_host_process() {
        let pod = json!({"spec": {"containers": [], "securityContext": {"windowsOptions": {"hostProcess": true}}}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("pod"));
    }

    #[test]
    fn baseline_rejects_an_unconfined_apparmor_profile() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"appArmorProfile": {"type": "Unconfined"}}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("Unconfined"));
    }

    #[test]
    fn baseline_allows_runtime_default_apparmor() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"appArmorProfile": {"type": "RuntimeDefault"}}}]}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_rejects_a_forbidden_apparmor_annotation() {
        let pod = json!({
            "metadata": {"annotations": {"container.apparmor.security.beta.kubernetes.io/c1": "unconfined"}},
            "spec": {"containers": [{"name": "c1"}]},
        });
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn baseline_rejects_a_custom_selinux_type() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"seLinuxOptions": {"type": "spc_t"}}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("spc_t"));
    }

    #[test]
    fn baseline_allows_an_approved_selinux_type() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"seLinuxOptions": {"type": "container_t"}}}]}});
        assert!(validate(&pod, Level::Baseline).is_empty());
    }

    #[test]
    fn baseline_rejects_a_selinux_user_or_role() {
        let pod = json!({"spec": {"containers": [{"name": "c1", "securityContext": {"seLinuxOptions": {"user": "custom_u"}}}]}});
        let violations = validate(&pod, Level::Baseline);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("user may not be set"));
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
