fn pod_row(pod: &Value) -> Value {
    let (ready, total) = pod_ready_counts(pod);
    let (status, restarts) = pod_status_and_restarts(pod);
    let ip = pod
        .pointer("/status/podIPs/0/ip")
        .and_then(Value::as_str)
        .unwrap_or("<none>");
    let node = pod
        .pointer("/spec/nodeName")
        .and_then(Value::as_str)
        .unwrap_or("<none>");
    let nominated_node = pod
        .pointer("/status/nominatedNodeName")
        .and_then(Value::as_str)
        .unwrap_or("<none>");
    let readiness_gates = pod_readiness_gates(pod);
    json!({
        "cells": [
            pod.pointer("/metadata/name").cloned().unwrap_or(Value::Null),
            format!("{ready}/{total}"),
            status,
            restarts.to_string(),
            relative_age(pod.pointer("/metadata/creationTimestamp").and_then(Value::as_str)),
            ip,
            node,
            nominated_node,
            readiness_gates,
        ],
        "object": pod,
    })
}

fn pod_ready_counts(pod: &Value) -> (usize, usize) {
    let total = pod
        .pointer("/spec/containers")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let ready = pod
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)
        .map(|statuses| {
            statuses
                .iter()
                .filter(|status| {
                    status
                        .get("ready")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0);
    (ready.min(total), total)
}

fn pod_status_and_restarts(pod: &Value) -> (String, i64) {
    let phase = pod
        .pointer("/status/phase")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut status = pod
        .pointer("/status/reason")
        .and_then(Value::as_str)
        .unwrap_or(phase)
        .to_string();
    let mut restarts = 0;
    let mut initializing = false;
    if let Some(init_statuses) = pod
        .pointer("/status/initContainerStatuses")
        .and_then(Value::as_array)
    {
        let init_total = pod
            .pointer("/spec/initContainers")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        for (index, container) in init_statuses.iter().enumerate() {
            restarts += container
                .get("restartCount")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let terminated = container.pointer("/state/terminated");
            if terminated
                .and_then(|value| value.get("exitCode"))
                .and_then(Value::as_i64)
                == Some(0)
            {
                continue;
            }
            if let Some(reason) = container
                .pointer("/state/waiting/reason")
                .and_then(Value::as_str)
                .filter(|reason| !reason.is_empty() && *reason != "PodInitializing")
            {
                status = format!("Init:{reason}");
            } else if let Some(reason) = terminated
                .and_then(|value| value.get("reason"))
                .and_then(Value::as_str)
                .filter(|reason| !reason.is_empty())
            {
                status = format!("Init:{reason}");
            } else if let Some(exit_code) = terminated
                .and_then(|value| value.get("exitCode"))
                .and_then(Value::as_i64)
            {
                status = format!("Init:ExitCode:{exit_code}");
            } else {
                status = format!("Init:{index}/{init_total}");
            }
            initializing = true;
            break;
        }
    }
    if !initializing {
        if let Some(container_statuses) = pod
            .pointer("/status/containerStatuses")
            .and_then(Value::as_array)
        {
            for container in container_statuses {
                restarts += container
                    .get("restartCount")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                if let Some(reason) = container
                    .pointer("/state/waiting/reason")
                    .and_then(Value::as_str)
                    .filter(|reason| !reason.is_empty())
                {
                    status = reason.to_string();
                } else if let Some(reason) = container
                    .pointer("/state/terminated/reason")
                    .and_then(Value::as_str)
                    .filter(|reason| !reason.is_empty())
                {
                    status = reason.to_string();
                } else if let Some(exit_code) = container
                    .pointer("/state/terminated/exitCode")
                    .and_then(Value::as_i64)
                {
                    status = if exit_code == 0 {
                        "Completed".to_string()
                    } else {
                        format!("ExitCode:{exit_code}")
                    };
                }
            }
        }
        let has_running = pod
            .pointer("/status/containerStatuses")
            .and_then(Value::as_array)
            .is_some_and(|statuses| {
                statuses
                    .iter()
                    .any(|status| status.pointer("/state/running").is_some())
            });
        if has_running && status == "Completed" {
            status = if pod_ready_condition_true(pod) {
                "Running"
            } else {
                "NotReady"
            }
            .to_string();
        }
    }
    if pod
        .get("metadata")
        .and_then(|metadata| metadata.get("deletionTimestamp"))
        .is_some()
        && phase != "Succeeded"
        && phase != "Failed"
    {
        status = "Terminating".to_string();
    }
    if status.is_empty() {
        status = "Unknown".to_string();
    }
    (status, restarts)
}

fn pod_ready_condition_true(pod: &Value) -> bool {
    pod.pointer("/status/conditions")
        .and_then(Value::as_array)
        .is_some_and(|conditions| {
            conditions.iter().any(|condition| {
                condition.get("type").and_then(Value::as_str) == Some("Ready")
                    && condition.get("status").and_then(Value::as_str) == Some("True")
            })
        })
}

fn pod_readiness_gates(pod: &Value) -> String {
    let Some(gates) = pod
        .pointer("/spec/readinessGates")
        .and_then(Value::as_array)
    else {
        return "<none>".to_string();
    };
    if gates.is_empty() {
        return "<none>".to_string();
    }
    let true_count = gates
        .iter()
        .filter(|gate| {
            let gate_type = gate.get("conditionType").and_then(Value::as_str);
            pod.pointer("/status/conditions")
                .and_then(Value::as_array)
                .is_some_and(|conditions| {
                    conditions.iter().any(|condition| {
                        condition.get("type").and_then(Value::as_str) == gate_type
                            && condition.get("status").and_then(Value::as_str) == Some("True")
                    })
                })
        })
        .count();
    format!("{true_count}/{}", gates.len())
}

fn relative_age(timestamp: Option<&str>) -> String {
    let Some(timestamp) = timestamp else {
        return "<unknown>".to_string();
    };
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(timestamp) else {
        return "<unknown>".to_string();
    };
    let seconds = (chrono::Utc::now() - parsed.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else if seconds < 2_592_000 {
        format!("{}d", seconds / 86_400)
    } else if seconds < 31_536_000 {
        format!("{}mo", seconds / 2_592_000)
    } else {
        format!("{}y", seconds / 31_536_000)
    }
}

fn copy_list_metadata(table: &mut Value, object: &Value, is_list: bool) {
    if !is_list {
        return;
    }
    if let Some(list_meta) = object.get("metadata").and_then(Value::as_object) {
        let mut table_meta = serde_json::Map::new();
        for key in ["resourceVersion", "continue", "remainingItemCount"] {
            if let Some(value) = list_meta.get(key) {
                table_meta.insert(key.to_string(), value.clone());
            }
        }
        if !table_meta.is_empty() {
            table["metadata"] = Value::Object(table_meta);
        }
    }
}
