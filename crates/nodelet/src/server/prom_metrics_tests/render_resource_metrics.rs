use super::*;
use crate::runtime::{ContainerUsage, UsageStats};

fn pod_with_one_container(cpu_nanos: Option<u64>, mem_bytes: Option<u64>) -> PodUsage {
    PodUsage {
        namespace: "default".to_string(),
        name: "web".to_string(),
        uid: "abc-123".to_string(),
        pod: UsageStats { cpu_usage_core_nano_seconds: cpu_nanos, memory_working_set_bytes: mem_bytes, ..Default::default() },
        containers: vec![ContainerUsage {
            name: "app".to_string(),
            stats: UsageStats { cpu_usage_core_nano_seconds: cpu_nanos, memory_working_set_bytes: mem_bytes, ..Default::default() },
        }],
        ..Default::default()
    }
}

#[test]
fn includes_help_and_type_lines_for_every_metric() {
    let out = render_resource_metrics("node-1", None, None, &[]);
    for name in [
        "node_cpu_usage_seconds_total",
        "node_memory_working_set_bytes",
        "pod_cpu_usage_seconds_total",
        "pod_memory_working_set_bytes",
        "container_cpu_usage_seconds_total",
        "container_memory_working_set_bytes",
    ] {
        assert!(out.contains(&format!("# HELP {name} ")), "missing HELP for {name}");
        assert!(out.contains(&format!("# TYPE {name} ")), "missing TYPE for {name}");
    }
}

#[test]
fn node_cpu_and_memory_are_rendered_with_the_node_label() {
    let out = render_resource_metrics("node-1", Some(12.5), Some(1_048_576), &[]);
    assert!(out.contains("node_cpu_usage_seconds_total{node=\"node-1\"} 12.5"));
    assert!(out.contains("node_memory_working_set_bytes{node=\"node-1\"} 1048576"));
}

#[test]
fn absent_node_stats_produce_no_sample_line_just_the_header() {
    let out = render_resource_metrics("node-1", None, None, &[]);
    assert!(!out.contains("node_cpu_usage_seconds_total{"));
    assert!(!out.contains("node_memory_working_set_bytes{"));
}

#[test]
fn pod_and_container_cpu_are_converted_from_nanoseconds_to_seconds() {
    let pods = vec![pod_with_one_container(Some(2_500_000_000), None)];
    let out = render_resource_metrics("node-1", None, None, &pods);
    assert!(out.contains("pod_cpu_usage_seconds_total{namespace=\"default\",pod=\"web\"} 2.5"));
    assert!(out.contains("container_cpu_usage_seconds_total{namespace=\"default\",pod=\"web\",container=\"app\"} 2.5"));
}

#[test]
fn pod_and_container_memory_are_rendered_in_bytes() {
    let pods = vec![pod_with_one_container(None, Some(4096))];
    let out = render_resource_metrics("node-1", None, None, &pods);
    assert!(out.contains("pod_memory_working_set_bytes{namespace=\"default\",pod=\"web\"} 4096"));
    assert!(out.contains("container_memory_working_set_bytes{namespace=\"default\",pod=\"web\",container=\"app\"} 4096"));
}

#[test]
fn missing_per_pod_stats_omit_that_pod_sample_but_not_the_whole_metric_block() {
    let pods = vec![PodUsage {
        namespace: "default".to_string(),
        name: "unmeasured".to_string(),
        uid: "x".to_string(),
        pod: UsageStats::default(),
        containers: vec![],
        ..Default::default()
    }];
    let out = render_resource_metrics("node-1", None, None, &pods);
    assert!(out.contains("# TYPE pod_cpu_usage_seconds_total counter"));
    assert!(!out.contains("pod=\"unmeasured\""));
}

#[test]
fn label_values_with_special_characters_are_escaped() {
    let pods = vec![pod_with_one_container(Some(1_000_000_000), None)]
        .into_iter()
        .map(|mut p| {
            p.name = "weird\"name\\with\nnewline".to_string();
            p
        })
        .collect::<Vec<_>>();
    let out = render_resource_metrics("node-1", None, None, &pods);
    assert!(out.contains("pod=\"weird\\\"name\\\\with\\nnewline\""));
}

#[test]
fn multiple_pods_and_containers_all_appear() {
    let pods = vec![
        pod_with_one_container(Some(1_000_000_000), Some(1024)),
        {
            let mut p = pod_with_one_container(Some(2_000_000_000), Some(2048));
            p.name = "db".to_string();
            p.containers[0].name = "postgres".to_string();
            p
        },
    ];
    let out = render_resource_metrics("node-1", None, None, &pods);
    assert!(out.contains("pod=\"web\""));
    assert!(out.contains("pod=\"db\""));
    assert!(out.contains("container=\"postgres\""));
}
