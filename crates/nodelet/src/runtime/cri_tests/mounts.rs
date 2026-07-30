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
    assert!(build_mounts(&[], &volumes).is_empty());
}

#[test]
fn resolves_a_simple_mount_to_its_volume_directory() {
    let mut volumes = HashMap::new();
    volumes.insert("config-volume".to_string(), PathBuf::from("/var/lib/nodelet/pods/uid1/volumes/config-volume"));
    let mounts = build_mounts(&[vm("config-volume", "/etc/coredns")], &volumes);
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].container_path, "/etc/coredns");
    assert_eq!(mounts[0].host_path, "/var/lib/nodelet/pods/uid1/volumes/config-volume");
}

#[test]
fn mount_naming_an_unresolved_volume_is_dropped_not_errored() {
    // e.g. a projected/serviceAccountToken volume resolve_volumes() didn't
    // materialize — must not produce a Mount pointing at a nonexistent path.
    let volumes = HashMap::new();
    let mounts = build_mounts(&[vm("kube-api-access", "/var/run/secrets/kubernetes.io/serviceaccount")], &volumes);
    assert!(mounts.is_empty());
}

#[test]
fn sub_path_is_joined_onto_the_volume_directory() {
    let mut volumes = HashMap::new();
    volumes.insert("config-volume".to_string(), PathBuf::from("/vol/config-volume"));
    let mut mount = vm("config-volume", "/etc/coredns/Corefile");
    mount.sub_path = Some("Corefile".to_string());
    let mounts = build_mounts(&[mount], &volumes);
    assert_eq!(mounts[0].host_path, "/vol/config-volume/Corefile");
}

#[test]
fn read_only_true_is_propagated() {
    let mut volumes = HashMap::new();
    volumes.insert("v".to_string(), PathBuf::from("/vol/v"));
    let mut mount = vm("v", "/etc/v");
    mount.read_only = Some(true);
    let mounts = build_mounts(&[mount], &volumes);
    assert!(mounts[0].readonly);
}

#[test]
fn read_only_unset_defaults_to_false() {
    let mut volumes = HashMap::new();
    volumes.insert("v".to_string(), PathBuf::from("/vol/v"));
    let mounts = build_mounts(&[vm("v", "/etc/v")], &volumes);
    assert!(!mounts[0].readonly);
}

#[test]
fn read_only_explicit_false_stays_false() {
    let mut volumes = HashMap::new();
    volumes.insert("v".to_string(), PathBuf::from("/vol/v"));
    let mut mount = vm("v", "/etc/v");
    mount.read_only = Some(false);
    let mounts = build_mounts(&[mount], &volumes);
    assert!(!mounts[0].readonly);
}

#[test]
fn multiple_mounts_each_resolve_independently() {
    let mut volumes = HashMap::new();
    volumes.insert("a".to_string(), PathBuf::from("/vol/a"));
    volumes.insert("b".to_string(), PathBuf::from("/vol/b"));
    let mounts = build_mounts(&[vm("a", "/mnt/a"), vm("b", "/mnt/b"), vm("c", "/mnt/c")], &volumes);
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
    volumes.insert("config-volume".to_string(), PathBuf::from("/var/lib/nodelet/pods/abc/volumes/config-volume"));
    let mounts = build_mounts(&[vm("config-volume", "/etc/coredns")], &volumes);
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].container_path, "/etc/coredns");
    assert!(!mounts[0].host_path.is_empty());
}
