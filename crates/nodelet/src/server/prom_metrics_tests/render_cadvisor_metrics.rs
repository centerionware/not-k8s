use super::*;
use crate::runtime::{ContainerUsage, UsageStats};

fn pod_with_one_container(stats: UsageStats) -> PodUsage {
    PodUsage {
        namespace: "default".to_string(),
        name: "web".to_string(),
        uid: "abc-123".to_string(),
        pod: UsageStats::default(),
        containers: vec![ContainerUsage { name: "app".to_string(), stats }],
    }
}

#[test]
fn includes_help_and_type_lines_for_every_metric() {
    let out = render_cadvisor_metrics(&[]);
    for name in [
        "container_cpu_usage_seconds_total",
        "container_memory_usage_bytes",
        "container_memory_working_set_bytes",
        "container_memory_rss",
    ] {
        assert!(out.contains(&format!("# HELP {name} ")), "missing HELP for {name}");
        assert!(out.contains(&format!("# TYPE {name} ")), "missing TYPE for {name}");
    }
}

#[test]
fn container_cpu_is_converted_from_nanoseconds_to_seconds() {
    let pods = vec![pod_with_one_container(UsageStats {
        cpu_usage_core_nano_seconds: Some(3_000_000_000),
        ..Default::default()
    })];
    let out = render_cadvisor_metrics(&pods);
    assert!(out.contains("container_cpu_usage_seconds_total{namespace=\"default\",pod=\"web\",container=\"app\"} 3"));
}

#[test]
fn distinguishes_usage_working_set_and_rss() {
    let pods = vec![pod_with_one_container(UsageStats {
        memory_usage_bytes: Some(3000),
        memory_working_set_bytes: Some(2000),
        memory_rss_bytes: Some(1000),
        ..Default::default()
    })];
    let out = render_cadvisor_metrics(&pods);
    assert!(out.contains("container_memory_usage_bytes{namespace=\"default\",pod=\"web\",container=\"app\"} 3000"));
    assert!(out.contains("container_memory_working_set_bytes{namespace=\"default\",pod=\"web\",container=\"app\"} 2000"));
    assert!(out.contains("container_memory_rss{namespace=\"default\",pod=\"web\",container=\"app\"} 1000"));
}

#[test]
fn all_empty_usage_produces_headers_with_no_sample_lines() {
    let pods = vec![pod_with_one_container(UsageStats::default())];
    let out = render_cadvisor_metrics(&pods);
    assert!(!out.contains("container=\"app\""));
}

#[test]
fn empty_pod_list_still_renders_headers() {
    let out = render_cadvisor_metrics(&[]);
    assert!(out.contains("# HELP container_cpu_usage_seconds_total"));
}
