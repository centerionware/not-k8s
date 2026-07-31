use super::*;
use crate::runtime::ContainerUsage;

fn usage_with(cpu_cores: Option<u64>, mem_working_set: Option<u64>) -> UsageStats {
    UsageStats { cpu_usage_nano_cores: cpu_cores, memory_working_set_bytes: mem_working_set, ..Default::default() }
}

#[test]
fn pod_ref_carries_through_namespace_name_and_uid() {
    let usage = PodUsage {
        namespace: "default".to_string(),
        name: "web".to_string(),
        uid: "abc-123".to_string(),
        pod: UsageStats::default(),
        containers: vec![],
        ..Default::default()
    };
    let stats = build_pod_stats(&usage);
    assert_eq!(stats.pod_ref.namespace, "default");
    assert_eq!(stats.pod_ref.name, "web");
    assert_eq!(stats.pod_ref.uid, "abc-123");
}

#[test]
fn container_stats_are_mapped_by_name() {
    let usage = PodUsage {
        namespace: "default".to_string(),
        name: "web".to_string(),
        uid: "abc-123".to_string(),
        pod: UsageStats::default(),
        containers: vec![
            ContainerUsage { name: "app".to_string(), stats: usage_with(Some(50_000_000), Some(1_048_576)) },
            ContainerUsage { name: "sidecar".to_string(), stats: usage_with(None, None) },
        ],
        ..Default::default()
    };
    let stats = build_pod_stats(&usage);
    assert_eq!(stats.containers.len(), 2);
    assert_eq!(stats.containers[0].name, "app");
    assert_eq!(stats.containers[0].cpu.as_ref().unwrap().usage_nano_cores, Some(50_000_000));
    assert_eq!(stats.containers[0].memory.as_ref().unwrap().working_set_bytes, Some(1_048_576));
    assert_eq!(stats.containers[1].name, "sidecar");
}

#[test]
fn all_empty_usage_omits_cpu_and_memory_blocks_entirely() {
    let usage = PodUsage {
        namespace: "default".to_string(),
        name: "web".to_string(),
        uid: "abc-123".to_string(),
        pod: UsageStats::default(),
        containers: vec![ContainerUsage { name: "app".to_string(), stats: UsageStats::default() }],
        ..Default::default()
    };
    let stats = build_pod_stats(&usage);
    assert!(stats.cpu.is_none());
    assert!(stats.memory.is_none());
    assert!(stats.containers[0].cpu.is_none());
    assert!(stats.containers[0].memory.is_none());
}

#[test]
fn partial_memory_stats_still_produce_a_memory_block() {
    // Only working_set_bytes known — the block should still appear with
    // just that field, not be treated as "empty."
    let stats_struct = UsageStats { memory_working_set_bytes: Some(2048), ..Default::default() };
    assert!(memory_stats(&stats_struct).is_some());
    assert_eq!(memory_stats(&stats_struct).unwrap().working_set_bytes, Some(2048));
}

#[test]
fn serializes_to_the_expected_camel_case_json_shape() {
    let usage = PodUsage {
        namespace: "default".to_string(),
        name: "web".to_string(),
        uid: "abc-123".to_string(),
        pod: usage_with(Some(1_000_000), Some(4096)),
        containers: vec![],
        ..Default::default()
    };
    let stats = build_pod_stats(&usage);
    let json = serde_json::to_value(&stats).unwrap();
    assert_eq!(json["podRef"]["name"], "web");
    assert_eq!(json["cpu"]["usageNanoCores"], 1_000_000);
    assert_eq!(json["memory"]["workingSetBytes"], 4096);
}

#[test]
fn node_stats_include_the_node_name() {
    let stats = node_stats("my-node");
    assert_eq!(stats.node_name, "my-node");
}
