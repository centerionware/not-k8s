//! build_mounts(): this is the fix for the actual coredns crash (not the
//! pile-up symptom) — nodelet never set ContainerConfig.mounts at all
//! before this. Covers subPath joining, readOnly defaulting, and silently
//! dropping a mount whose volume didn't resolve (unsupported type, or a
//! failed ConfigMap/Secret fetch) rather than pointing CRI at a path that
//! doesn't exist.
use super::*;
use k8s_openapi::api::core::v1::VolumeMount;

fn vm(name: &str, mount_path: &str) -> VolumeMount {
    VolumeMount { name: name.to_string(), mount_path: mount_path.to_string(), ..Default::default() }
}

#[test]
fn empty_mounts_list_produces_no_mounts() {
    let volumes = HashMap::new();
    assert!(build_mounts(&[], &volumes, &[]).is_empty());
}

#[test]
fn resolves_a_simple_mount_to_its_volume_directory() {
    let mut volumes = HashMap::new();
    volumes.insert("config-volume".to_string(), ResolvedVolume::HostPath(PathBuf::from("/var/lib/nodelet/pods/uid1/volumes/config-volume")));
    let mounts = build_mounts(&[vm("config-volume", "/etc/coredns")], &volumes, &[]);
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].container_path, "/etc/coredns");
    assert_eq!(mounts[0].host_path, "/var/lib/nodelet/pods/uid1/volumes/config-volume");
}

#[test]
fn mount_naming_an_unresolved_volume_is_dropped_not_errored() {
    // e.g. a projected/serviceAccountToken volume resolve_volumes() didn't
    // materialize — must not produce a Mount pointing at a nonexistent path.
    let volumes = HashMap::new();
    let mounts = build_mounts(&[vm("kube-api-access", "/var/run/secrets/kubernetes.io/serviceaccount")], &volumes, &[]);
    assert!(mounts.is_empty());
}

#[test]
fn sub_path_is_joined_onto_the_volume_directory() {
    let mut volumes = HashMap::new();
    volumes.insert("config-volume".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/config-volume")));
    let mut mount = vm("config-volume", "/etc/coredns/Corefile");
    mount.sub_path = Some("Corefile".to_string());
    let mounts = build_mounts(&[mount], &volumes, &[]);
    assert_eq!(mounts[0].host_path, "/vol/config-volume/Corefile");
}

#[test]
fn read_only_true_is_propagated() {
    let mut volumes = HashMap::new();
    volumes.insert("v".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/v")));
    let mut mount = vm("v", "/etc/v");
    mount.read_only = Some(true);
    let mounts = build_mounts(&[mount], &volumes, &[]);
    assert!(mounts[0].readonly);
}

#[test]
fn read_only_unset_defaults_to_false() {
    let mut volumes = HashMap::new();
    volumes.insert("v".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/v")));
    let mounts = build_mounts(&[vm("v", "/etc/v")], &volumes, &[]);
    assert!(!mounts[0].readonly);
}

#[test]
fn read_only_explicit_false_stays_false() {
    let mut volumes = HashMap::new();
    volumes.insert("v".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/v")));
    let mut mount = vm("v", "/etc/v");
    mount.read_only = Some(false);
    let mounts = build_mounts(&[mount], &volumes, &[]);
    assert!(!mounts[0].readonly);
}

#[test]
fn multiple_mounts_each_resolve_independently() {
    let mut volumes = HashMap::new();
    volumes.insert("a".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/a")));
    volumes.insert("b".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/b")));
    let mounts = build_mounts(&[vm("a", "/mnt/a"), vm("b", "/mnt/b"), vm("c", "/mnt/c")], &volumes, &[]);
    // "c" has no matching volume — dropped, leaving exactly the two resolvable ones.
    assert_eq!(mounts.len(), 2);
    assert!(mounts.iter().any(|m| m.container_path == "/mnt/a" && m.host_path == "/vol/a"));
    assert!(mounts.iter().any(|m| m.container_path == "/mnt/b" && m.host_path == "/vol/b"));
}

#[test]
fn coredns_shaped_mount_matches_the_real_crash_scenario() {
    // The exact case that motivated this whole file: a ConfigMap volume
    // mounted at /etc/coredns, containing a Corefile.
    let mut volumes = HashMap::new();
    volumes.insert("config-volume".to_string(), ResolvedVolume::HostPath(PathBuf::from("/var/lib/nodelet/pods/abc/volumes/config-volume")));
    let mounts = build_mounts(&[vm("config-volume", "/etc/coredns")], &volumes, &[]);
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].container_path, "/etc/coredns");
    assert!(!mounts[0].host_path.is_empty());
}

// --- volumeSource.image (round 32) ---

#[test]
fn image_volume_sets_the_image_field_not_a_host_path() {
    let mut volumes = HashMap::new();
    volumes.insert("config-image".to_string(), ResolvedVolume::Image { image_ref: "docker.io/library/nginx@sha256:abc".to_string() });
    let mounts = build_mounts(&[vm("config-image", "/etc/nginx")], &volumes, &[]);
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].host_path, "");
    assert!(mounts[0].readonly, "image volumes must always be readonly");
    assert_eq!(mounts[0].image.as_ref().unwrap().image, "docker.io/library/nginx@sha256:abc");
}

#[test]
fn image_volume_subpath_becomes_image_sub_path_not_a_joined_host_path() {
    let mut volumes = HashMap::new();
    volumes.insert("config-image".to_string(), ResolvedVolume::Image { image_ref: "example.com/img:latest".to_string() });
    let mut mount = vm("config-image", "/etc/nginx");
    mount.sub_path = Some("conf.d".to_string());
    let mounts = build_mounts(&[mount], &volumes, &[]);
    assert_eq!(mounts[0].image_sub_path, "conf.d");
    assert_eq!(mounts[0].host_path, "");
}

#[test]
fn image_volume_with_no_subpath_leaves_image_sub_path_empty() {
    let mut volumes = HashMap::new();
    volumes.insert("config-image".to_string(), ResolvedVolume::Image { image_ref: "example.com/img:latest".to_string() });
    let mounts = build_mounts(&[vm("config-image", "/etc/nginx")], &volumes, &[]);
    assert_eq!(mounts[0].image_sub_path, "");
}

// --- subPathExpr (round 69; found in a fresh gap re-audit) ---

fn env(key: &str, value: &str) -> KeyValue {
    KeyValue { key: key.to_string(), value: value.as_bytes().to_vec() }
}

#[test]
fn sub_path_expr_expands_a_downward_api_style_env_var() {
    let mut volumes = HashMap::new();
    volumes.insert("data".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/data")));
    let mut mount = vm("data", "/etc/data");
    mount.sub_path_expr = Some("$(POD_NAME)".to_string());
    let envs = [env("POD_NAME", "my-pod")];
    let mounts = build_mounts(&[mount], &volumes, &envs);
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].host_path, "/vol/data/my-pod");
}

#[test]
fn sub_path_expr_supports_multiple_references_and_literal_text() {
    let mut volumes = HashMap::new();
    volumes.insert("data".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/data")));
    let mut mount = vm("data", "/etc/data");
    mount.sub_path_expr = Some("$(POD_NAMESPACE)/$(POD_NAME)-logs".to_string());
    let envs = [env("POD_NAME", "my-pod"), env("POD_NAMESPACE", "default")];
    let mounts = build_mounts(&[mount], &volumes, &envs);
    assert_eq!(mounts[0].host_path, "/vol/data/default/my-pod-logs");
}

#[test]
fn sub_path_expr_a_double_dollar_is_a_literal_dollar_not_a_reference() {
    let mut volumes = HashMap::new();
    volumes.insert("data".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/data")));
    let mut mount = vm("data", "/etc/data");
    mount.sub_path_expr = Some("price-$$5".to_string());
    let mounts = build_mounts(&[mount], &volumes, &[]);
    assert_eq!(mounts[0].host_path, "/vol/data/price-$5");
}

#[test]
fn sub_path_expr_referencing_an_unknown_var_drops_the_mount() {
    let mut volumes = HashMap::new();
    volumes.insert("data".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/data")));
    let mut mount = vm("data", "/etc/data");
    mount.sub_path_expr = Some("$(NOT_SET)".to_string());
    let mounts = build_mounts(&[mount], &volumes, &[]);
    assert!(mounts.is_empty(), "an unresolvable subPathExpr must never produce a mount pointing at a garbage path");
}

#[test]
fn sub_path_expr_an_unclosed_reference_drops_the_mount() {
    let mut volumes = HashMap::new();
    volumes.insert("data".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/data")));
    let mut mount = vm("data", "/etc/data");
    mount.sub_path_expr = Some("$(POD_NAME".to_string());
    let mounts = build_mounts(&[mount], &volumes, &[env("POD_NAME", "my-pod")]);
    assert!(mounts.is_empty());
}

#[test]
fn sub_path_expr_wins_over_a_plain_sub_path_if_both_are_somehow_set() {
    let mut volumes = HashMap::new();
    volumes.insert("data".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/data")));
    let mut mount = vm("data", "/etc/data");
    mount.sub_path = Some("literal".to_string());
    mount.sub_path_expr = Some("$(POD_NAME)".to_string());
    let mounts = build_mounts(&[mount], &volumes, &[env("POD_NAME", "expanded")]);
    assert_eq!(mounts[0].host_path, "/vol/data/expanded");
}

#[test]
fn sub_path_expr_also_applies_to_image_volumes() {
    let mut volumes = HashMap::new();
    volumes.insert("config-image".to_string(), ResolvedVolume::Image { image_ref: "example.com/img:latest".to_string() });
    let mut mount = vm("config-image", "/etc/nginx");
    mount.sub_path_expr = Some("$(POD_NAME)".to_string());
    let mounts = build_mounts(&[mount], &volumes, &[env("POD_NAME", "my-pod")]);
    assert_eq!(mounts[0].image_sub_path, "my-pod");
}
