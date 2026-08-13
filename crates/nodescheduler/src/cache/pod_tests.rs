//! Tests for the pod projection and the resource arithmetic under it.
//!
//! Quantity parsing gets the most attention here because it is the one place
//! a silent misread turns into a placement decision: a memory request parsed
//! as a bare count instead of bytes makes a 128Mi pod look like it needs 128
//! bytes, and it will schedule onto a node that cannot hold it.

use super::*;
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, Pod, PodSpec, ResourceRequirements,
};

fn q(s: &str) -> Quantity {
    Quantity(s.to_string())
}

fn container(name: &str, cpu: Option<&str>, mem: Option<&str>) -> Container {
    let mut requests = BTreeMap::new();
    if let Some(c) = cpu {
        requests.insert("cpu".to_string(), q(c));
    }
    if let Some(m) = mem {
        requests.insert("memory".to_string(), q(m));
    }
    Container {
        name: name.to_string(),
        resources: Some(ResourceRequirements {
            requests: Some(requests),
            ..Default::default()
        }),
        ..Default::default()
    }
}

// ── Quantity parsing ────────────────────────────────────────────────────

#[test]
fn cpu_quantities_parse_to_millicores() {
    assert_eq!(parse_quantity_milli("100m"), 100);
    assert_eq!(parse_quantity_milli("1"), 1000);
    assert_eq!(parse_quantity_milli("0.5"), 500);
    assert_eq!(parse_quantity_milli("2.5"), 2500);
    assert_eq!(parse_quantity_milli("1500m"), 1500);
    assert_eq!(parse_quantity_milli(""), 0);
}

#[test]
fn binary_suffixes_parse_to_bytes() {
    assert_eq!(parse_quantity("1Ki"), 1024);
    assert_eq!(parse_quantity("1Mi"), 1024 * 1024);
    assert_eq!(parse_quantity("1Gi"), 1024 * 1024 * 1024);
    assert_eq!(parse_quantity("128Mi"), 128 * 1024 * 1024);
}

#[test]
fn decimal_suffixes_are_powers_of_ten_not_two() {
    // The classic confusion. 1M is a million bytes, 1Mi is 1048576.
    assert_eq!(parse_quantity("1k"), 1_000);
    assert_eq!(parse_quantity("1M"), 1_000_000);
    assert_eq!(parse_quantity("1G"), 1_000_000_000);
    assert_ne!(parse_quantity("1M"), parse_quantity("1Mi"));
}

#[test]
fn a_bare_byte_count_parses_unchanged() {
    assert_eq!(parse_quantity("128974848"), 128_974_848);
}

#[test]
fn exponent_notation_parses() {
    assert_eq!(parse_quantity("1e3"), 1_000);
    assert_eq!(parse_quantity("1.5e3"), 1_500);
}

#[test]
fn milli_of_a_countable_resource_rounds_up() {
    // Rounding down would hand a pod less than it asked for.
    assert_eq!(parse_quantity("1500m"), 2);
    assert_eq!(parse_quantity("1m"), 1);
}

#[test]
fn an_unparseable_quantity_reads_as_zero_rather_than_panicking() {
    // A malformed quantity is the apiserver's problem to reject; the
    // scheduler must not take the process down over one bad object.
    assert_eq!(parse_quantity("not-a-number"), 0);
    assert_eq!(parse_quantity_milli("¿"), 0);
}

// ── Resource arithmetic ─────────────────────────────────────────────────

#[test]
fn subtraction_clamps_at_zero_instead_of_going_negative() {
    // A negative committed total would make the node look *more* free than
    // empty, and pods would schedule onto capacity that does not exist.
    let mut a = Resources { milli_cpu: 100, memory: 100, ..Default::default() };
    a.sub(&Resources { milli_cpu: 500, memory: 500, ..Default::default() });
    assert_eq!(a.milli_cpu, 0);
    assert_eq!(a.memory, 0);
}

#[test]
fn extended_resources_round_trip_by_name() {
    let mut r = Resources::default();
    r.set("nvidia.com/gpu", 2);
    assert_eq!(r.get("nvidia.com/gpu"), 2);
    assert_eq!(r.get("amd.com/gpu"), 0, "an unrequested resource reads as zero");
}

#[test]
fn hugepages_are_kept_apart_from_extended_resources() {
    let mut r = Resources::default();
    r.set("hugepages-2Mi", 4);
    assert_eq!(r.get("hugepages-2Mi"), 4);
    assert!(r.hugepages.contains_key("hugepages-2Mi"));
    assert!(r.extended.is_empty());
}

#[test]
fn names_lists_only_resources_actually_requested() {
    let mut r = Resources { milli_cpu: 100, ..Default::default() };
    r.set("nvidia.com/gpu", 1);
    let names = r.names();
    assert!(names.contains(&"cpu".to_string()));
    assert!(names.contains(&"nvidia.com/gpu".to_string()));
    assert!(!names.contains(&"memory".to_string()));
}

// ── Pod requests ────────────────────────────────────────────────────────

#[test]
fn regular_container_requests_are_summed() {
    let pod = Pod {
        spec: Some(PodSpec {
            containers: vec![
                container("a", Some("100m"), Some("128Mi")),
                container("b", Some("200m"), Some("256Mi")),
            ],
            ..Default::default()
        }),
        ..Default::default()
    };
    let r = pod_requests(&pod);
    assert_eq!(r.milli_cpu, 300);
    assert_eq!(r.memory, 384 * 1024 * 1024);
}

#[test]
fn init_containers_take_the_max_not_the_sum() {
    // They run one at a time, to completion, before the regular containers
    // start — so their peak demand is the largest single one.
    let pod = Pod {
        spec: Some(PodSpec {
            containers: vec![container("main", Some("100m"), None)],
            init_containers: Some(vec![
                container("init-a", Some("500m"), None),
                container("init-b", Some("800m"), None),
            ]),
            ..Default::default()
        }),
        ..Default::default()
    };
    // max(sum of regular = 100m, max of inits = 800m) = 800m
    assert_eq!(pod_requests(&pod).milli_cpu, 800);
}

#[test]
fn a_sidecar_counts_toward_the_sum_because_it_runs_alongside() {
    // An init container with restartPolicy: Always is a sidecar — it does not
    // complete before the regular containers, so treating it as init-max
    // under-counts the node's real commitment.
    let mut sidecar = container("sidecar", Some("300m"), None);
    sidecar.restart_policy = Some("Always".to_string());
    let pod = Pod {
        spec: Some(PodSpec {
            containers: vec![container("main", Some("100m"), None)],
            init_containers: Some(vec![sidecar]),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(pod_requests(&pod).milli_cpu, 400);
}

#[test]
fn pod_overhead_is_added_on_top() {
    let pod = Pod {
        spec: Some(PodSpec {
            containers: vec![container("main", Some("100m"), None)],
            overhead: Some(BTreeMap::from([("cpu".to_string(), q("50m"))])),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(pod_requests(&pod).milli_cpu, 150);
}

#[test]
fn a_pod_requesting_nothing_requests_nothing() {
    // Filtering must see the truth: a pod with no requests genuinely fits
    // anywhere. The substitution below is a scoring-only concern.
    let pod = Pod {
        spec: Some(PodSpec {
            containers: vec![Container { name: "c".to_string(), ..Default::default() }],
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(pod_requests(&pod), Resources::default());
}

#[test]
fn non_zero_requests_substitutes_only_the_unspecified_ones() {
    let info = PodInfo {
        requests: Resources { milli_cpu: 250, ..Default::default() },
        ..Default::default()
    };
    let nz = info.non_zero_requests();
    assert_eq!(nz.milli_cpu, 250, "an explicit request is never overridden");
    assert_eq!(nz.memory, DEFAULT_MEMORY_REQUEST);
}

#[test]
fn the_substituted_defaults_are_100m_and_200mi() {
    // Widely misquoted as 1000m/128Mi. Wrong values here change bin-packing
    // subtly and permanently.
    assert_eq!(DEFAULT_MILLI_CPU_REQUEST, 100);
    assert_eq!(DEFAULT_MEMORY_REQUEST, 200 * 1024 * 1024);
}

// ── Host ports ──────────────────────────────────────────────────────────

#[test]
fn a_wildcard_host_port_collides_with_every_address() {
    let wildcard = HostPort { protocol: "TCP".into(), ip: "0.0.0.0".into(), port: 80 };
    let specific = HostPort { protocol: "TCP".into(), ip: "10.0.0.1".into(), port: 80 };
    assert!(wildcard.conflicts_with(&specific));
    assert!(specific.conflicts_with(&wildcard));
}

#[test]
fn an_empty_host_ip_is_also_a_wildcard() {
    let empty = HostPort { protocol: "TCP".into(), ip: String::new(), port: 80 };
    let specific = HostPort { protocol: "TCP".into(), ip: "10.0.0.1".into(), port: 80 };
    assert!(empty.conflicts_with(&specific));
}

#[test]
fn different_protocols_on_one_port_do_not_collide() {
    let tcp = HostPort { protocol: "TCP".into(), ip: "0.0.0.0".into(), port: 53 };
    let udp = HostPort { protocol: "UDP".into(), ip: "0.0.0.0".into(), port: 53 };
    assert!(!tcp.conflicts_with(&udp));
}

#[test]
fn distinct_addresses_on_one_port_do_not_collide() {
    let a = HostPort { protocol: "TCP".into(), ip: "10.0.0.1".into(), port: 80 };
    let b = HostPort { protocol: "TCP".into(), ip: "10.0.0.2".into(), port: 80 };
    assert!(!a.conflicts_with(&b));
}

#[test]
fn host_port_zero_is_unset_and_is_not_projected() {
    // 0 means "no host port", not "port zero". Projecting it would make every
    // pod that omits hostPort collide with every other one.
    let pod = Pod {
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "c".to_string(),
                ports: Some(vec![ContainerPort {
                    container_port: 8080,
                    host_port: Some(0),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    let info = PodInfo::from_pod(&pod, k8s_openapi::jiff::Timestamp::now());
    assert!(info.host_ports.is_empty());
}

// ── Projection ──────────────────────────────────────────────────────────

#[test]
fn an_unscheduled_pod_has_no_node_name() {
    // The single field that decides whether a pod is the queue's business or
    // the cache's, so an empty string must not read as "placed on node ''".
    let pod = Pod {
        spec: Some(PodSpec { node_name: Some(String::new()), ..Default::default() }),
        ..Default::default()
    };
    assert_eq!(PodInfo::from_pod(&pod, k8s_openapi::jiff::Timestamp::now()).node_name, None);
}

#[test]
fn the_scheduler_name_defaults_to_default_scheduler() {
    let pod = Pod { spec: Some(PodSpec::default()), ..Default::default() };
    assert_eq!(
        PodInfo::from_pod(&pod, k8s_openapi::jiff::Timestamp::now()).scheduler_name,
        "default-scheduler"
    );
}
