//! port_mappings_for()/protocol_cri(): spec.containers[].ports[].hostPort
//! (round 82; found in round 80's re-audit) -> CRI's
//! PodSandboxConfig.port_mappings.
use super::*;
use k8s_openapi::api::core::v1::ContainerPort;

fn container_with_ports(ports: Vec<ContainerPort>) -> Container {
    Container { ports: Some(ports), ..Default::default() }
}

fn port(container_port: i32, host_port: Option<i32>, protocol: Option<&str>) -> ContainerPort {
    ContainerPort { container_port, host_port, protocol: protocol.map(str::to_string), ..Default::default() }
}

#[test]
fn no_containers_produces_no_mappings() {
    assert!(port_mappings_for(&[], false).is_empty());
}

#[test]
fn a_container_port_with_no_host_port_produces_no_mapping() {
    let containers = vec![container_with_ports(vec![port(8080, None, None)])];
    assert!(port_mappings_for(&containers, false).is_empty());
}

#[test]
fn an_explicit_zero_host_port_means_no_host_port_per_the_cri_contract() {
    let containers = vec![container_with_ports(vec![port(8080, Some(0), None)])];
    assert!(port_mappings_for(&containers, false).is_empty());
}

#[test]
fn a_real_host_port_produces_a_mapping() {
    let containers = vec![container_with_ports(vec![port(8080, Some(30080), None)])];
    let mappings = port_mappings_for(&containers, false);
    assert_eq!(mappings.len(), 1);
    assert_eq!(mappings[0].container_port, 8080);
    assert_eq!(mappings[0].host_port, 30080);
}

#[test]
fn unset_protocol_defaults_to_tcp() {
    let containers = vec![container_with_ports(vec![port(8080, Some(30080), None)])];
    let mappings = port_mappings_for(&containers, false);
    assert_eq!(mappings[0].protocol, Protocol::Tcp as i32);
}

#[test]
fn udp_protocol_is_carried_through() {
    let containers = vec![container_with_ports(vec![port(53, Some(5353), Some("UDP"))])];
    let mappings = port_mappings_for(&containers, false);
    assert_eq!(mappings[0].protocol, Protocol::Udp as i32);
}

#[test]
fn sctp_protocol_is_carried_through() {
    let containers = vec![container_with_ports(vec![port(9, Some(9), Some("SCTP"))])];
    let mappings = port_mappings_for(&containers, false);
    assert_eq!(mappings[0].protocol, Protocol::Sctp as i32);
}

#[test]
fn an_unrecognized_protocol_falls_back_to_tcp() {
    let containers = vec![container_with_ports(vec![port(8080, Some(30080), Some("BOGUS"))])];
    let mappings = port_mappings_for(&containers, false);
    assert_eq!(mappings[0].protocol, Protocol::Tcp as i32);
}

#[test]
fn host_network_pods_get_no_mappings_at_all() {
    let containers = vec![container_with_ports(vec![port(8080, Some(30080), None)])];
    assert!(port_mappings_for(&containers, true).is_empty());
}

#[test]
fn multiple_containers_and_multiple_ports_are_all_collected() {
    let containers = vec![
        container_with_ports(vec![port(80, Some(8080), None), port(443, Some(8443), None)]),
        container_with_ports(vec![port(9090, Some(9090), Some("UDP"))]),
    ];
    let mappings = port_mappings_for(&containers, false);
    assert_eq!(mappings.len(), 3);
}

#[test]
fn host_ip_is_carried_through_when_set() {
    let mut p = port(8080, Some(30080), None);
    p.host_ip = Some("127.0.0.1".to_string());
    let containers = vec![container_with_ports(vec![p])];
    let mappings = port_mappings_for(&containers, false);
    assert_eq!(mappings[0].host_ip, "127.0.0.1");
}
