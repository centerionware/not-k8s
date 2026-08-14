use super::*;
use crate::cache::dra::{
    RawBasicDevice, RawCelSelector, RawDevice, RawDeviceAttribute, RawDeviceClassSpec,
    RawDeviceRequest, RawDeviceSelector, RawResourceClaimSpec, RawResourcePool,
    RawResourceSliceSpec,
};
use crate::cache::{Cache, RawDeviceClass, RawResourceClaim, RawResourceSlice};
use crate::framework::plugins::testutil::pod;
use k8s_openapi::api::core::v1::Node as ApiNode;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

fn api_node(name: &str) -> ApiNode {
    ApiNode { metadata: ObjectMeta { name: Some(name.to_string()), ..Default::default() }, ..Default::default() }
}

fn claim_meta(namespace: &str, name: &str) -> ObjectMeta {
    ObjectMeta { name: Some(name.to_string()), namespace: Some(namespace.to_string()), ..Default::default() }
}

fn class_with_cel(name: &str, expr: &str) -> RawDeviceClass {
    RawDeviceClass {
        metadata: ObjectMeta { name: Some(name.to_string()), ..Default::default() },
        spec: RawDeviceClassSpec {
            selectors: Some(vec![RawDeviceSelector { cel: Some(RawCelSelector { expression: expr.to_string() }) }]),
        },
    }
}

fn unbound_claim(namespace: &str, name: &str, class: &str, count: i64) -> RawResourceClaim {
    RawResourceClaim {
        metadata: claim_meta(namespace, name),
        spec: RawResourceClaimSpec {
            devices: Some(crate::cache::dra::RawDeviceClaim {
                requests: Some(vec![RawDeviceRequest {
                    name: "req".to_string(),
                    device_class_name: Some(class.to_string()),
                    selectors: None,
                    allocation_mode: None,
                    count: Some(count),
                    admin_access: None,
                    first_available: None,
                }]),
                constraints: None,
            }),
        },
        status: None,
    }
}

fn slice_with_devices(driver: &str, node: &str, device_names: &[&str]) -> RawResourceSlice {
    RawResourceSlice {
        metadata: ObjectMeta { name: Some(format!("{driver}-{node}")), ..Default::default() },
        spec: RawResourceSliceSpec {
            driver: driver.to_string(),
            pool: RawResourcePool { name: node.to_string(), generation: Some(1), resource_slice_count: Some(1) },
            node_name: Some(node.to_string()),
            all_nodes: None,
            node_selector: None,
            devices: Some(
                device_names
                    .iter()
                    .map(|n| RawDevice {
                        name: n.to_string(),
                        basic: Some(RawBasicDevice { attributes: None, capacity: None }),
                    })
                    .collect(),
            ),
        },
    }
}

fn pod_with_claim(namespace: &str, pod_claim_name: &str, claim_name: &str) -> PodInfo {
    let mut p = pod("p");
    p.namespace = namespace.to_string();
    p.uid = "uid-p".to_string();
    p.resource_claims = vec![PodClaimRef {
        name: pod_claim_name.to_string(),
        resource_claim_name: Some(claim_name.to_string()),
        resource_claim_template_name: None,
    }];
    p
}

fn no_excluded() -> HashSet<DeviceId> {
    HashSet::new()
}

#[test]
fn a_pod_with_no_claims_skips_the_plugin() {
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &pod("p"), &Snapshot::default(), &no_excluded());
    assert!(status.is_skip());
    assert!(state.filter_skipped(NAME));
}

#[test]
fn a_claim_with_a_satisfiable_class_selector_is_allocated_on_a_matching_node() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_device_class("gpu.example.com".to_string(), class_with_cel("gpu.example.com", "device.driver == \"gpu.example.com\""));
    cache.upsert_resource_slice("s1".to_string(), slice_with_devices("gpu.example.com", "n1", &["gpu-0"]));
    cache.upsert_resource_claim(
        "ns/claim".to_string(),
        unbound_claim("ns", "claim", "gpu.example.com", 1),
    );
    let snapshot = cache.snapshot();

    let p = pod_with_claim("ns", "gpu", "claim");
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    assert!(status.is_success());

    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(filter_impl(&state, &p, &n).is_success());
}

#[test]
fn a_claim_whose_class_selector_matches_nothing_is_rejected() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_device_class(
        "gpu.example.com".to_string(),
        class_with_cel("gpu.example.com", "device.driver == \"nonexistent.example.com\""),
    );
    cache.upsert_resource_slice("s1".to_string(), slice_with_devices("gpu.example.com", "n1", &["gpu-0"]));
    cache.upsert_resource_claim(
        "ns/claim".to_string(),
        unbound_claim("ns", "claim", "gpu.example.com", 1),
    );
    let snapshot = cache.snapshot();

    let p = pod_with_claim("ns", "gpu", "claim");
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());

    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(!filter_impl(&state, &p, &n).is_success());
}

#[test]
fn a_request_for_more_devices_than_exist_is_rejected() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_device_class("gpu.example.com".to_string(), class_with_cel("gpu.example.com", "device.driver == \"gpu.example.com\""));
    cache.upsert_resource_slice("s1".to_string(), slice_with_devices("gpu.example.com", "n1", &["gpu-0"]));
    cache.upsert_resource_claim(
        "ns/claim".to_string(),
        unbound_claim("ns", "claim", "gpu.example.com", 2),
    );
    let snapshot = cache.snapshot();

    let p = pod_with_claim("ns", "gpu", "claim");
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());

    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(!filter_impl(&state, &p, &n).is_success());
}

#[test]
fn a_device_already_assumed_by_another_pods_cycle_is_excluded() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_device_class("gpu.example.com".to_string(), class_with_cel("gpu.example.com", "device.driver == \"gpu.example.com\""));
    cache.upsert_resource_slice("s1".to_string(), slice_with_devices("gpu.example.com", "n1", &["gpu-0"]));
    cache.upsert_resource_claim(
        "ns/claim".to_string(),
        unbound_claim("ns", "claim", "gpu.example.com", 1),
    );
    let snapshot = cache.snapshot();

    let mut excluded = HashSet::new();
    excluded.insert(("gpu.example.com".to_string(), "n1".to_string(), "gpu-0".to_string()));

    let p = pod_with_claim("ns", "gpu", "claim");
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &excluded);

    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(!filter_impl(&state, &p, &n).is_success(), "the only device is already assumed elsewhere");
}

#[test]
fn a_device_attribute_selector_filters_correctly() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_device_class("gpu.example.com".to_string(), class_with_cel("gpu.example.com", "device.driver == \"gpu.example.com\""));
    let mut slice = slice_with_devices("gpu.example.com", "n1", &["big", "small"]);
    if let Some(devices) = &mut slice.spec.devices {
        let mut attrs = std::collections::BTreeMap::new();
        attrs.insert("size".to_string(), RawDeviceAttribute { bool: None, int: None, string: Some("big".to_string()), version: None });
        devices[0].basic = Some(RawBasicDevice { attributes: Some(attrs), capacity: None });
        let mut attrs2 = std::collections::BTreeMap::new();
        attrs2.insert("size".to_string(), RawDeviceAttribute { bool: None, int: None, string: Some("small".to_string()), version: None });
        devices[1].basic = Some(RawBasicDevice { attributes: Some(attrs2), capacity: None });
    }
    cache.upsert_resource_slice("s1".to_string(), slice);

    let mut claim = unbound_claim("ns", "claim", "gpu.example.com", 1);
    if let Some(devices) = &mut claim.spec.devices {
        if let Some(requests) = &mut devices.requests {
            requests[0].selectors = Some(vec![RawDeviceSelector {
                cel: Some(RawCelSelector {
                    expression: "device.attributes[\"gpu.example.com\"].size == \"big\"".to_string(),
                }),
            }]);
        }
    }
    cache.upsert_resource_claim("ns/claim".to_string(), claim);
    let snapshot = cache.snapshot();

    let p = pod_with_claim("ns", "gpu", "claim");
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(filter_impl(&state, &p, &n).is_success(), "exactly one device (\"big\") should satisfy the selector");
}

#[test]
fn a_bound_claim_already_reserved_for_this_pod_needs_nothing_further() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    let mut claim = unbound_claim("ns", "claim", "gpu.example.com", 1);
    claim.status = Some(crate::cache::dra::RawResourceClaimStatus {
        allocation: Some(crate::cache::dra::RawAllocationResult { devices: None, node_selector: None }),
        reserved_for: Some(vec![crate::cache::dra::RawConsumerReference {
            api_group: None,
            resource: "pods".to_string(),
            name: "p".to_string(),
            uid: "uid-p".to_string(),
        }]),
    });
    cache.upsert_resource_claim("ns/claim".to_string(), claim);
    let snapshot = cache.snapshot();

    let p = pod_with_claim("ns", "gpu", "claim");
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    assert!(status.is_success());

    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(filter_impl(&state, &p, &n).is_success());
}

#[test]
fn a_claim_using_admin_access_is_rejected_as_unimplemented() {
    let mut cache = Cache::new();
    let mut claim = unbound_claim("ns", "claim", "gpu.example.com", 1);
    if let Some(devices) = &mut claim.spec.devices {
        if let Some(requests) = &mut devices.requests {
            requests[0].admin_access = Some(true);
        }
    }
    cache.upsert_resource_claim("ns/claim".to_string(), claim);
    let snapshot = cache.snapshot();

    let p = pod_with_claim("ns", "gpu", "claim");
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    assert!(!status.is_success());
    assert!(!status.code.is_resolvable_by_preemption());
}

#[test]
fn preenqueue_holds_a_pod_whose_template_claim_has_not_been_generated_yet() {
    let mut p = pod("p");
    p.resource_claims = vec![PodClaimRef {
        name: "gpu".to_string(),
        resource_claim_name: None,
        resource_claim_template_name: Some("gpu-template".to_string()),
    }];
    assert!(!pre_enqueue_impl(&p).is_success());
}

#[test]
fn preenqueue_admits_a_pod_whose_template_claim_has_been_generated() {
    let mut p = pod("p");
    p.resource_claims = vec![PodClaimRef {
        name: "gpu".to_string(),
        resource_claim_name: None,
        resource_claim_template_name: Some("gpu-template".to_string()),
    }];
    p.resource_claim_statuses.insert("gpu".to_string(), Some("gpu-abc123".to_string()));
    assert!(pre_enqueue_impl(&p).is_success());
}

#[test]
fn preenqueue_admits_a_pod_referencing_an_already_existing_claim_directly() {
    let p = pod_with_claim("ns", "gpu", "claim");
    assert!(pre_enqueue_impl(&p).is_success());
}

#[test]
fn it_wakes_on_the_events_that_can_progress_a_stuck_claim() {
    let events = events_impl();
    let claim_added = ClusterEvent::new(EventResource::ResourceClaim, ActionType::ADD);
    let slice_added = ClusterEvent::new(EventResource::ResourceSlice, ActionType::ADD);

    assert!(events.iter().any(|e| e.event.matches(&claim_added)));
    assert!(events.iter().any(|e| e.event.matches(&slice_added)));
}
