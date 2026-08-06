//! ClaimRef -> Claim / registry bookkeeping: the pure, non-networked
//! pieces of dra.rs.
use super::*;

#[test]
fn claim_ref_converts_to_the_wire_claim_message_verbatim() {
    let c = ClaimRef { namespace: "ns".to_string(), uid: "uid-1".to_string(), name: "my-claim".to_string() };
    let wire = Claim::from(&c);
    assert_eq!(wire.namespace, "ns");
    assert_eq!(wire.uid, "uid-1");
    assert_eq!(wire.name, "my-claim");
}

#[test]
fn registering_a_driver_makes_it_configured() {
    let drivers = DraDrivers::new();
    assert!(!drivers.driver_configured("gpu.example.com"));
    drivers.register("gpu.example.com".to_string(), "unix:///var/lib/nodelet/plugins/gpu.sock".to_string());
    assert!(drivers.driver_configured("gpu.example.com"));
}

#[test]
fn deregistering_a_driver_removes_it() {
    let drivers = DraDrivers::new();
    drivers.register("gpu.example.com".to_string(), "unix:///socket".to_string());
    drivers.deregister("gpu.example.com");
    assert!(!drivers.driver_configured("gpu.example.com"));
}

#[test]
fn deregistering_an_unknown_driver_is_a_harmless_no_op() {
    let drivers = DraDrivers::new();
    drivers.deregister("never-registered.example.com");
    assert!(!drivers.driver_configured("never-registered.example.com"));
}

#[test]
fn from_proto_devices_carries_request_names_and_cdi_ids_through() {
    let devices = vec![Device { request_names: vec!["req-a".to_string()], pool_name: "pool".to_string(), device_name: "gpu-0".to_string(), cdi_device_ids: vec!["vendor.com/gpu=0".to_string()], share_id: None }];
    let prepared = from_proto_devices(devices);
    assert_eq!(prepared.len(), 1);
    assert_eq!(prepared[0].request_names, vec!["req-a".to_string()]);
    assert_eq!(prepared[0].cdi_device_ids, vec!["vendor.com/gpu=0".to_string()]);
}
