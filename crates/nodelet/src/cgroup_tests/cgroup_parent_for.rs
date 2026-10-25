use super::*;

#[test]
fn guaranteed_pods_use_kubepods_parent() {
    assert_eq!(cgroup_parent_for(QosClass::Guaranteed, Driver::Cgroupfs), "/kubepods");
}

#[test]
fn burstable_pods_get_their_own_subdirectory() {
    assert_eq!(cgroup_parent_for(QosClass::Burstable, Driver::Cgroupfs), "/kubepods/burstable");
}

#[test]
fn besteffort_pods_get_their_own_subdirectory() {
    assert_eq!(cgroup_parent_for(QosClass::BestEffort, Driver::Cgroupfs), "/kubepods/besteffort");
}

#[test]
fn systemd_driver_uses_qos_scoped_parent_slices() {
    assert_eq!(cgroup_parent_for(QosClass::Guaranteed, Driver::Systemd), "/kubepods.slice");
    assert_eq!(cgroup_parent_for(QosClass::Burstable, Driver::Systemd), "/kubepods.slice/kubepods-burstable.slice");
    assert_eq!(cgroup_parent_for(QosClass::BestEffort, Driver::Systemd), "/kubepods.slice/kubepods-besteffort.slice");
}

#[test]
fn paths_start_with_a_leading_slash() {
    for qos in [QosClass::Guaranteed, QosClass::Burstable, QosClass::BestEffort] {
        assert!(cgroup_parent_for(qos, Driver::Cgroupfs).starts_with('/'));
        assert!(cgroup_parent_for(qos, Driver::Systemd).starts_with('/'));
    }
}
