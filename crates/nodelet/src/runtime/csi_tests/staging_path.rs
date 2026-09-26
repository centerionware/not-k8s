use super::*;

#[test]
fn includes_driver_and_volume_handle_and_ends_in_globalmount() {
    let p = staging_path(
        Path::new("/var/lib/kubelet/plugins/kubernetes.io/csi"),
        "hostpath.csi.k8s.io",
        "vol-abc-123",
    );
    assert_eq!(
        p,
        Path::new("/var/lib/kubelet/plugins/kubernetes.io/csi/hostpath.csi.k8s.io/0c4f4ffa6d1042ca79f9092cbdb7ed929570c04d94dce052492f98abdd97ea1f/globalmount")
    );
}

#[test]
fn different_drivers_produce_different_paths_for_the_same_handle() {
    let root = Path::new("/var/lib/kubelet/plugins/kubernetes.io/csi");
    let a = staging_path(root, "driver-a", "vol-1");
    let b = staging_path(root, "driver-b", "vol-1");
    assert_ne!(a, b);
}

#[test]
fn different_volume_handles_produce_different_paths_for_the_same_driver() {
    let root = Path::new("/var/lib/kubelet/plugins/kubernetes.io/csi");
    let a = staging_path(root, "driver-a", "vol-1");
    let b = staging_path(root, "driver-a", "vol-2");
    assert_ne!(a, b);
}
