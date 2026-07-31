//! map_prepare_results()/map_unprepare_results(): the pure logic behind
//! round 64's RPC batching — mapping a single NodePrepareResources/
//! NodeUnprepareResources response's per-claim-UID results back onto
//! every claim that was actually requested in the batch.
use super::*;
use std::collections::HashMap;

fn claim(uid: &str) -> ClaimRef {
    ClaimRef { namespace: "ns".to_string(), uid: uid.to_string(), name: format!("claim-{uid}") }
}

#[test]
fn every_requested_claim_gets_an_entry() {
    let claims = vec![claim("a"), claim("b")];
    let mut resp = HashMap::new();
    resp.insert("a".to_string(), v1beta1::NodePrepareResourceResponse { devices: vec![], error: String::new() });
    resp.insert("b".to_string(), v1beta1::NodePrepareResourceResponse { devices: vec![], error: String::new() });
    let out = map_prepare_results(&claims, &resp);
    assert_eq!(out.len(), 2);
    assert!(out.contains_key("a"));
    assert!(out.contains_key("b"));
}

#[test]
fn successful_devices_are_translated() {
    let claims = vec![claim("a")];
    let mut resp = HashMap::new();
    resp.insert(
        "a".to_string(),
        v1beta1::NodePrepareResourceResponse {
            devices: vec![Device { request_names: vec!["req".to_string()], cdi_device_ids: vec!["vendor.com/gpu=0".to_string()] }],
            error: String::new(),
        },
    );
    let out = map_prepare_results(&claims, &resp);
    let devices = out.get("a").unwrap().as_ref().unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].cdi_device_ids, vec!["vendor.com/gpu=0".to_string()]);
}

#[test]
fn a_claim_level_error_does_not_fail_the_whole_batch() {
    let claims = vec![claim("a"), claim("b")];
    let mut resp = HashMap::new();
    resp.insert("a".to_string(), v1beta1::NodePrepareResourceResponse { devices: vec![], error: "device busy".to_string() });
    resp.insert("b".to_string(), v1beta1::NodePrepareResourceResponse { devices: vec![], error: String::new() });
    let out = map_prepare_results(&claims, &resp);
    assert_eq!(out.get("a").unwrap().as_ref().unwrap_err(), "device busy");
    assert!(out.get("b").unwrap().is_ok());
}

#[test]
fn a_claim_missing_from_the_response_is_a_synthetic_error() {
    let claims = vec![claim("a")];
    let resp: HashMap<String, v1beta1::NodePrepareResourceResponse> = HashMap::new();
    let out = map_prepare_results(&claims, &resp);
    assert!(out.get("a").unwrap().is_err());
}

#[test]
fn unprepare_success_and_error_both_map_through() {
    let claims = vec![claim("a"), claim("b")];
    let mut resp = HashMap::new();
    resp.insert("a".to_string(), v1beta1::NodeUnprepareResourceResponse { error: String::new() });
    resp.insert("b".to_string(), v1beta1::NodeUnprepareResourceResponse { error: "still in use".to_string() });
    let out = map_unprepare_results(&claims, &resp);
    assert!(out.get("a").unwrap().is_ok());
    assert_eq!(out.get("b").unwrap().as_ref().unwrap_err(), "still in use");
}

#[test]
fn unprepare_claim_missing_from_the_response_is_treated_as_already_gone() {
    // Unlike prepare, a missing unprepare result has no useful device
    // state hiding behind it, so it's not an error.
    let claims = vec![claim("a")];
    let resp: HashMap<String, v1beta1::NodeUnprepareResourceResponse> = HashMap::new();
    let out = map_unprepare_results(&claims, &resp);
    assert!(out.get("a").unwrap().is_ok());
}

#[test]
fn empty_claims_list_produces_an_empty_map() {
    let claims: Vec<ClaimRef> = vec![];
    let resp: HashMap<String, v1beta1::NodePrepareResourceResponse> = HashMap::new();
    assert!(map_prepare_results(&claims, &resp).is_empty());
}
