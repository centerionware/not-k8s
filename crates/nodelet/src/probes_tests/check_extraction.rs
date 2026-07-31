use super::*;
use k8s_openapi::api::core::v1::{Container, ContainerPort, ExecAction, GRPCAction, HTTPGetAction, Probe, TCPSocketAction};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

fn container_with_port(name: &str, port: i32) -> Container {
    Container {
        name: "app".to_string(),
        ports: Some(vec![ContainerPort { name: Some(name.to_string()), container_port: port, ..Default::default() }]),
        ..Default::default()
    }
}

#[test]
fn http_get_resolves_numeric_port_and_defaults_path_and_scheme() {
    let probe = Probe {
        http_get: Some(HTTPGetAction { port: IntOrString::Int(8080), ..Default::default() }),
        ..Default::default()
    };
    let check = probe_check(&probe, &Container::default());
    assert_eq!(check, ProbeCheck::Http { path: "/".to_string(), port: 8080, https: false });
}

#[test]
fn http_get_resolves_named_port_against_container_ports() {
    let probe = Probe {
        http_get: Some(HTTPGetAction {
            port: IntOrString::String("http".to_string()),
            path: Some("/healthz".to_string()),
            scheme: Some("HTTPS".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let check = probe_check(&probe, &container_with_port("http", 9090));
    assert_eq!(check, ProbeCheck::Http { path: "/healthz".to_string(), port: 9090, https: true });
}

#[test]
fn http_get_named_port_not_found_resolves_to_zero() {
    let probe = Probe {
        http_get: Some(HTTPGetAction { port: IntOrString::String("missing".to_string()), ..Default::default() }),
        ..Default::default()
    };
    let check = probe_check(&probe, &Container::default());
    assert_eq!(check, ProbeCheck::Http { path: "/".to_string(), port: 0, https: false });
}

#[test]
fn tcp_socket_resolves_port() {
    let probe = Probe {
        tcp_socket: Some(TCPSocketAction { port: IntOrString::Int(5432), ..Default::default() }),
        ..Default::default()
    };
    assert_eq!(probe_check(&probe, &Container::default()), ProbeCheck::Tcp { port: 5432 });
}

#[test]
fn exec_extracts_command() {
    let probe = Probe {
        exec: Some(ExecAction { command: Some(vec!["cat".to_string(), "/healthy".to_string()]) }),
        ..Default::default()
    };
    assert_eq!(
        probe_check(&probe, &Container::default()),
        ProbeCheck::Exec { command: vec!["cat".to_string(), "/healthy".to_string()] }
    );
}

#[test]
fn grpc_resolves_port_and_service() {
    let probe = Probe { grpc: Some(GRPCAction { port: 9000, service: Some("my.service".to_string()) }), ..Default::default() };
    let check = probe_check(&probe, &Container::default());
    assert_eq!(check, ProbeCheck::Grpc { port: 9000, service: Some("my.service".to_string()) });
}

#[test]
fn grpc_with_no_service_name_resolves_to_none_service() {
    let probe = Probe { grpc: Some(GRPCAction { port: 9000, service: None }), ..Default::default() };
    let check = probe_check(&probe, &Container::default());
    assert_eq!(check, ProbeCheck::Grpc { port: 9000, service: None });
}

#[test]
fn empty_probe_resolves_to_none() {
    assert_eq!(probe_check(&Probe::default(), &Container::default()), ProbeCheck::None);
}

#[test]
fn timing_uses_kubelet_defaults_when_unset() {
    let timing = probe_timing(&Probe::default());
    assert_eq!(timing.period, std::time::Duration::from_secs(10));
    assert_eq!(timing.timeout, std::time::Duration::from_secs(1));
    assert_eq!(timing.success_threshold, 1);
    assert_eq!(timing.failure_threshold, 3);
    assert_eq!(timing.initial_delay, std::time::Duration::from_secs(0));
}

#[test]
fn timing_honors_explicit_values() {
    let probe = Probe {
        initial_delay_seconds: Some(5),
        period_seconds: Some(20),
        timeout_seconds: Some(3),
        success_threshold: Some(2),
        failure_threshold: Some(5),
        ..Default::default()
    };
    let timing = probe_timing(&probe);
    assert_eq!(timing.initial_delay, std::time::Duration::from_secs(5));
    assert_eq!(timing.period, std::time::Duration::from_secs(20));
    assert_eq!(timing.timeout, std::time::Duration::from_secs(3));
    assert_eq!(timing.success_threshold, 2);
    assert_eq!(timing.failure_threshold, 5);
}
