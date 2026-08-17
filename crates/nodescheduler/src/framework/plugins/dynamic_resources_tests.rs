use super::*;
use crate::cache::dra::{
    RawBasicDevice, RawCelSelector, RawDevice, RawDeviceAttribute, RawDeviceClassSpec,
    RawDeviceRequest, RawDeviceSelector, RawResourceClaimSpec, RawResourcePool,
    RawResourceSliceSpec,
};
use crate::cache::{Cache, PodClaimRef, RawDeviceClass, RawResourceClaim, RawResourceSlice};
use crate::framework::plugins::testutil::pod;
use k8s_openapi::api::core::v1::Node as ApiNode;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

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
                    exactly: Some(crate::cache::dra::RawExactDeviceRequest {
                        device_class_name: Some(class.to_string()),
                        selectors: None,
                        allocation_mode: None,
                        count: Some(count),
                        admin_access: None,
                    }),
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
            per_device_node_selection: None,
            devices: Some(
                device_names
                    .iter()
                    .map(|n| RawDevice {
                        name: n.to_string(),
                        basic: RawBasicDevice { attributes: None, capacity: None, ..Default::default() },
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

fn pod_with_claims(namespace: &str, claims: &[(&str, &str)]) -> PodInfo {
    let mut p = pod("p");
    p.namespace = namespace.to_string();
    p.uid = "uid-p".to_string();
    p.resource_claims = claims
        .iter()
        .map(|(pod_claim_name, claim_name)| PodClaimRef {
            name: (*pod_claim_name).to_string(),
            resource_claim_name: Some((*claim_name).to_string()),
            resource_claim_template_name: None,
        })
        .collect();
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
fn allocation_backtracks_when_an_early_valid_pick_is_needed_by_a_later_request() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_device_class("broad".to_string(), class_with_cel("broad", "true"));
    cache.upsert_device_class(
        "narrow".to_string(),
        class_with_cel(
            "narrow",
            r#"device.attributes["gpu.example.com"].kind == "narrow""#,
        ),
    );

    let mut slice = slice_with_devices("gpu.example.com", "n1", &["only-narrow", "broad-only"]);
    let devices = slice.spec.devices.as_mut().unwrap();
    devices[0].basic.attributes = Some(std::collections::BTreeMap::from([(
        "kind".to_string(),
        RawDeviceAttribute {
            bool: None,
            int: None,
            string: Some("narrow".to_string()),
            version: None,
        },
    )]));
    devices[1].basic.attributes = Some(std::collections::BTreeMap::from([(
        "kind".to_string(),
        RawDeviceAttribute {
            bool: None,
            int: None,
            string: Some("broad".to_string()),
            version: None,
        },
    )]));
    cache.upsert_resource_slice("s1".to_string(), slice);

    let exact = |name: &str, class: &str| RawDeviceRequest {
        name: name.to_string(),
        exactly: Some(crate::cache::dra::RawExactDeviceRequest {
            device_class_name: Some(class.to_string()),
            selectors: None,
            allocation_mode: None,
            count: Some(1),
            admin_access: None,
        }),
        first_available: None,
    };
    let claim = RawResourceClaim {
        metadata: claim_meta("ns", "claim"),
        spec: RawResourceClaimSpec {
            devices: Some(crate::cache::dra::RawDeviceClaim {
                // The broad request sees only-narrow first. A greedy pass
                // consumes it and incorrectly rejects the narrow request;
                // upstream rolls that choice back and uses broad-only.
                requests: Some(vec![exact("broad", "broad"), exact("narrow", "narrow")]),
                constraints: None,
            }),
        },
        status: None,
    };
    cache.upsert_resource_claim("ns/claim".to_string(), claim);
    let snapshot = cache.snapshot();

    let p = pod_with_claim("ns", "gpu", "claim");
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    assert!(status.is_success());
    assert!(filter_impl(&state, &p, snapshot.node("n1").unwrap()).is_success());

    let wanted = state.read::<WantedClaims>(NAME).unwrap();
    let ClaimPlan::Allocate { by_node, .. } = &wanted.0[0] else { panic!("expected allocation") };
    let allocation = by_node.get("n1").unwrap();
    assert_eq!(allocation[0].device, "broad-only");
    assert_eq!(allocation[1].device, "only-narrow");
}

#[test]
fn allocation_is_exclusive_and_backtracks_across_separate_claims() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_device_class("broad".to_string(), class_with_cel("broad", "true"));
    cache.upsert_device_class(
        "narrow".to_string(),
        class_with_cel(
            "narrow",
            r#"device.attributes["gpu.example.com"].kind == "narrow""#,
        ),
    );

    let mut slice =
        slice_with_devices("gpu.example.com", "n1", &["only-narrow", "broad-only"]);
    slice.spec.devices.as_mut().unwrap()[0].basic.attributes = Some(
        std::collections::BTreeMap::from([(
            "kind".to_string(),
            RawDeviceAttribute {
                bool: None,
                int: None,
                string: Some("narrow".to_string()),
                version: None,
            },
        )]),
    );
    slice.spec.devices.as_mut().unwrap()[1].basic.attributes = Some(
        std::collections::BTreeMap::from([(
            "kind".to_string(),
            RawDeviceAttribute {
                bool: None,
                int: None,
                string: Some("broad".to_string()),
                version: None,
            },
        )]),
    );
    cache.upsert_resource_slice("s1".to_string(), slice);
    cache.upsert_resource_claim(
        "ns/broad-claim".to_string(),
        unbound_claim("ns", "broad-claim", "broad", 1),
    );
    cache.upsert_resource_claim(
        "ns/narrow-claim".to_string(),
        unbound_claim("ns", "narrow-claim", "narrow", 1),
    );

    let snapshot = cache.snapshot();
    let p = pod_with_claims(
        "ns",
        &[("broad", "broad-claim"), ("narrow", "narrow-claim")],
    );
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    assert!(status.is_success());
    assert!(filter_impl(&state, &p, snapshot.node("n1").unwrap()).is_success());

    let wanted = state.read::<WantedClaims>(NAME).unwrap();
    let ClaimPlan::Allocate { by_node: broad, .. } = &wanted.0[0] else {
        panic!("expected Allocate")
    };
    let ClaimPlan::Allocate { by_node: narrow, .. } = &wanted.0[1] else {
        panic!("expected Allocate")
    };
    assert_eq!(broad["n1"][0].device, "broad-only");
    assert_eq!(narrow["n1"][0].device, "only-narrow");
}

#[test]
fn two_separate_claims_cannot_receive_the_same_exclusive_device() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_device_class("gpu".to_string(), class_with_cel("gpu", "true"));
    cache.upsert_resource_slice(
        "s1".to_string(),
        slice_with_devices("gpu.example.com", "n1", &["only-device"]),
    );
    cache.upsert_resource_claim(
        "ns/claim-a".to_string(),
        unbound_claim("ns", "claim-a", "gpu", 1),
    );
    cache.upsert_resource_claim(
        "ns/claim-b".to_string(),
        unbound_claim("ns", "claim-b", "gpu", 1),
    );

    let snapshot = cache.snapshot();
    let p = pod_with_claims("ns", &[("a", "claim-a"), ("b", "claim-b")]);
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    assert!(
        !filter_impl(&state, &p, snapshot.node("n1").unwrap()).is_success(),
        "one exclusive device cannot satisfy two claims from the same pod"
    );
}

#[test]
fn a_template_claim_must_be_controlled_by_the_pod_that_references_it() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_device_class("gpu".to_string(), class_with_cel("gpu", "true"));
    cache.upsert_resource_slice(
        "s1".to_string(),
        slice_with_devices("gpu.example.com", "n1", &["gpu-0"]),
    );
    let mut claim = unbound_claim("ns", "generated", "gpu", 1);
    claim.metadata.owner_references = Some(vec![OwnerReference {
        api_version: "v1".to_string(),
        kind: "Pod".to_string(),
        name: "some-other-pod".to_string(),
        uid: "some-other-uid".to_string(),
        controller: Some(true),
        block_owner_deletion: None,
    }]);
    cache.upsert_resource_claim("ns/generated".to_string(), claim);

    let snapshot = cache.snapshot();
    let mut p = pod_with_claim("ns", "gpu", "generated");
    p.resource_claims[0].resource_claim_name = None;
    p.resource_claims[0].resource_claim_template_name = Some("gpu-template".to_string());
    p.resource_claim_statuses
        .insert("gpu".to_string(), Some("generated".to_string()));
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());

    assert_eq!(
        status.code,
        crate::framework::status::Code::UnschedulableAndUnresolvable
    );
    assert!(status.reasons[0].contains("pod is not owner"));
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
        devices[0].basic = RawBasicDevice { attributes: Some(attrs), capacity: None, ..Default::default() };
        let mut attrs2 = std::collections::BTreeMap::new();
        attrs2.insert("size".to_string(), RawDeviceAttribute { bool: None, int: None, string: Some("small".to_string()), version: None });
        devices[1].basic = RawBasicDevice { attributes: Some(attrs2), capacity: None, ..Default::default() };
    }
    cache.upsert_resource_slice("s1".to_string(), slice);

    let mut claim = unbound_claim("ns", "claim", "gpu.example.com", 1);
    if let Some(devices) = &mut claim.spec.devices {
        if let Some(requests) = &mut devices.requests {
            if let Some(exactly) = &mut requests[0].exactly {
                exactly.selectors = Some(vec![RawDeviceSelector {
                    cel: Some(RawCelSelector {
                        expression: "device.attributes[\"gpu.example.com\"].size == \"big\"".to_string(),
                    }),
                }]);
            }
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
fn a_capacity_selector_uses_exact_quantity_arithmetic() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_device_class("gpu.example.com".to_string(), class_with_cel("gpu.example.com", "true"));
    let mut slice = slice_with_devices("gpu.example.com", "n1", &["precise"]);
    slice.spec.devices.as_mut().unwrap()[0].basic.capacity = Some(
        std::collections::BTreeMap::from([(
            "memory".to_string(),
            crate::cache::dra::RawDeviceCapacity {
                value: k8s_openapi::apimachinery::pkg::api::resource::Quantity(
                    "9007199254740993".to_string(),
                ),
            },
        )]),
    );
    cache.upsert_resource_slice("s1".to_string(), slice);

    let mut claim = unbound_claim("ns", "claim", "gpu.example.com", 1);
    claim.spec.devices.as_mut().unwrap().requests.as_mut().unwrap()[0]
        .exactly.as_mut().unwrap().selectors = Some(vec![RawDeviceSelector {
            cel: Some(RawCelSelector {
                expression: r#"device.capacity["gpu.example.com"].memory.isGreaterThan(quantity("9007199254740992"))"#.to_string(),
            }),
        }]);
    cache.upsert_resource_claim("ns/claim".to_string(), claim);
    let snapshot = cache.snapshot();

    let p = pod_with_claim("ns", "gpu", "claim");
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    assert!(filter_impl(&state, &p, snapshot.node("n1").unwrap()).is_success());
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
fn admin_access_lets_two_claims_share_the_same_device() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_device_class(
        "gpu.example.com".to_string(),
        class_with_cel("gpu.example.com", "device.driver == \"gpu.example.com\""),
    );
    cache.upsert_resource_slice("s1".to_string(), slice_with_devices("gpu.example.com", "n1", &["gpu-0"]));

    let mut admin_claim = unbound_claim("ns", "admin-claim", "gpu.example.com", 1);
    if let Some(devices) = &mut admin_claim.spec.devices {
        if let Some(requests) = &mut devices.requests {
            if let Some(exactly) = &mut requests[0].exactly {
                exactly.admin_access = Some(true);
            }
        }
    }
    cache.upsert_resource_claim("ns/admin-claim".to_string(), admin_claim);

    // The ordinary (non-admin) claim already owns the only device via the
    // assume cache — an admin-access request must be allocatable anyway.
    let mut excluded = HashSet::new();
    excluded.insert(("gpu.example.com".to_string(), "n1".to_string(), "gpu-0".to_string()));

    let p = pod_with_claim("ns", "gpu", "admin-claim");
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &p, &cache.snapshot(), &excluded);
    assert!(status.is_success());
    let n = cache.snapshot().node("n1").unwrap().as_ref().clone();
    assert!(filter_impl(&state, &p, &n).is_success(), "adminAccess must not be blocked by another claim's ordinary exclusive hold");
}

#[test]
fn allocation_mode_all_takes_every_matching_device() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_device_class("gpu.example.com".to_string(), class_with_cel("gpu.example.com", "device.driver == \"gpu.example.com\""));
    cache.upsert_resource_slice("s1".to_string(), slice_with_devices("gpu.example.com", "n1", &["gpu-0", "gpu-1"]));

    let mut claim = unbound_claim("ns", "claim", "gpu.example.com", 1);
    if let Some(devices) = &mut claim.spec.devices {
        if let Some(requests) = &mut devices.requests {
            if let Some(exactly) = &mut requests[0].exactly {
                exactly.allocation_mode = Some("All".to_string());
                exactly.count = None;
            }
        }
    }
    cache.upsert_resource_claim("ns/claim".to_string(), claim);
    let snapshot = cache.snapshot();

    let p = pod_with_claim("ns", "gpu", "claim");
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    assert!(status.is_success());
    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(filter_impl(&state, &p, &n).is_success());

    let wanted = state.read::<WantedClaims>(NAME).unwrap();
    let ClaimPlan::Allocate { by_node, .. } = &wanted.0[0] else { panic!("expected Allocate") };
    assert_eq!(by_node.get("n1").unwrap().len(), 2, "'All' must take every matching device, not just one");
}

#[test]
fn allocation_mode_all_fails_when_nothing_matches() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_device_class(
        "gpu.example.com".to_string(),
        class_with_cel("gpu.example.com", "device.driver == \"nonexistent.example.com\""),
    );
    cache.upsert_resource_slice("s1".to_string(), slice_with_devices("gpu.example.com", "n1", &["gpu-0"]));
    let mut claim = unbound_claim("ns", "claim", "gpu.example.com", 1);
    if let Some(devices) = &mut claim.spec.devices {
        if let Some(requests) = &mut devices.requests {
            if let Some(exactly) = &mut requests[0].exactly {
                exactly.allocation_mode = Some("All".to_string());
                exactly.count = None;
            }
        }
    }
    cache.upsert_resource_claim("ns/claim".to_string(), claim);
    let snapshot = cache.snapshot();

    let p = pod_with_claim("ns", "gpu", "claim");
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(!filter_impl(&state, &p, &n).is_success(), "'All' with zero matches must not trivially succeed");
}

#[test]
fn allocation_mode_all_fails_when_one_of_the_matching_devices_is_busy() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_device_class("gpu.example.com".to_string(), class_with_cel("gpu.example.com", "true"));
    cache.upsert_resource_slice(
        "s1".to_string(),
        slice_with_devices("gpu.example.com", "n1", &["gpu-0", "gpu-1"]),
    );
    let mut claim = unbound_claim("ns", "claim", "gpu.example.com", 1);
    claim.spec.devices.as_mut().unwrap().requests.as_mut().unwrap()[0]
        .exactly.as_mut().unwrap().allocation_mode = Some("All".to_string());
    cache.upsert_resource_claim("ns/claim".to_string(), claim);
    let snapshot = cache.snapshot();
    let mut excluded = no_excluded();
    excluded.insert(("gpu.example.com".to_string(), "n1".to_string(), "gpu-0".to_string()));

    let p = pod_with_claim("ns", "gpu", "claim");
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &excluded);
    assert!(
        !filter_impl(&state, &p, snapshot.node("n1").unwrap()).is_success(),
        "All means every matching device, not every matching device that happens to be free"
    );
}

#[test]
fn first_available_falls_through_to_a_later_subrequest() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_device_class("gpu.example.com".to_string(), class_with_cel("gpu.example.com", "device.driver == \"gpu.example.com\""));
    cache.upsert_resource_slice("s1".to_string(), slice_with_devices("gpu.example.com", "n1", &["gpu-0"]));

    let claim = RawResourceClaim {
        metadata: claim_meta("ns", "claim"),
        spec: RawResourceClaimSpec {
            devices: Some(crate::cache::dra::RawDeviceClaim {
                requests: Some(vec![RawDeviceRequest {
                    name: "req".to_string(),
                    exactly: None,
                    first_available: Some(vec![
                        crate::cache::dra::RawDeviceSubRequest {
                            name: "primary".to_string(),
                            device_class_name: Some("nonexistent-class".to_string()),
                            selectors: None,
                            allocation_mode: None,
                            count: Some(1),
                        },
                        crate::cache::dra::RawDeviceSubRequest {
                            name: "fallback".to_string(),
                            device_class_name: Some("gpu.example.com".to_string()),
                            selectors: None,
                            allocation_mode: None,
                            count: Some(1),
                        },
                    ]),
                }]),
                constraints: None,
            }),
        },
        status: None,
    };
    cache.upsert_resource_claim("ns/claim".to_string(), claim);
    let snapshot = cache.snapshot();

    let p = pod_with_claim("ns", "gpu", "claim");
    let mut state = CycleState::default();
    let (status, _) = pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    assert!(status.is_success());
    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(filter_impl(&state, &p, &n).is_success());

    let wanted = state.read::<WantedClaims>(NAME).unwrap();
    let ClaimPlan::Allocate { by_node, .. } = &wanted.0[0] else { panic!("expected Allocate") };
    assert_eq!(by_node.get("n1").unwrap()[0].request, "req/fallback", "the fallback subrequest's own name must be recorded");
}

#[test]
fn a_match_attribute_constraint_rejects_a_device_set_with_different_values() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_device_class("gpu.example.com".to_string(), class_with_cel("gpu.example.com", "device.driver == \"gpu.example.com\""));
    let mut slice = slice_with_devices("gpu.example.com", "n1", &["gpu-0", "gpu-1"]);
    if let Some(devices) = &mut slice.spec.devices {
        for (i, numa) in [("gpu-0", "0"), ("gpu-1", "1")] {
            let d = devices.iter_mut().find(|d| d.name == i).unwrap();
            let mut attrs = std::collections::BTreeMap::new();
            attrs.insert("numa".to_string(), RawDeviceAttribute { bool: None, int: None, string: Some(numa.to_string()), version: None });
            d.basic.attributes = Some(attrs);
        }
    }
    cache.upsert_resource_slice("s1".to_string(), slice);

    // Two requests, one device each, constrained to share the same "numa"
    // attribute value. Only one device of each NUMA node exists, so the
    // constraint can never be satisfied by two distinct devices here.
    let claim = RawResourceClaim {
        metadata: claim_meta("ns", "claim"),
        spec: RawResourceClaimSpec {
            devices: Some(crate::cache::dra::RawDeviceClaim {
                requests: Some(vec![
                    RawDeviceRequest {
                        name: "a".to_string(),
                        exactly: Some(crate::cache::dra::RawExactDeviceRequest {
                            device_class_name: Some("gpu.example.com".to_string()),
                            selectors: None,
                            allocation_mode: None,
                            count: Some(1),
                            admin_access: None,
                        }),
                        first_available: None,
                    },
                    RawDeviceRequest {
                        name: "b".to_string(),
                        exactly: Some(crate::cache::dra::RawExactDeviceRequest {
                            device_class_name: Some("gpu.example.com".to_string()),
                            selectors: None,
                            allocation_mode: None,
                            count: Some(1),
                            admin_access: None,
                        }),
                        first_available: None,
                    },
                ]),
                constraints: Some(vec![crate::cache::dra::RawDeviceConstraint {
                    match_attribute: Some("gpu.example.com/numa".to_string()),
                    requests: vec![],
                }]),
            }),
        },
        status: None,
    };
    cache.upsert_resource_claim("ns/claim".to_string(), claim);
    let snapshot = cache.snapshot();

    let p = pod_with_claim("ns", "gpu", "claim");
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(
        !filter_impl(&state, &p, &n).is_success(),
        "request a takes gpu-0 (numa=0); request b's only remaining device gpu-1 (numa=1) violates the constraint"
    );
}

#[test]
fn a_match_attribute_constraint_admits_a_consistent_device_set() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_device_class("gpu.example.com".to_string(), class_with_cel("gpu.example.com", "device.driver == \"gpu.example.com\""));
    let mut slice = slice_with_devices("gpu.example.com", "n1", &["gpu-0", "gpu-1"]);
    if let Some(devices) = &mut slice.spec.devices {
        for name in ["gpu-0", "gpu-1"] {
            let d = devices.iter_mut().find(|d| d.name == name).unwrap();
            let mut attrs = std::collections::BTreeMap::new();
            attrs.insert("numa".to_string(), RawDeviceAttribute { bool: None, int: None, string: Some("0".to_string()), version: None });
            d.basic.attributes = Some(attrs);
        }
    }
    cache.upsert_resource_slice("s1".to_string(), slice);

    let claim = RawResourceClaim {
        metadata: claim_meta("ns", "claim"),
        spec: RawResourceClaimSpec {
            devices: Some(crate::cache::dra::RawDeviceClaim {
                requests: Some(vec![
                    RawDeviceRequest {
                        name: "a".to_string(),
                        exactly: Some(crate::cache::dra::RawExactDeviceRequest {
                            device_class_name: Some("gpu.example.com".to_string()),
                            selectors: None,
                            allocation_mode: None,
                            count: Some(1),
                            admin_access: None,
                        }),
                        first_available: None,
                    },
                    RawDeviceRequest {
                        name: "b".to_string(),
                        exactly: Some(crate::cache::dra::RawExactDeviceRequest {
                            device_class_name: Some("gpu.example.com".to_string()),
                            selectors: None,
                            allocation_mode: None,
                            count: Some(1),
                            admin_access: None,
                        }),
                        first_available: None,
                    },
                ]),
                constraints: Some(vec![crate::cache::dra::RawDeviceConstraint {
                    match_attribute: Some("gpu.example.com/numa".to_string()),
                    requests: vec![],
                }]),
            }),
        },
        status: None,
    };
    cache.upsert_resource_claim("ns/claim".to_string(), claim);
    let snapshot = cache.snapshot();

    let p = pod_with_claim("ns", "gpu", "claim");
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    let n = snapshot.node("n1").unwrap().as_ref().clone();
    assert!(filter_impl(&state, &p, &n).is_success(), "both devices share numa=0, so the constraint is satisfiable");
}

#[test]
fn per_device_node_selection_scopes_each_device_to_its_own_node() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    cache.upsert_node(&api_node("n2"));
    cache.upsert_device_class("gpu.example.com".to_string(), class_with_cel("gpu.example.com", "device.driver == \"gpu.example.com\""));

    let mut slice = RawResourceSlice {
        metadata: ObjectMeta { name: Some("s1".to_string()), ..Default::default() },
        spec: RawResourceSliceSpec {
            driver: "gpu.example.com".to_string(),
            pool: RawResourcePool { name: "pool".to_string(), generation: Some(1), resource_slice_count: Some(1) },
            node_name: None,
            all_nodes: None,
            node_selector: None,
            per_device_node_selection: Some(true),
            devices: Some(vec![
                RawDevice { name: "gpu-on-n1".to_string(), basic: RawBasicDevice { node_name: Some("n1".to_string()), ..Default::default() } },
                RawDevice { name: "gpu-on-n2".to_string(), basic: RawBasicDevice { node_name: Some("n2".to_string()), ..Default::default() } },
            ]),
        },
    };
    // per_device_node_selection needs no slice-level node_name/all_nodes.
    slice.spec.node_name = None;
    cache.upsert_resource_slice("s1".to_string(), slice);
    cache.upsert_resource_claim("ns/claim".to_string(), unbound_claim("ns", "claim", "gpu.example.com", 1));
    let snapshot = cache.snapshot();

    let p = pod_with_claim("ns", "gpu", "claim");
    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());

    let wanted = state.read::<WantedClaims>(NAME).unwrap();
    let ClaimPlan::Allocate { by_node, .. } = &wanted.0[0] else { panic!("expected Allocate") };
    assert_eq!(by_node.get("n1").unwrap()[0].device, "gpu-on-n1");
    assert_eq!(by_node.get("n2").unwrap()[0].device, "gpu-on-n2");
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

// ── PostFilter: freeing a claim stuck on an unreachable topology ────────

fn bound_claim_on_unreachable_topology(reserved_for: Vec<crate::cache::dra::RawConsumerReference>) -> RawResourceClaim {
    let mut claim = unbound_claim("ns", "claim", "gpu.example.com", 1);
    claim.status = Some(crate::cache::dra::RawResourceClaimStatus {
        allocation: Some(crate::cache::dra::RawAllocationResult {
            devices: None,
            // No real node satisfies this — the whole point of the test.
            node_selector: Some(k8s_openapi::api::core::v1::NodeSelector {
                node_selector_terms: vec![k8s_openapi::api::core::v1::NodeSelectorTerm {
                    match_expressions: Some(vec![k8s_openapi::api::core::v1::NodeSelectorRequirement {
                        key: "nonexistent-label".to_string(),
                        operator: "Exists".to_string(),
                        values: None,
                    }]),
                    match_fields: None,
                }],
            }),
        }),
        reserved_for: Some(reserved_for),
    });
    claim
}

fn consumer(pod: &PodInfo) -> crate::cache::dra::RawConsumerReference {
    crate::cache::dra::RawConsumerReference {
        api_group: None,
        resource: "pods".to_string(),
        name: pod.name.clone(),
        uid: pod.uid.clone(),
    }
}

#[test]
fn post_filter_frees_a_claim_reserved_only_for_this_pod_on_an_unreachable_topology() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    let p = pod_with_claim("ns", "gpu", "claim");
    cache.upsert_resource_claim("ns/claim".to_string(), bound_claim_on_unreachable_topology(vec![consumer(&p)]));
    let snapshot = cache.snapshot();

    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    let wanted = state.read::<WantedClaims>(NAME).unwrap();

    let decision = post_filter_impl(&wanted, &p, &snapshot);
    assert_eq!(decision, Some(("ns".to_string(), "claim".to_string())));
}

#[test]
fn post_filter_frees_a_claim_with_an_empty_reservation_too() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    let p = pod_with_claim("ns", "gpu", "claim");
    cache.upsert_resource_claim("ns/claim".to_string(), bound_claim_on_unreachable_topology(vec![]));
    let snapshot = cache.snapshot();

    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    let wanted = state.read::<WantedClaims>(NAME).unwrap();

    assert!(post_filter_impl(&wanted, &p, &snapshot).is_some());
}

#[test]
fn post_filter_leaves_a_claim_alone_if_another_consumer_still_reserves_it() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    let p = pod_with_claim("ns", "gpu", "claim");
    let other = crate::cache::dra::RawConsumerReference {
        api_group: None,
        resource: "pods".to_string(),
        name: "other-pod".to_string(),
        uid: "other-uid".to_string(),
    };
    cache.upsert_resource_claim("ns/claim".to_string(), bound_claim_on_unreachable_topology(vec![other]));
    let snapshot = cache.snapshot();

    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    let wanted = state.read::<WantedClaims>(NAME).unwrap();

    assert_eq!(
        post_filter_impl(&wanted, &p, &snapshot),
        None,
        "freeing a claim another pod still holds a reservation on would break that pod"
    );
}

#[test]
fn post_filter_leaves_a_claim_alone_if_some_node_can_actually_reach_it() {
    let mut cache = Cache::new();
    cache.upsert_node(&api_node("n1"));
    let p = pod_with_claim("ns", "gpu", "claim");
    let mut claim = unbound_claim("ns", "claim", "gpu.example.com", 1);
    claim.status = Some(crate::cache::dra::RawResourceClaimStatus {
        allocation: Some(crate::cache::dra::RawAllocationResult { devices: None, node_selector: None }),
        reserved_for: Some(vec![consumer(&p)]),
    });
    cache.upsert_resource_claim("ns/claim".to_string(), claim);
    let snapshot = cache.snapshot();

    let mut state = CycleState::default();
    pre_filter_impl(&mut state, &p, &snapshot, &no_excluded());
    let wanted = state.read::<WantedClaims>(NAME).unwrap();

    assert_eq!(post_filter_impl(&wanted, &p, &snapshot), None, "a claim with no node_selector is reachable from any node");
}

#[test]
fn it_wakes_on_the_events_that_can_progress_a_stuck_claim() {
    let events = events_impl();
    let got: Vec<(EventResource, ActionType)> = events
        .iter()
        .map(|event| (event.event.resource, event.event.action))
        .collect();
    assert_eq!(
        got,
        vec![
            (EventResource::Node, ActionType::ADD | ActionType::UPDATE_NODE_LABEL),
            (EventResource::ResourceClaim, ActionType::ADD | ActionType::UPDATE),
            (EventResource::ResourceSlice, ActionType::ADD | ActionType::UPDATE),
            (EventResource::DeviceClass, ActionType::ADD | ActionType::UPDATE),
            (
                EventResource::UnschedulablePod,
                ActionType::UPDATE_POD_GENERATED_RESOURCE_CLAIM,
            ),
        ]
    );
}
