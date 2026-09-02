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
//! plugin doesn't cover because it applies only to top-level Pod `CREATE`,
//! while the subresource has its own update strategy), `seLinuxOptions` (the 1.31+ allowed-type set,
//! which is the widest one upstream has defined).
//!
//! **All six real `restricted`-level checks are ported too**:
//! `runAsNonRoot` (the real three-way pod/container logic — a container
//! that leaves it unset inherits an explicit pod-level `true`),
//! `runAsUser` (forbids `runAsUser=0`), `allowPrivilegeEscalation`
//! (Windows-exempt, matching upstream's own 1.25+ variant — Pod API
//! validation already rejects the field on a Windows pod),
//! `capabilities_restricted` (must drop `ALL`, may only add
//! `NET_BIND_SERVICE`; Windows-exempt too), `seccompProfile_restricted`
//! (same three-way pod/container logic as `runAsNonRoot`, Windows-exempt),
//! `restrictedVolumes` (the real inline-volume-source allowlist).
//! Real upstream's own `OverrideCheckIDs` is ported too: at `Restricted`,
//! `hostPathVolumes`/`capabilities_baseline`/`seccompProfile_baseline`
//! are suppressed in favor of their strictly-stronger restricted
//! equivalents (`restrictedVolumes`/`capabilities_restricted`/
//! `seccompProfile_restricted`), so a `Restricted`-level violation isn't
//! reported twice for the same root cause.
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

pub fn applies_to(
    group: &str,
    resource: &str,
    subresource: &str,
    operation: crate::admission::attributes::Operation,
) -> bool {
    group.is_empty()
        && resource == "pods"
        && subresource.is_empty()
        && operation == crate::admission::attributes::Operation::Create
}

/// Real upstream's own label, read off the target namespace object —
/// `None`/anything but `"baseline"`/`"restricted"` means `Privileged`
/// (upstream's own "no restriction" default, including a genuinely
/// unrecognized value — same "don't fail open into a stricter mode than
/// requested, but don't silently reject an unrecognized label either"
/// posture as leaving it unenforced).
pub fn enforcement_level(namespace: &Value) -> Level {
    match namespace
        .get("metadata")
        .and_then(|m| m.get("labels"))
        .and_then(|l| l.get(ENFORCE_LABEL))
        .and_then(Value::as_str)
    {
        Some("baseline") => Level::Baseline,
        Some("restricted") => Level::Restricted,
        _ => Level::Privileged,
    }
}

fn containers(pod: &Value) -> impl Iterator<Item = &Value> {
    let spec = pod.get("spec");
    let containers = spec
        .and_then(|s| s.get("containers"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let init_containers = spec
        .and_then(|s| s.get("initContainers"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    containers.chain(init_containers)
}

fn container_name(container: &Value) -> &str {
    container.get("name").and_then(Value::as_str).unwrap_or("")
}

fn check_privileged(pod: &Value) -> Option<String> {
    let bad: Vec<&str> = containers(pod)
        .filter(|c| {
            c.get("securityContext")
                .and_then(|sc| sc.get("privileged"))
                .and_then(Value::as_bool)
                == Some(true)
        })
        .map(container_name)
        .collect();
    if bad.is_empty() {
        None
    } else {
        Some(format!(
            "privileged: container(s) {} must not set securityContext.privileged=true",
            bad.join(", ")
        ))
    }
}

fn check_host_namespaces(pod: &Value) -> Option<String> {
    let spec = pod.get("spec");
    let mut bad = Vec::new();
    if spec
        .and_then(|s| s.get("hostNetwork"))
        .and_then(Value::as_bool)
        == Some(true)
    {
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
        let ports = container
            .get("ports")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
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
        Some(format!(
            "hostPort: container(s) {} use hostPort(s) {}",
            bad_containers.join(", "),
            bad_ports.into_iter().collect::<Vec<_>>().join(", ")
        ))
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
        .map(|v| {
            v.get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        })
        .collect();
    if bad.is_empty() {
        None
    } else {
        Some(format!("hostPath volumes: {}", bad.join(", ")))
    }
}

/// Real upstream's own `capabilities_allowed_1_0` set, ported exactly.
const ALLOWED_CAPABILITIES: &[&str] = &[
    "AUDIT_WRITE",
    "CHOWN",
    "DAC_OVERRIDE",
    "FOWNER",
    "FSETID",
    "KILL",
    "MKNOD",
    "NET_BIND_SERVICE",
    "SETFCAP",
    "SETGID",
    "SETPCAP",
    "SETUID",
    "SYS_CHROOT",
];

fn check_capabilities_baseline(pod: &Value) -> Option<String> {
    let mut bad_containers = Vec::new();
    let mut bad_caps = std::collections::BTreeSet::new();
    for container in containers(pod) {
        let Some(add) = container
            .get("securityContext")
            .and_then(|sc| sc.get("capabilities"))
            .and_then(|c| c.get("add"))
            .and_then(Value::as_array)
        else {
            continue;
        };
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
        Some(format!(
            "non-default capabilities: container(s) {} must not include {} in securityContext.capabilities.add",
            bad_containers.join(", "),
            bad_caps.into_iter().collect::<Vec<_>>().join(", ")
        ))
    }
}

fn valid_seccomp_type(t: &str) -> bool {
    t == "RuntimeDefault" || t == "Localhost"
}

fn check_seccomp_profile_baseline(pod: &Value) -> Option<String> {
    let mut bad_setters = Vec::new();
    let mut bad_values = std::collections::BTreeSet::new();

    if let Some(t) = pod
        .get("spec")
        .and_then(|s| s.get("securityContext"))
        .and_then(|sc| sc.get("seccompProfile"))
        .and_then(|sp| sp.get("type"))
        .and_then(Value::as_str)
    {
        if !valid_seccomp_type(t) {
            bad_setters.push("pod".to_string());
            bad_values.insert(t.to_string());
        }
    }

    let bad_containers: Vec<String> = containers(pod)
        .filter_map(|c| {
            let t = c
                .get("securityContext")
                .and_then(|sc| sc.get("seccompProfile"))
                .and_then(|sp| sp.get("type"))
                .and_then(Value::as_str)?;
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
        Some(format!(
            "seccompProfile: {} must not set securityContext.seccompProfile.type to {}",
            bad_setters.join(" and "),
            bad_values.into_iter().collect::<Vec<_>>().join(", ")
        ))
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

/// Real upstream's own `relaxPolicyForUserNamespacePod` — a pod opting
/// into a user namespace (`hostUsers: false`) relaxes several checks
/// (this crate has no `UserNamespacesPodSecurityStandards` feature-gate
/// machinery, so the relaxation is unconditional on `hostUsers`, not
/// gated behind a feature flag that doesn't exist here).
fn relax_for_user_namespace_pod(pod: &Value) -> bool {
    pod.get("spec")
        .and_then(|s| s.get("hostUsers"))
        .and_then(Value::as_bool)
        == Some(false)
}

/// Real upstream's own `podSpec.OS != nil && podSpec.OS.Name ==
/// corev1.Windows` — several restricted-level checks (fields real
/// upstream's own Pod API validation already forbids on a Windows pod)
/// exempt one, ported exactly.
fn is_windows_pod(pod: &Value) -> bool {
    pod.get("spec")
        .and_then(|s| s.get("os"))
        .and_then(|os| os.get("name"))
        .and_then(Value::as_str)
        == Some("windows")
}

/// `procMount_1_0`: a pod opting into a user namespace has every
/// `procMount` value allowed (upstream's own comment: pod validation
/// already checks for a well-formed type there, so this deliberately
/// doesn't double-validate).
fn check_proc_mount(pod: &Value) -> Option<String> {
    if relax_for_user_namespace_pod(pod) {
        return None;
    }
    let mut bad_containers = Vec::new();
    let mut bad_types = std::collections::BTreeSet::new();
    for container in containers(pod) {
        let Some(t) = container
            .get("securityContext")
            .and_then(|sc| sc.get("procMount"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if t != "Default" {
            bad_containers.push(container_name(container).to_string());
            bad_types.insert(t.to_string());
        }
    }
    if bad_containers.is_empty() {
        None
    } else {
        Some(format!(
            "procMount: container(s) {} must not set securityContext.procMount to {}",
            bad_containers.join(", "),
            bad_types.into_iter().collect::<Vec<_>>().join(", ")
        ))
    }
}

fn forbidden_probe_hosts(probe: &Value) -> Vec<String> {
    let mut hosts = Vec::new();
    for kind in ["httpGet", "tcpSocket"] {
        if let Some(host) = probe
            .get(kind)
            .and_then(|g| g.get("host"))
            .and_then(Value::as_str)
        {
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
    let host_process = |sc: &Value| {
        sc.get("windowsOptions")
            .and_then(|w| w.get("hostProcess"))
            .and_then(Value::as_bool)
            == Some(true)
    };

    let pod_forbidden = pod
        .get("spec")
        .and_then(|s| s.get("securityContext"))
        .is_some_and(host_process);
    let bad_containers: Vec<String> = containers(pod)
        .filter(|c| c.get("securityContext").is_some_and(host_process))
        .map(|c| container_name(c).to_string())
        .collect();

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
        Some(format!(
            "hostProcess: {} must not set securityContext.windowsOptions.hostProcess=true",
            setters.join(" and ")
        ))
    }
}

fn allowed_apparmor_profile_type(t: &str) -> bool {
    t == "RuntimeDefault" || t == "Localhost"
}

const APPARMOR_ANNOTATION_PREFIX: &str = "container.apparmor.security.beta.kubernetes.io/";

fn allowed_apparmor_annotation_value(v: &str) -> bool {
    v.is_empty() || v == "runtime/default" || v.starts_with("localhost/")
}

/// A free function, not a closure — a closure's elided-lifetime
/// inference can't express "generic over the input's own lifetime" for
/// a fn that returns a reference borrowed from its argument (it infers
/// one fixed lifetime from the first call site instead), which breaks
/// reusing it across borrows of different lifetimes below (the pod's own
/// `securityContext` vs. each container's, from separate iterator
/// borrows) — a real compile error CI caught, not a style choice.
fn apparmor_type(sc: &Value) -> Option<&str> {
    sc.get("appArmorProfile")
        .and_then(|p| p.get("type"))
        .and_then(Value::as_str)
}

fn check_apparmor_profile(pod: &Value) -> Option<String> {
    let mut bad_setters = Vec::new();
    let mut bad_values = std::collections::BTreeSet::new();

    if let Some(t) = pod
        .get("spec")
        .and_then(|s| s.get("securityContext"))
        .and_then(apparmor_type)
    {
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
        bad_setters.push(if forbidden_annotations.len() == 1 {
            "annotation".to_string()
        } else {
            "annotations".to_string()
        });
        bad_values.extend(forbidden_annotations);
    }

    if bad_setters.is_empty() {
        None
    } else {
        Some(format!(
            "forbidden AppArmor profile: {} must not set AppArmor profile type to {}",
            bad_setters.join(" and "),
            bad_values.into_iter().collect::<Vec<_>>().join(", ")
        ))
    }
}

/// Real upstream's own `selinuxAllowedTypes1_31` — the widest (newest)
/// allowed set upstream has defined.
const ALLOWED_SELINUX_TYPES: &[&str] = &[
    "",
    "container_t",
    "container_init_t",
    "container_kvm_t",
    "container_engine_t",
];

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
        if opts
            .get("user")
            .and_then(Value::as_str)
            .is_some_and(|u| !u.is_empty())
        {
            valid = false;
            set_user = true;
        }
        if opts
            .get("role")
            .and_then(Value::as_str)
            .is_some_and(|r| !r.is_empty())
        {
            valid = false;
            set_role = true;
        }
        if !valid {
            setters.push(setter.to_string());
        }
    };

    if let Some(opts) = pod
        .get("spec")
        .and_then(|s| s.get("securityContext"))
        .and_then(|sc| sc.get("seLinuxOptions"))
    {
        check_one(opts, "pod", &mut bad_setters);
    }
    let mut bad_containers = Vec::new();
    for container in containers(pod) {
        if let Some(opts) = container
            .get("securityContext")
            .and_then(|sc| sc.get("seLinuxOptions"))
        {
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
        detail.push(format!(
            "type(s) {}",
            bad_types.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if set_user {
        detail.push("user may not be set".to_string());
    }
    if set_role {
        detail.push("role may not be set".to_string());
    }
    Some(format!(
        "seLinuxOptions: {} set forbidden securityContext.seLinuxOptions: {}",
        bad_setters.join(" and "),
        detail.join("; ")
    ))
}

/// `runAsNonRoot_1_0`: real upstream's own three-way "explicit
/// pod-level false / explicit container-level false / neither pod nor
/// container opted in" logic, ported exactly (the pod-level `true`
/// exemption for a container that leaves it unset is the "undefined/null
/// at container-level if pod-level is set to true" allowed value from
/// upstream's own doc comment).
fn check_run_as_non_root(pod: &Value) -> Option<String> {
    if relax_for_user_namespace_pod(pod) {
        return None;
    }
    let pod_run_as_non_root = pod
        .get("spec")
        .and_then(|s| s.get("securityContext"))
        .and_then(|sc| sc.get("runAsNonRoot"))
        .and_then(Value::as_bool);

    let mut bad_setters = Vec::new();
    if pod_run_as_non_root == Some(false) {
        bad_setters.push("pod".to_string());
    }
    let pod_opted_in = pod_run_as_non_root == Some(true);

    let mut explicitly_bad = Vec::new();
    let mut implicitly_bad = Vec::new();
    for container in containers(pod) {
        match container
            .get("securityContext")
            .and_then(|sc| sc.get("runAsNonRoot"))
            .and_then(Value::as_bool)
        {
            Some(false) => explicitly_bad.push(container_name(container).to_string()),
            Some(true) => {}
            None => {
                if !pod_opted_in {
                    implicitly_bad.push(container_name(container).to_string());
                }
            }
        }
    }
    if !explicitly_bad.is_empty() {
        bad_setters.push(format!("container(s) {}", explicitly_bad.join(", ")));
    }
    if !bad_setters.is_empty() {
        return Some(format!(
            "runAsNonRoot != true: {} must not set securityContext.runAsNonRoot=false",
            bad_setters.join(" and ")
        ));
    }
    if !implicitly_bad.is_empty() {
        return Some(format!(
            "runAsNonRoot != true: pod or container(s) {} must set securityContext.runAsNonRoot=true",
            implicitly_bad.join(", ")
        ));
    }
    None
}

fn check_run_as_user(pod: &Value) -> Option<String> {
    if relax_for_user_namespace_pod(pod) {
        return None;
    }
    let mut bad_setters = Vec::new();
    if pod
        .get("spec")
        .and_then(|s| s.get("securityContext"))
        .and_then(|sc| sc.get("runAsUser"))
        .and_then(Value::as_i64)
        == Some(0)
    {
        bad_setters.push("pod".to_string());
    }
    let bad_containers: Vec<String> = containers(pod)
        .filter(|c| {
            c.get("securityContext")
                .and_then(|sc| sc.get("runAsUser"))
                .and_then(Value::as_i64)
                == Some(0)
        })
        .map(|c| container_name(c).to_string())
        .collect();
    if !bad_containers.is_empty() {
        bad_setters.push(format!("container(s) {}", bad_containers.join(", ")));
    }
    if bad_setters.is_empty() {
        None
    } else {
        Some(format!(
            "runAsUser=0: {} must not set runAsUser=0",
            bad_setters.join(" and ")
        ))
    }
}

/// `allowPrivilegeEscalation_1_25`: exempts a Windows pod entirely
/// (upstream's own comment: Pod API validation already rejects the field
/// being set on a Windows pod, so an unset value is fine to admit).
fn check_allow_privilege_escalation(pod: &Value) -> Option<String> {
    if is_windows_pod(pod) {
        return None;
    }
    let bad: Vec<String> = containers(pod)
        .filter(|c| {
            c.get("securityContext")
                .and_then(|sc| sc.get("allowPrivilegeEscalation"))
                .and_then(Value::as_bool)
                != Some(false)
        })
        .map(|c| container_name(c).to_string())
        .collect();
    if bad.is_empty() {
        None
    } else {
        Some(format!(
            "allowPrivilegeEscalation != false: container(s) {} must set securityContext.allowPrivilegeEscalation=false",
            bad.join(", ")
        ))
    }
}

const CAPABILITY_ALL: &str = "ALL";
const CAPABILITY_NET_BIND_SERVICE: &str = "NET_BIND_SERVICE";

/// `capabilitiesRestricted_1_25`: also Windows-exempt (same reasoning as
/// [`check_allow_privilege_escalation`]). Overrides
/// [`check_capabilities_baseline`] at the `Restricted` level (real
/// upstream's own `OverrideCheckIDs`) — it's a strict superset
/// requirement, so both would otherwise report overlapping violations
/// for the same root cause.
fn check_capabilities_restricted(pod: &Value) -> Option<String> {
    if is_windows_pod(pod) {
        return None;
    }
    let mut missing_drop_all = Vec::new();
    let mut adding_forbidden = Vec::new();
    let mut forbidden_caps = std::collections::BTreeSet::new();

    for container in containers(pod) {
        let capabilities = container
            .get("securityContext")
            .and_then(|sc| sc.get("capabilities"));
        let dropped_all = capabilities
            .and_then(|c| c.get("drop"))
            .and_then(Value::as_array)
            .is_some_and(|drop| drop.iter().any(|c| c.as_str() == Some(CAPABILITY_ALL)));
        if !dropped_all {
            missing_drop_all.push(container_name(container).to_string());
        }
        let mut added_forbidden = false;
        for cap in capabilities
            .and_then(|c| c.get("add"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(name) = cap.as_str() {
                if name != CAPABILITY_NET_BIND_SERVICE {
                    added_forbidden = true;
                    forbidden_caps.insert(name.to_string());
                }
            }
        }
        if added_forbidden {
            adding_forbidden.push(container_name(container).to_string());
        }
    }

    let mut details = Vec::new();
    if !missing_drop_all.is_empty() {
        details.push(format!(
            r#"container(s) {} must set securityContext.capabilities.drop=["ALL"]"#,
            missing_drop_all.join(", ")
        ));
    }
    if !adding_forbidden.is_empty() {
        details.push(format!(
            "container(s) {} must not include {} in securityContext.capabilities.add",
            adding_forbidden.join(", "),
            forbidden_caps.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if details.is_empty() {
        None
    } else {
        Some(format!("unrestricted capabilities: {}", details.join("; ")))
    }
}

/// `seccompProfileRestricted_1_19`/`_1_25`: same three-way pod/container
/// logic as [`check_run_as_non_root`], and Windows-exempt like
/// [`check_allow_privilege_escalation`]. Overrides
/// [`check_seccomp_profile_baseline`] at the `Restricted` level.
fn check_seccomp_profile_restricted(pod: &Value) -> Option<String> {
    if is_windows_pod(pod) {
        return None;
    }
    let pod_type = pod
        .get("spec")
        .and_then(|s| s.get("securityContext"))
        .and_then(|sc| sc.get("seccompProfile"))
        .and_then(|sp| sp.get("type"))
        .and_then(Value::as_str);

    let mut bad_setters = Vec::new();
    let mut bad_values = std::collections::BTreeSet::new();
    let mut pod_seccomp_set = false;
    if let Some(t) = pod_type {
        if !valid_seccomp_type(t) {
            bad_setters.push("pod".to_string());
            bad_values.insert(t.to_string());
        } else {
            pod_seccomp_set = true;
        }
    }

    let mut explicitly_bad = Vec::new();
    let mut implicitly_bad = Vec::new();
    for container in containers(pod) {
        match container
            .get("securityContext")
            .and_then(|sc| sc.get("seccompProfile"))
            .and_then(|sp| sp.get("type"))
            .and_then(Value::as_str)
        {
            Some(t) if !valid_seccomp_type(t) => {
                explicitly_bad.push(container_name(container).to_string());
                bad_values.insert(t.to_string());
            }
            Some(_) => {}
            None => {
                if !pod_seccomp_set {
                    implicitly_bad.push(container_name(container).to_string());
                }
            }
        }
    }
    if !explicitly_bad.is_empty() {
        bad_setters.push(format!("container(s) {}", explicitly_bad.join(", ")));
    }
    if !bad_setters.is_empty() {
        return Some(format!(
            "seccompProfile: {} must not set securityContext.seccompProfile.type to {}",
            bad_setters.join(" and "),
            bad_values.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if !implicitly_bad.is_empty() {
        return Some(format!(
            r#"seccompProfile: pod or container(s) {} must set securityContext.seccompProfile.type to "RuntimeDefault" or "Localhost""#,
            implicitly_bad.join(", ")
        ));
    }
    None
}

/// `restrictedVolumes_1_0`: real upstream's own allowlist of inline
/// volume sources, ported exactly (`image` is real upstream's newer
/// `VolumeSource.Image` — this crate's vendored OpenAPI spec is checked
/// against the same release, so it's included here too). Overrides
/// [`check_host_path_volumes`] at the `Restricted` level — it's a strict
/// superset (every restricted volume type it forbids includes
/// `hostPath`).
fn check_restricted_volumes(pod: &Value) -> Option<String> {
    const ALLOWED_VOLUME_SOURCES: &[&str] = &[
        "configMap",
        "csi",
        "downwardAPI",
        "emptyDir",
        "ephemeral",
        "image",
        "persistentVolumeClaim",
        "projected",
        "secret",
    ];
    let mut bad_volumes = Vec::new();
    let mut bad_types = std::collections::BTreeSet::new();
    for volume in pod
        .get("spec")
        .and_then(|s| s.get("volumes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(obj) = volume.as_object() else {
            continue;
        };
        let source_key = obj.keys().find(|k| k.as_str() != "name");
        let Some(source_key) = source_key else {
            continue;
        };
        if ALLOWED_VOLUME_SOURCES.contains(&source_key.as_str()) {
            continue;
        }
        bad_volumes.push(
            volume
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        );
        bad_types.insert(source_key.clone());
    }
    if bad_volumes.is_empty() {
        None
    } else {
        Some(format!(
            "restricted volume types: volume(s) {} use restricted volume type(s) {}",
            bad_volumes.join(", "),
            bad_types.into_iter().collect::<Vec<_>>().join(", ")
        ))
    }
}

include!("pod_security_validation.rs");
include!("pod_security_tests.rs");
