use super::*;

#[test]
fn guaranteed_pods_sit_directly_under_kubepods_no_qos_subdir() {
    assert_eq!(cgroup_parent_for(QosClass::Guaranteed, "abc-123", Driver::Cgroupfs), "/kubepods/podabc-123");
}

#[test]
fn burstable_pods_get_their_own_subdirectory() {
    assert_eq!(cgroup_parent_for(QosClass::Burstable, "abc-123", Driver::Cgroupfs), "/kubepods/burstable/podabc-123");
}

#[test]
fn besteffort_pods_get_their_own_subdirectory() {
    assert_eq!(cgroup_parent_for(QosClass::BestEffort, "abc-123", Driver::Cgroupfs), "/kubepods/besteffort/podabc-123");
}

#[test]
fn systemd_driver_uses_qos_scoped_slices_and_escapes_uid_separators() {
    assert_eq!(cgroup_parent_for(QosClass::Guaranteed, "abc-123", Driver::Systemd), "/kubepods.slice/kubepods-podabc_123.slice");
    assert_eq!(cgroup_parent_for(QosClass::Burstable, "abc-123", Driver::Systemd), "/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-podabc_123.slice");
    assert_eq!(cgroup_parent_for(QosClass::BestEffort, "abc-123", Driver::Systemd), "/kubepods.slice/kubepods-besteffort.slice/kubepods-besteffort-podabc_123.slice");
}

#[test]
fn different_uids_produce_different_paths() {
    let a = cgroup_parent_for(QosClass::Burstable, "uid-1", Driver::Cgroupfs);
    let b = cgroup_parent_for(QosClass::Burstable, "uid-2", Driver::Cgroupfs);
    assert_ne!(a, b);
}

#[test]
fn paths_start_with_a_leading_slash() {
    for qos in [QosClass::Guaranteed, QosClass::Burstable, QosClass::BestEffort] {
        assert!(cgroup_parent_for(qos, "x", Driver::Cgroupfs).starts_with('/'));
        assert!(cgroup_parent_for(qos, "x", Driver::Systemd).starts_with('/'));
    }
}
