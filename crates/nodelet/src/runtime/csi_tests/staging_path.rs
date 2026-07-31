use super::*;

#[test]
fn includes_driver_and_volume_handle_and_ends_in_globalmount() {
    let p = staging_path("hostpath.csi.k8s.io", "vol-abc-123");
    let s = p.to_string_lossy();
    assert!(s.contains("hostpath.csi.k8s.io"));
    assert!(s.contains("vol-abc-123"));
    assert!(s.ends_with("globalmount"));
}

#[test]
fn different_drivers_produce_different_paths_for_the_same_handle() {
    let a = staging_path("driver-a", "vol-1");
    let b = staging_path("driver-b", "vol-1");
    assert_ne!(a, b);
}

#[test]
fn different_volume_handles_produce_different_paths_for_the_same_driver() {
    let a = staging_path("driver-a", "vol-1");
    let b = staging_path("driver-a", "vol-2");
    assert_ne!(a, b);
}
