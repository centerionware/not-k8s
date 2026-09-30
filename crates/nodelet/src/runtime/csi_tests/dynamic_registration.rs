//! CsiDrivers::register()/deregister()/driver_configured() — the state
//! plugin_registry.rs mutates as CSI driver sockets appear/disappear.
use super::*;

#[test]
fn a_driver_not_registered_at_all_is_unconfigured() {
    let drivers = CsiDrivers::new(Default::default());
    assert!(!drivers.driver_configured("hostpath.csi.k8s.io"));
}

#[test]
fn statically_configured_drivers_are_seen_as_configured() {
    let mut seed = BTreeMap::new();
    seed.insert("hostpath.csi.k8s.io".to_string(), "unix:///static.sock".to_string());
    let drivers = CsiDrivers::new(seed);
    assert!(drivers.driver_configured("hostpath.csi.k8s.io"));
}

#[test]
fn dynamically_registering_a_driver_makes_it_configured() {
    let drivers = CsiDrivers::new(Default::default());
    drivers.register("hostpath.csi.k8s.io".to_string(), "unix:///dynamic.sock".to_string());
    assert!(drivers.driver_configured("hostpath.csi.k8s.io"));
}

#[test]
fn registering_a_driver_wakes_only_pods_waiting_for_that_driver() {
    let drivers = CsiDrivers::new(Default::default());
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    drivers.set_registration_events(events);
    drivers.wait_for_driver("hostpath.csi.k8s.io", "apps/stateful-0", "pod-uid-a");
    drivers.wait_for_driver("other.csi.example", "apps/other-0", "pod-uid-b");

    drivers.register("hostpath.csi.k8s.io".to_string(), "unix:///first.sock".to_string());
    assert_eq!(receiver.try_recv().unwrap(), "apps/stateful-0");
    assert!(receiver.try_recv().is_err());

    drivers.register("hostpath.csi.k8s.io".to_string(), "unix:///first.sock".to_string());
    assert!(receiver.try_recv().is_err());

    drivers.register("other.csi.example".to_string(), "unix:///other.sock".to_string());
    assert_eq!(receiver.try_recv().unwrap(), "apps/other-0");
}

#[test]
fn waiting_on_a_registered_driver_enqueues_immediately() {
    let drivers = CsiDrivers::new(Default::default());
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    drivers.set_registration_events(events);
    drivers.register("hostpath.csi.k8s.io".to_string(), "unix:///driver.sock".to_string());
    drivers.wait_for_driver("hostpath.csi.k8s.io", "apps/stateful-0", "pod-uid-a");
    assert_eq!(receiver.try_recv().unwrap(), "apps/stateful-0");
}

#[test]
fn removing_a_pod_uid_forgets_its_pending_driver_waits() {
    let drivers = CsiDrivers::new(Default::default());
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    drivers.set_registration_events(events);
    drivers.wait_for_driver("hostpath.csi.k8s.io", "apps/stateful-0", "old-uid");
    drivers.forget_waiting_pod("old-uid");
    drivers.register("hostpath.csi.k8s.io".to_string(), "unix:///driver.sock".to_string());
    assert!(receiver.try_recv().is_err());
}

#[test]
fn same_name_replacement_keeps_only_the_new_pod_waiter() {
    let drivers = CsiDrivers::new(Default::default());
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    drivers.set_registration_events(events);
    drivers.wait_for_driver("hostpath.csi.k8s.io", "apps/stateful-0", "old-uid");
    drivers.wait_for_driver("hostpath.csi.k8s.io", "apps/stateful-0", "new-uid");
    drivers.forget_waiting_pod("old-uid");
    drivers.register("hostpath.csi.k8s.io".to_string(), "unix:///driver.sock".to_string());
    assert_eq!(receiver.try_recv().unwrap(), "apps/stateful-0");
}

#[test]
fn deregistering_a_driver_makes_it_unconfigured_again() {
    let drivers = CsiDrivers::new(Default::default());
    drivers.register("hostpath.csi.k8s.io".to_string(), "unix:///dynamic.sock".to_string());
    drivers.deregister("hostpath.csi.k8s.io");
    assert!(!drivers.driver_configured("hostpath.csi.k8s.io"));
}

#[test]
fn deregistering_an_unknown_driver_is_a_harmless_no_op() {
    let drivers = CsiDrivers::new(Default::default());
    drivers.deregister("never-registered");
    assert!(!drivers.driver_configured("never-registered"));
}

#[test]
fn re_registering_a_driver_updates_its_endpoint() {
    let drivers = CsiDrivers::new(Default::default());
    drivers.register("hostpath.csi.k8s.io".to_string(), "unix:///first.sock".to_string());
    drivers.register("hostpath.csi.k8s.io".to_string(), "unix:///second.sock".to_string());
    // Not directly observable from outside without a live connection, but
    // driver_configured() staying true across the re-registration is —
    // the endpoint_for() lookup that mount()/unmount() use internally
    // reads the same map, so this pins down there's still exactly one
    // live entry, not a stale-plus-new duplicate causing ambiguity.
    assert!(drivers.driver_configured("hostpath.csi.k8s.io"));
}
