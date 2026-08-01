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
    assert!(build_mounts(&[], &volumes, &[], None, false).is_empty());
}

#[test]
fn resolves_a_simple_mount_to_its_volume_directory() {
    let mut volumes = HashMap::new();
    volumes.insert("config-volume".to_string(), ResolvedVolume::HostPath(PathBuf::from("/var/lib/nodelet/pods/uid1/volumes/config-volume")));
    let mounts = build_mounts(&[vm("config-volume", "/etc/coredns")], &volumes, &[], None, false);
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].container_path, "/etc/coredns");
    assert_eq!(mounts[0].host_path, "/var/lib/nodelet/pods/uid1/volumes/config-volume");
}

#[test]
fn mount_naming_an_unresolved_volume_is_dropped_not_errored() {
    // e.g. a projected/serviceAccountToken volume resolve_volumes() didn't
    // materialize — must not produce a Mount pointing at a nonexistent path.
    let volumes = HashMap::new();
    let mounts = build_mounts(&[vm("kube-api-access", "/var/run/secrets/kubernetes.io/serviceaccount")], &volumes, &[], None, false);
    assert!(mounts.is_empty());
}

#[test]
fn mount_propagation_unset_defaults_to_private() {
    let mut volumes = HashMap::new();
    volumes.insert("v".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/v")));
    let mounts = build_mounts(&[vm("v", "/etc/v")], &volumes, &[], None, false);
    assert_eq!(mounts[0].propagation, MountPropagation::PropagationPrivate as i32);
}

#[test]
fn mount_propagation_host_to_container_is_carried_through() {
    let mut volumes = HashMap::new();
    volumes.insert("v".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/v")));
    let mut mount = vm("v", "/etc/v");
    mount.mount_propagation = Some("HostToContainer".to_string());
    let mounts = build_mounts(&[mount], &volumes, &[], None, false);
    assert_eq!(mounts[0].propagation, MountPropagation::PropagationHostToContainer as i32);
}

#[test]
fn mount_propagation_bidirectional_is_carried_through() {
    let mut volumes = HashMap::new();
    volumes.insert("v".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/v")));
    let mut mount = vm("v", "/etc/v");
    mount.mount_propagation = Some("Bidirectional".to_string());
    let mounts = build_mounts(&[mount], &volumes, &[], None, false);
    assert_eq!(mounts[0].propagation, MountPropagation::PropagationBidirectional as i32);
}

#[test]
fn recursive_read_only_enabled_with_read_only_true_is_carried_through() {
    let mut volumes = HashMap::new();
    volumes.insert("v".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/v")));
    let mut mount = vm("v", "/etc/v");
    mount.read_only = Some(true);
    mount.recursive_read_only = Some("Enabled".to_string());
    let mounts = build_mounts(&[mount], &volumes, &[], None, false);
    assert!(mounts[0].recursive_read_only);
}

#[test]
fn recursive_read_only_enabled_without_read_only_stays_false() {
    let mut volumes = HashMap::new();
    volumes.insert("v".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/v")));
    let mut mount = vm("v", "/etc/v");
    mount.recursive_read_only = Some("Enabled".to_string());
    let mounts = build_mounts(&[mount], &volumes, &[], None, false);
    assert!(!mounts[0].recursive_read_only, "readOnly wasn't set to true, so recursive_read_only must stay false per the CRI contract");
}

#[test]
fn recursive_read_only_unset_stays_false_even_with_read_only_true() {
    let mut volumes = HashMap::new();
    volumes.insert("v".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/v")));
    let mut mount = vm("v", "/etc/v");
    mount.read_only = Some(true);
    let mounts = build_mounts(&[mount], &volumes, &[], None, false);
    assert!(!mounts[0].recursive_read_only);
}

#[test]
fn userns_mapping_none_leaves_uid_gid_mappings_empty() {
    let mut volumes = HashMap::new();
    volumes.insert("v".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/v")));
    let mounts = build_mounts(&[vm("v", "/etc/v")], &volumes, &[], None, false);
    assert!(mounts[0].uid_mappings.is_empty());
    assert!(mounts[0].gid_mappings.is_empty());
}

#[test]
fn userns_mapping_some_is_carried_onto_the_mount() {
    let mut volumes = HashMap::new();
    volumes.insert("v".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/v")));
    let mounts = build_mounts(&[vm("v", "/etc/v")], &volumes, &[], Some((100_000, 65_536)), false);
    assert_eq!(mounts[0].uid_mappings.len(), 1);
    assert_eq!(mounts[0].uid_mappings[0].host_id, 100_000);
    assert_eq!(mounts[0].uid_mappings[0].container_id, 0);
    assert_eq!(mounts[0].uid_mappings[0].length, 65_536);
    assert_eq!(mounts[0].gid_mappings, mounts[0].uid_mappings, "uid and gid mappings should mirror the same range");
}

#[test]
fn userns_mapping_is_not_applied_to_image_backed_mounts() {
    // Image-backed mounts (round 32) never go through the host bind-mount
    // path idmapped mounts apply to at all.
    let mut volumes = HashMap::new();
    volumes.insert("img".to_string(), ResolvedVolume::Image { image_ref: "docker.io/library/nginx:1.25".to_string() });
    let mounts = build_mounts(&[vm("img", "/etc/nginx")], &volumes, &[], Some((100_000, 65_536)), false);
    assert!(mounts[0].uid_mappings.is_empty());
    assert!(mounts[0].gid_mappings.is_empty());
}

#[test]
fn a_mount_naming_a_block_device_volume_is_dropped_defensively() {
    // Round 77: a raw block volume is only ever referenced via
    // volumeDevices, never volumeMounts -- the API itself should prevent
    // this combination, but build_mounts() stays defensive rather than
    // trusting that (same posture build_devices() takes for the opposite
    // mismatch).
    let mut volumes = HashMap::new();
    volumes.insert("raw-disk".to_string(), ResolvedVolume::BlockDevice(PathBuf::from("/host/raw-disk")));
    let mounts = build_mounts(&[vm("raw-disk", "/mnt/raw-disk")], &volumes, &[], None, false);
    assert!(mounts.is_empty());
}

#[test]
fn sub_path_is_joined_onto_the_volume_directory() {
    let mut volumes = HashMap::new();
    volumes.insert("config-volume".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/config-volume")));
    let mut mount = vm("config-volume", "/etc/coredns/Corefile");
    mount.sub_path = Some("Corefile".to_string());
    let mounts = build_mounts(&[mount], &volumes, &[], None, false);
    assert_eq!(mounts[0].host_path, "/vol/config-volume/Corefile");
}

#[test]
fn read_only_true_is_propagated() {
    let mut volumes = HashMap::new();
    volumes.insert("v".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/v")));
    let mut mount = vm("v", "/etc/v");
    mount.read_only = Some(true);
    let mounts = build_mounts(&[mount], &volumes, &[], None, false);
    assert!(mounts[0].readonly);
}

#[test]
fn read_only_unset_defaults_to_false() {
    let mut volumes = HashMap::new();
    volumes.insert("v".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/v")));
    let mounts = build_mounts(&[vm("v", "/etc/v")], &volumes, &[], None, false);
    assert!(!mounts[0].readonly);
}

#[test]
fn read_only_explicit_false_stays_false() {
    let mut volumes = HashMap::new();
    volumes.insert("v".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/v")));
    let mut mount = vm("v", "/etc/v");
    mount.read_only = Some(false);
    let mounts = build_mounts(&[mount], &volumes, &[], None, false);
    assert!(!mounts[0].readonly);
}

#[test]
fn multiple_mounts_each_resolve_independently() {
    let mut volumes = HashMap::new();
    volumes.insert("a".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/a")));
    volumes.insert("b".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/b")));
    let mounts = build_mounts(&[vm("a", "/mnt/a"), vm("b", "/mnt/b"), vm("c", "/mnt/c")], &volumes, &[], None, false);
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
    let mounts = build_mounts(&[vm("config-volume", "/etc/coredns")], &volumes, &[], None, false);
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].container_path, "/etc/coredns");
    assert!(!mounts[0].host_path.is_empty());
}

// --- volumeSource.image (round 32) ---

#[test]
fn image_volume_sets_the_image_field_not_a_host_path() {
    let mut volumes = HashMap::new();
    volumes.insert("config-image".to_string(), ResolvedVolume::Image { image_ref: "docker.io/library/nginx@sha256:abc".to_string() });
    let mounts = build_mounts(&[vm("config-image", "/etc/nginx")], &volumes, &[], None, false);
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
    let mounts = build_mounts(&[mount], &volumes, &[], None, false);
    assert_eq!(mounts[0].image_sub_path, "conf.d");
    assert_eq!(mounts[0].host_path, "");
}

#[test]
fn image_volume_with_no_subpath_leaves_image_sub_path_empty() {
    let mut volumes = HashMap::new();
    volumes.insert("config-image".to_string(), ResolvedVolume::Image { image_ref: "example.com/img:latest".to_string() });
    let mounts = build_mounts(&[vm("config-image", "/etc/nginx")], &volumes, &[], None, false);
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
    let mounts = build_mounts(&[mount], &volumes, &envs, None, false);
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
    let mounts = build_mounts(&[mount], &volumes, &envs, None, false);
    assert_eq!(mounts[0].host_path, "/vol/data/default/my-pod-logs");
}

#[test]
fn sub_path_expr_a_double_dollar_is_a_literal_dollar_not_a_reference() {
    let mut volumes = HashMap::new();
    volumes.insert("data".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/data")));
    let mut mount = vm("data", "/etc/data");
    mount.sub_path_expr = Some("price-$$5".to_string());
    let mounts = build_mounts(&[mount], &volumes, &[], None, false);
    assert_eq!(mounts[0].host_path, "/vol/data/price-$5");
}

#[test]
fn sub_path_expr_referencing_an_unknown_var_drops_the_mount() {
    let mut volumes = HashMap::new();
    volumes.insert("data".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/data")));
    let mut mount = vm("data", "/etc/data");
    mount.sub_path_expr = Some("$(NOT_SET)".to_string());
    let mounts = build_mounts(&[mount], &volumes, &[], None, false);
    assert!(mounts.is_empty(), "an unresolvable subPathExpr must never produce a mount pointing at a garbage path");
}

#[test]
fn sub_path_expr_an_unclosed_reference_drops_the_mount() {
    let mut volumes = HashMap::new();
    volumes.insert("data".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/data")));
    let mut mount = vm("data", "/etc/data");
    mount.sub_path_expr = Some("$(POD_NAME".to_string());
    let mounts = build_mounts(&[mount], &volumes, &[env("POD_NAME", "my-pod")], None, false);
    assert!(mounts.is_empty());
}

#[test]
fn sub_path_expr_wins_over_a_plain_sub_path_if_both_are_somehow_set() {
    let mut volumes = HashMap::new();
    volumes.insert("data".to_string(), ResolvedVolume::HostPath(PathBuf::from("/vol/data")));
    let mut mount = vm("data", "/etc/data");
    mount.sub_path = Some("literal".to_string());
    mount.sub_path_expr = Some("$(POD_NAME)".to_string());
    let mounts = build_mounts(&[mount], &volumes, &[env("POD_NAME", "expanded")], None, false);
    assert_eq!(mounts[0].host_path, "/vol/data/expanded");
}

#[test]
fn sub_path_expr_also_applies_to_image_volumes() {
    let mut volumes = HashMap::new();
    volumes.insert("config-image".to_string(), ResolvedVolume::Image { image_ref: "example.com/img:latest".to_string() });
    let mut mount = vm("config-image", "/etc/nginx");
    mount.sub_path_expr = Some("$(POD_NAME)".to_string());
    let mounts = build_mounts(&[mount], &volumes, &[env("POD_NAME", "my-pod")], None, false);
    assert_eq!(mounts[0].image_sub_path, "my-pod");
}
