//! build_devices(): raw block volumes (round 77; found in round 76's
//! re-audit) — the volumeDevices counterpart to build_mounts()'s
//! volumeMounts handling. A BlockDevice-resolved volume is injected via
//! CRI's ContainerConfig.devices, never via Mount.
use super::*;
use k8s_openapi::api::core::v1::VolumeDevice;

fn vd(name: &str, device_path: &str) -> VolumeDevice {
    VolumeDevice { name: name.to_string(), device_path: device_path.to_string() }
}

#[test]
fn empty_devices_list_produces_no_devices() {
    let volumes = HashMap::new();
    assert!(build_devices(&[], &volumes).is_empty());
}

#[test]
fn resolves_a_block_device_to_its_host_path_with_rwm_permissions() {
    let mut volumes = HashMap::new();
    volumes.insert("raw-disk".to_string(), ResolvedVolume::BlockDevice(PathBuf::from("/var/lib/nodelet/pods/uid1/volumes/raw-disk")));
    let devices = build_devices(&[vd("raw-disk", "/dev/xvda")], &volumes);
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].container_path, "/dev/xvda");
    assert_eq!(devices[0].host_path, "/var/lib/nodelet/pods/uid1/volumes/raw-disk");
    assert_eq!(devices[0].permissions, "rwm");
}

#[test]
fn a_device_naming_an_unresolved_volume_is_dropped_not_errored() {
    let volumes = HashMap::new();
    let devices = build_devices(&[vd("raw-disk", "/dev/xvda")], &volumes);
    assert!(devices.is_empty());
}

#[test]
fn a_device_naming_a_regular_filesystem_volume_is_dropped_defensively() {
    // The API itself should prevent volumeDevices from naming a
    // Filesystem-mode volume, but build_devices() stays defensive rather
    // than trusting that -- same posture build_mounts() takes for the
    // opposite mismatch (a volumeMount naming a BlockDevice).
    let mut volumes = HashMap::new();
    volumes.insert("cm-volume".to_string(), ResolvedVolume::HostPath(PathBuf::from("/var/lib/nodelet/pods/uid1/volumes/cm-volume")));
    let devices = build_devices(&[vd("cm-volume", "/dev/xvda")], &volumes);
    assert!(devices.is_empty());
}

#[test]
fn multiple_devices_are_all_resolved_independently() {
    let mut volumes = HashMap::new();
    volumes.insert("disk-a".to_string(), ResolvedVolume::BlockDevice(PathBuf::from("/host/disk-a")));
    volumes.insert("disk-b".to_string(), ResolvedVolume::BlockDevice(PathBuf::from("/host/disk-b")));
    let devices = build_devices(&[vd("disk-a", "/dev/xvda"), vd("disk-b", "/dev/xvdb")], &volumes);
    assert_eq!(devices.len(), 2);
    assert!(devices.iter().any(|d| d.container_path == "/dev/xvda" && d.host_path == "/host/disk-a"));
    assert!(devices.iter().any(|d| d.container_path == "/dev/xvdb" && d.host_path == "/host/disk-b"));
}
