//! Server-side printing (`Table` conversion, finding 9): the shape
//! `kubectl get` actually renders when it negotiates
//! `application/json;as=Table;g=meta.k8s.io;v=v1`
//! (`codec::negotiation::negotiate`'s own `as=Table` parameters).
//!
//! **Wired into `server::listener`**: `GET`/`LIST`'s own real-verb
//! branches check `Accepted::wants_table()` (captured from the request's
//! `Accept` header before the body-reading logic can consume `req`) and
//! run the response through [`convert_to_table`] when set — this was a
//! real, undocumented gap for a while (the converter existed, correctly
//! documented as landed, but nothing in `server/` ever called it) until
//! this wiring closed it.
//!
//! # What this captures, and what it honestly doesn't
//!
//! Real kube-apiserver has two table converters: a hand-written one per
//! type with real business meaning (Pod's `READY`/`STATUS`/`RESTARTS`/...
//! columns, computed from container statuses — `pkg/printers/internalversion`,
//! genuinely bespoke Go per Kind, nothing here derives it from data) and a
//! **generic default converter** every type without one of those falls
//! back to — most visibly, every CRD, since a CRD has no compiled-in Go
//! printer at all. This module faithfully ports that generic converter and
//! the verified common built-in printers; less-common per-type printers remain
//! separate work.
//! (`k8s.io/apiserver/pkg/registry/rest/table.go`'s `defaultTableConvertor`,
//! fetched and read directly, not reconstructed from memory): exactly two
//! columns, `Name` and `Created At`, cells `[metadata.name,
//! metadata.creationTimestamp]` — real upstream does *not* compute a
//! relative age server-side (`kubectl`'s own client-side rendering turns
//! the raw RFC3339 timestamp into the `AGE` column's relative display;
//! the server only ever sends the absolute timestamp). Per-type printers
//! are real, separate, much larger hand-written work, not implied by this
//! module's completeness — resources without a registered printer get the
//! generic table today, matching what a fresh CRD gets in real
//! kube-apiserver, until another specific type earns its own printer.
//!
//! Descriptions on the two column definitions are copied verbatim from the
//! vendored `ObjectMeta.name`/`ObjectMeta.creationTimestamp` property
//! descriptions (`api__v1_openapi.json`) — real text, not invented.

use serde_json::{json, Value};

const NAME_DESCRIPTION: &str = "Name must be unique within a namespace. Is required when creating resources, although some resources may allow a client to request the generation of an appropriate name automatically. Name is primarily intended for creation idempotence and configuration definition. Cannot be updated. More info: https://kubernetes.io/docs/concepts/overview/working-with-objects/names#names";

const CREATED_AT_DESCRIPTION: &str = "CreationTimestamp is a timestamp representing the server time when this object was created. It is not guaranteed to be set in happens-before order across separate operations. Clients may not set this value. It is represented in RFC3339 form and is in UTC.\n\nPopulated by the system. Read-only. Null for lists. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#metadata";

/// Converts a single object or a List-shaped object (anything with an
/// `items` array — a real `PodList`/`ConfigMapList`/... or this crate's
/// own list response shape) into the generic default `Table`. Matches
/// `defaultTableConvertor.ConvertToTable`'s real behavior field-for-field:
/// one row per item (or the one object itself), `object` on each row set
/// to the full item (real upstream's `IncludeObjectPolicy` default is
/// actually `Metadata`-only; clients that need the standard metadata-only
/// representation can negotiate `PartialObjectMetadata` through
/// `codec::partial_metadata`), and `ResourceVersion`/`Continue`/
/// `RemainingItemCount` copied through from a List's own metadata.
pub fn convert_to_table(object: &Value) -> Value {
    let items = list_items(object);
    let rows: Vec<Value> = match &items {
        Some(items) => items.iter().map(|item| row_for(item)).collect(),
        None => vec![row_for(object)],
    };

    let mut table = json!({
        "kind": "Table",
        "apiVersion": "meta.k8s.io/v1",
        "columnDefinitions": [
            {"name": "Name", "type": "string", "format": "name", "description": NAME_DESCRIPTION, "priority": 0},
            {"name": "Created At", "type": "date", "description": CREATED_AT_DESCRIPTION, "priority": 0},
        ],
        "rows": rows,
    });

    if items.is_some() {
        if let Some(list_meta) = object.get("metadata").and_then(Value::as_object) {
            let mut table_meta = serde_json::Map::new();
            for key in ["resourceVersion", "continue", "remainingItemCount"] {
                if let Some(v) = list_meta.get(key) {
                    table_meta.insert(key.to_string(), v.clone());
                }
            }
            if !table_meta.is_empty() {
                table["metadata"] = Value::Object(table_meta);
            }
        }
    }

    table
}

/// Converts a resource using the built-in printer for the small set of
/// resource types this crate has verified. Resources without a printer keep
/// the generic default-table behavior, including all CRD-defined resources.
pub fn convert_to_table_for_resource(group: &str, version: &str, resource: &str, object: &Value) -> Value {
    if group.is_empty() && version == "v1" {
        return match resource {
            "pods" => convert_pod_to_table(object),
            "namespaces" => convert_with_printer(object, namespace_columns(), namespace_row),
            "nodes" => convert_with_printer(object, node_columns(), node_row),
            "services" => convert_with_printer(object, service_columns(), service_row),
            _ => convert_to_table(object),
        };
    }
    if group == "apps" && version == "v1" {
        return match resource {
            "daemonsets" => convert_with_printer(object, daemon_set_columns(), daemon_set_row),
            "deployments" => convert_with_printer(object, deployment_columns(), deployment_row),
            "replicasets" => convert_with_printer(object, replica_set_columns(), replica_set_row),
            "statefulsets" => convert_with_printer(object, stateful_set_columns(), stateful_set_row),
            _ => convert_to_table(object),
        };
    }
    convert_to_table(object)
}

const POD_READY_DESCRIPTION: &str = "The aggregate readiness state of this pod for accepting traffic.";
const POD_STATUS_DESCRIPTION: &str = "The aggregate status of the containers in this pod.";
const POD_RESTARTS_DESCRIPTION: &str = "The number of times the containers in this pod have been restarted and when the last container in this pod has restarted.";

fn convert_pod_to_table(object: &Value) -> Value {
    let items = list_items(object);
    let rows: Vec<Value> = match &items {
        Some(items) => items.iter().map(|item| pod_row(item)).collect(),
        None => vec![pod_row(object)],
    };
    let mut table = json!({
        "kind": "Table",
        "apiVersion": "meta.k8s.io/v1",
        "columnDefinitions": [
            {"name": "Name", "type": "string", "format": "name", "description": NAME_DESCRIPTION, "priority": 0},
            {"name": "Ready", "type": "string", "description": POD_READY_DESCRIPTION, "priority": 0},
            {"name": "Status", "type": "string", "description": POD_STATUS_DESCRIPTION, "priority": 0},
            {"name": "Restarts", "type": "string", "description": POD_RESTARTS_DESCRIPTION, "priority": 0},
            {"name": "Age", "type": "string", "description": CREATED_AT_DESCRIPTION, "priority": 0},
            {"name": "IP", "type": "string", "description": "The pod's IP address.", "priority": 1},
            {"name": "Node", "type": "string", "description": "The node this pod is assigned to.", "priority": 1},
            {"name": "Nominated Node", "type": "string", "description": "The node nominated for this pod.", "priority": 1},
            {"name": "Readiness Gates", "type": "string", "description": "The number of readiness gates satisfied by this pod.", "priority": 1},
        ],
        "rows": rows,
    });
    copy_list_metadata(&mut table, object, items.is_some());
    table
}

fn convert_with_printer(object: &Value, columns: Value, row_for: fn(&Value) -> Value) -> Value {
    let items = list_items(object);
    let rows = match &items {
        Some(items) => items.iter().map(|item| row_for(item)).collect(),
        None => vec![row_for(object)],
    };
    let mut table = json!({
        "kind": "Table",
        "apiVersion": "meta.k8s.io/v1",
        "columnDefinitions": columns,
        "rows": rows,
    });
    copy_list_metadata(&mut table, object, items.is_some());
    table
}

fn deployment_columns() -> Value {
    json!([
        {"name": "Name", "type": "string", "format": "name", "description": NAME_DESCRIPTION, "priority": 0},
        {"name": "Ready", "type": "string", "description": "Number of the pod replicas that are ready.", "priority": 0},
        {"name": "Up-to-date", "type": "integer", "description": "Number of the pod replicas that have been updated.", "priority": 0},
        {"name": "Available", "type": "integer", "description": "Number of the pod replicas that are available.", "priority": 0},
        {"name": "Age", "type": "string", "description": CREATED_AT_DESCRIPTION, "priority": 0},
        {"name": "Containers", "type": "string", "description": "Names of each container in the template.", "priority": 1},
        {"name": "Images", "type": "string", "description": "Images referenced by each container in the template.", "priority": 1},
        {"name": "Selector", "type": "string", "description": "The selector used to match pods.", "priority": 1},
    ])
}

fn replica_set_columns() -> Value {
    json!([
        {"name": "Name", "type": "string", "format": "name", "description": NAME_DESCRIPTION, "priority": 0},
        {"name": "Desired", "type": "integer", "description": "Number of desired pods.", "priority": 0},
        {"name": "Current", "type": "integer", "description": "Number of current pods.", "priority": 0},
        {"name": "Ready", "type": "integer", "description": "Number of ready pods.", "priority": 0},
        {"name": "Age", "type": "string", "description": CREATED_AT_DESCRIPTION, "priority": 0},
        {"name": "Containers", "type": "string", "description": "Names of each container in the template.", "priority": 1},
        {"name": "Images", "type": "string", "description": "Images referenced by each container in the template.", "priority": 1},
        {"name": "Selector", "type": "string", "description": "The selector used to match pods.", "priority": 1},
    ])
}

fn stateful_set_columns() -> Value {
    json!([
        {"name": "Name", "type": "string", "format": "name", "description": NAME_DESCRIPTION, "priority": 0},
        {"name": "Ready", "type": "string", "description": "Number of the pod replicas that are ready.", "priority": 0},
        {"name": "Age", "type": "string", "description": CREATED_AT_DESCRIPTION, "priority": 0},
        {"name": "Containers", "type": "string", "description": "Names of each container in the template.", "priority": 1},
        {"name": "Images", "type": "string", "description": "Images referenced by each container in the template.", "priority": 1},
    ])
}

fn daemon_set_columns() -> Value {
    json!([
        {"name": "Name", "type": "string", "format": "name", "description": NAME_DESCRIPTION, "priority": 0},
        {"name": "Desired", "type": "integer", "description": "Number of nodes that should be running the daemon pod.", "priority": 0},
        {"name": "Current", "type": "integer", "description": "Number of nodes that are running the daemon pod.", "priority": 0},
        {"name": "Ready", "type": "integer", "description": "Number of nodes with a ready daemon pod.", "priority": 0},
        {"name": "Up-to-date", "type": "integer", "description": "Number of nodes with an up-to-date daemon pod.", "priority": 0},
        {"name": "Available", "type": "integer", "description": "Number of nodes with an available daemon pod.", "priority": 0},
        {"name": "Node Selector", "type": "string", "description": "The selector used to choose nodes.", "priority": 0},
        {"name": "Age", "type": "string", "description": CREATED_AT_DESCRIPTION, "priority": 0},
        {"name": "Containers", "type": "string", "description": "Names of each container in the template.", "priority": 1},
        {"name": "Images", "type": "string", "description": "Images referenced by each container in the template.", "priority": 1},
        {"name": "Selector", "type": "string", "description": "The selector used to match pods.", "priority": 1},
    ])
}

fn service_columns() -> Value {
    json!([
        {"name": "Name", "type": "string", "format": "name", "description": NAME_DESCRIPTION, "priority": 0},
        {"name": "Type", "type": "string", "description": "The type of the service.", "priority": 0},
        {"name": "Cluster-IP", "type": "string", "description": "The cluster IP address of the service.", "priority": 0},
        {"name": "External-IP", "type": "string", "description": "External IP addresses of the service.", "priority": 0},
        {"name": "Port(s)", "type": "string", "description": "Service ports.", "priority": 0},
        {"name": "Age", "type": "string", "description": CREATED_AT_DESCRIPTION, "priority": 0},
    ])
}

fn node_columns() -> Value {
    json!([
        {"name": "Name", "type": "string", "format": "name", "description": NAME_DESCRIPTION, "priority": 0},
        {"name": "Status", "type": "string", "description": "The status of the node.", "priority": 0},
        {"name": "Roles", "type": "string", "description": "Roles assigned to the node.", "priority": 0},
        {"name": "Age", "type": "string", "description": CREATED_AT_DESCRIPTION, "priority": 0},
        {"name": "Version", "type": "string", "description": "The kubelet version reported by the node.", "priority": 0},
        {"name": "Internal-IP", "type": "string", "description": "The node's internal IP address.", "priority": 1},
        {"name": "External-IP", "type": "string", "description": "The node's external IP address.", "priority": 1},
        {"name": "OS-Image", "type": "string", "description": "The operating system image reported by the node.", "priority": 1},
        {"name": "Kernel-Version", "type": "string", "description": "The kernel version reported by the node.", "priority": 1},
        {"name": "Container-Runtime", "type": "string", "description": "The container runtime reported by the node.", "priority": 1},
    ])
}

fn namespace_columns() -> Value {
    json!([
        {"name": "Name", "type": "string", "format": "name", "description": NAME_DESCRIPTION, "priority": 0},
        {"name": "Status", "type": "string", "description": "The phase of the namespace.", "priority": 0},
        {"name": "Age", "type": "string", "description": CREATED_AT_DESCRIPTION, "priority": 0},
    ])
}

fn deployment_row(resource: &Value) -> Value {
    let desired = replicas(resource, "/spec/replicas", 1);
    let ready = int_at(resource, "/status/readyReplicas");
    json!({
        "cells": [
            name_cell(resource),
            format!("{ready}/{desired}"),
            int_at(resource, "/status/updatedReplicas"),
            int_at(resource, "/status/availableReplicas"),
            age_cell(resource),
            template_containers(resource),
            template_images(resource),
            selector_cell(resource, "/spec/selector"),
        ],
        "object": resource,
    })
}

fn replica_set_row(resource: &Value) -> Value {
    json!({
        "cells": [
            name_cell(resource),
            replicas(resource, "/spec/replicas", 1),
            int_at(resource, "/status/replicas"),
            int_at(resource, "/status/readyReplicas"),
            age_cell(resource),
            template_containers(resource),
            template_images(resource),
            selector_cell(resource, "/spec/selector"),
        ],
        "object": resource,
    })
}

fn stateful_set_row(resource: &Value) -> Value {
    let desired = replicas(resource, "/spec/replicas", 1);
    let ready = int_at(resource, "/status/readyReplicas");
    json!({
        "cells": [
            name_cell(resource),
            format!("{ready}/{desired}"),
            age_cell(resource),
            template_containers(resource),
            template_images(resource),
        ],
        "object": resource,
    })
}

fn daemon_set_row(resource: &Value) -> Value {
    json!({
        "cells": [
            name_cell(resource),
            int_at(resource, "/status/desiredNumberScheduled"),
            int_at(resource, "/status/currentNumberScheduled"),
            int_at(resource, "/status/numberReady"),
            int_at(resource, "/status/updatedNumberScheduled"),
            int_at(resource, "/status/numberAvailable"),
            map_cell(resource, "/spec/template/spec/nodeSelector"),
            age_cell(resource),
            template_containers(resource),
            template_images(resource),
            selector_cell(resource, "/spec/selector"),
        ],
        "object": resource,
    })
}

fn service_row(resource: &Value) -> Value {
    json!({
        "cells": [
            name_cell(resource),
            string_at(resource, "/spec/type", "ClusterIP"),
            string_at(resource, "/spec/clusterIP", "<none>"),
            service_external_ips(resource),
            service_ports(resource),
            age_cell(resource),
        ],
        "object": resource,
    })
}

fn node_row(resource: &Value) -> Value {
    let mut status = node_ready_status(resource);
    if resource.pointer("/spec/unschedulable").and_then(Value::as_bool).unwrap_or(false) {
        status.push_str(",SchedulingDisabled");
    }
    json!({
        "cells": [
            name_cell(resource),
            status,
            node_roles(resource),
            age_cell(resource),
            string_at(resource, "/status/nodeInfo/kubeletVersion", "<unknown>"),
            node_address(resource, "InternalIP"),
            node_address(resource, "ExternalIP"),
            string_at(resource, "/status/nodeInfo/osImage", "<unknown>"),
            string_at(resource, "/status/nodeInfo/kernelVersion", "<unknown>"),
            string_at(resource, "/status/nodeInfo/containerRuntimeVersion", "<unknown>"),
        ],
        "object": resource,
    })
}

fn namespace_row(resource: &Value) -> Value {
    json!({
        "cells": [
            name_cell(resource),
            string_at(resource, "/status/phase", "<unknown>"),
            age_cell(resource),
        ],
        "object": resource,
    })
}

fn name_cell(resource: &Value) -> Value {
    resource.pointer("/metadata/name").cloned().unwrap_or(Value::Null)
}

fn age_cell(resource: &Value) -> String {
    relative_age(resource.pointer("/metadata/creationTimestamp").and_then(Value::as_str))
}

fn string_at(resource: &Value, path: &str, default: &str) -> String {
    resource.pointer(path).and_then(Value::as_str).filter(|value| !value.is_empty()).unwrap_or(default).to_string()
}

fn int_at(resource: &Value, path: &str) -> i64 {
    resource.pointer(path).and_then(Value::as_i64).unwrap_or(0)
}

fn replicas(resource: &Value, path: &str, default: i64) -> i64 {
    resource.pointer(path).and_then(Value::as_i64).unwrap_or(default)
}

fn template_containers(resource: &Value) -> String {
    let Some(containers) = resource.pointer("/spec/template/spec/containers").and_then(Value::as_array) else {
        return "<none>".to_string();
    };
    let names: Vec<_> = containers.iter().filter_map(|container| container.get("name").and_then(Value::as_str)).collect();
    if names.is_empty() { "<none>".to_string() } else { names.join(",") }
}

fn template_images(resource: &Value) -> String {
    let Some(containers) = resource.pointer("/spec/template/spec/containers").and_then(Value::as_array) else {
        return "<none>".to_string();
    };
    let images: Vec<_> = containers.iter().filter_map(|container| container.get("image").and_then(Value::as_str)).collect();
    if images.is_empty() { "<none>".to_string() } else { images.join(",") }
}

fn selector_cell(resource: &Value, path: &str) -> String {
    let Some(selector) = resource.pointer(path) else {
        return "<none>".to_string();
    };
    let Some(selector) = selector.as_object() else {
        return "<none>".to_string();
    };
    let mut parts = Vec::new();
    if let Some(match_labels) = selector.get("matchLabels") {
        parts.extend(map_parts(match_labels));
    }
    if let Some(expressions) = selector.get("matchExpressions").and_then(Value::as_array) {
        for expression in expressions {
            let key = expression.get("key").and_then(Value::as_str).unwrap_or("");
            let operator = expression.get("operator").and_then(Value::as_str).unwrap_or("");
            let values = expression
                .get("values")
                .and_then(Value::as_array)
                .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(","))
                .unwrap_or_default();
            if !key.is_empty() && !operator.is_empty() {
                parts.push(if values.is_empty() {
                    format!("{key} {operator}")
                } else {
                    format!("{key} {operator} ({values})")
                });
            }
        }
    }
    parts.sort_unstable();
    if parts.is_empty() { "<none>".to_string() } else { parts.join(",") }
}

fn map_cell(resource: &Value, path: &str) -> String {
    let Some(map) = resource.pointer(path) else {
        return "<none>".to_string();
    };
    let parts = map_parts(map);
    if parts.is_empty() { "<none>".to_string() } else { parts.join(",") }
}

fn map_parts(value: &Value) -> Vec<String> {
    let Some(map) = value.as_object() else { return Vec::new() };
    let mut parts: Vec<_> = map
        .iter()
        .map(|(key, value)| format!("{key}={}", value.as_str().unwrap_or_default()))
        .collect();
    parts.sort_unstable();
    parts
}

fn service_external_ips(resource: &Value) -> String {
    let Some(ips) = resource.pointer("/spec/externalIPs").and_then(Value::as_array) else {
        return "<none>".to_string();
    };
    let ips: Vec<_> = ips.iter().filter_map(Value::as_str).collect();
    if ips.is_empty() { "<none>".to_string() } else { ips.join(",") }
}

fn service_ports(resource: &Value) -> String {
    let Some(ports) = resource.pointer("/spec/ports").and_then(Value::as_array) else {
        return "<none>".to_string();
    };
    let mut formatted = Vec::new();
    for port in ports {
        let Some(number) = port.get("port").and_then(Value::as_i64) else { continue };
        let protocol = port.get("protocol").and_then(Value::as_str).unwrap_or("TCP");
        let value = match port.get("nodePort").and_then(Value::as_i64).filter(|node_port| *node_port != 0) {
            Some(node_port) => format!("{number}:{node_port}/{protocol}"),
            None => format!("{number}/{protocol}"),
        };
        formatted.push(value);
    }
    if formatted.is_empty() { "<none>".to_string() } else { formatted.join(",") }
}

fn node_ready_status(resource: &Value) -> String {
    resource
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .and_then(|conditions| conditions.iter().find(|condition| condition.get("type").and_then(Value::as_str) == Some("Ready")))
        .and_then(|condition| condition.get("status").and_then(Value::as_str))
        .map_or_else(
            || "Unknown".to_string(),
            |status| match status {
                "True" => "Ready".to_string(),
                "False" => "NotReady".to_string(),
                _ => "Unknown".to_string(),
            },
        )
}

fn node_roles(resource: &Value) -> String {
    let Some(labels) = resource.pointer("/metadata/labels").and_then(Value::as_object) else {
        return "<none>".to_string();
    };
    let mut roles: Vec<_> = labels
        .iter()
        .filter_map(|(key, value)| {
            key.strip_prefix("node-role.kubernetes.io/")
                .map(|role| if role.is_empty() { "<none>".to_string() } else { role.to_string() })
                .or_else(|| (key == "kubernetes.io/role").then(|| value.as_str().unwrap_or("<none>").to_string()))
        })
        .collect();
    roles.sort_unstable();
    roles.dedup();
    if roles.is_empty() { "<none>".to_string() } else { roles.join(",") }
}

fn node_address(resource: &Value, address_type: &str) -> String {
    resource
        .pointer("/status/addresses")
        .and_then(Value::as_array)
        .and_then(|addresses| addresses.iter().find(|address| address.get("type").and_then(Value::as_str) == Some(address_type)))
        .and_then(|address| address.get("address").and_then(Value::as_str))
        .filter(|address| !address.is_empty())
        .unwrap_or("<none>")
        .to_string()
}

fn pod_row(pod: &Value) -> Value {
    let (ready, total) = pod_ready_counts(pod);
    let (status, restarts) = pod_status_and_restarts(pod);
    let ip = pod.pointer("/status/podIPs/0/ip").and_then(Value::as_str).unwrap_or("<none>");
    let node = pod.pointer("/spec/nodeName").and_then(Value::as_str).unwrap_or("<none>");
    let nominated_node = pod.pointer("/status/nominatedNodeName").and_then(Value::as_str).unwrap_or("<none>");
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
    let total = pod.pointer("/spec/containers").and_then(Value::as_array).map_or(0, Vec::len);
    let ready = pod
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)
        .map(|statuses| statuses.iter().filter(|status| status.get("ready").and_then(Value::as_bool).unwrap_or(false)).count())
        .unwrap_or(0);
    (ready.min(total), total)
}

fn pod_status_and_restarts(pod: &Value) -> (String, i64) {
    let phase = pod.pointer("/status/phase").and_then(Value::as_str).unwrap_or("");
    let mut status = pod.pointer("/status/reason").and_then(Value::as_str).unwrap_or(phase).to_string();
    let mut restarts = 0;
    let mut initializing = false;
    if let Some(init_statuses) = pod.pointer("/status/initContainerStatuses").and_then(Value::as_array) {
        let init_total = pod.pointer("/spec/initContainers").and_then(Value::as_array).map_or(0, Vec::len);
        for (index, container) in init_statuses.iter().enumerate() {
            restarts += container.get("restartCount").and_then(Value::as_i64).unwrap_or(0);
            let terminated = container.pointer("/state/terminated");
            if terminated.and_then(|value| value.get("exitCode")).and_then(Value::as_i64) == Some(0) {
                continue;
            }
            if let Some(reason) = container.pointer("/state/waiting/reason").and_then(Value::as_str).filter(|reason| !reason.is_empty() && *reason != "PodInitializing") {
                status = format!("Init:{reason}");
            } else if let Some(reason) = terminated.and_then(|value| value.get("reason")).and_then(Value::as_str).filter(|reason| !reason.is_empty()) {
                status = format!("Init:{reason}");
            } else if let Some(exit_code) = terminated.and_then(|value| value.get("exitCode")).and_then(Value::as_i64) {
                status = format!("Init:ExitCode:{exit_code}");
            } else {
                status = format!("Init:{index}/{init_total}");
            }
            initializing = true;
            break;
        }
    }
    if !initializing {
        if let Some(container_statuses) = pod.pointer("/status/containerStatuses").and_then(Value::as_array) {
            for container in container_statuses {
                restarts += container.get("restartCount").and_then(Value::as_i64).unwrap_or(0);
                if let Some(reason) = container.pointer("/state/waiting/reason").and_then(Value::as_str).filter(|reason| !reason.is_empty()) {
                    status = reason.to_string();
                } else if let Some(reason) = container.pointer("/state/terminated/reason").and_then(Value::as_str).filter(|reason| !reason.is_empty()) {
                    status = reason.to_string();
                } else if let Some(exit_code) = container.pointer("/state/terminated/exitCode").and_then(Value::as_i64) {
                    status = if exit_code == 0 { "Completed".to_string() } else { format!("ExitCode:{exit_code}") };
                }
            }
        }
        let has_running = pod
            .pointer("/status/containerStatuses")
            .and_then(Value::as_array)
            .is_some_and(|statuses| statuses.iter().any(|status| status.pointer("/state/running").is_some()));
        if has_running && status == "Completed" {
            status = if pod_ready_condition_true(pod) { "Running" } else { "NotReady" }.to_string();
        }
    }
    if pod.get("metadata").and_then(|metadata| metadata.get("deletionTimestamp")).is_some() && phase != "Succeeded" && phase != "Failed" {
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
        .is_some_and(|conditions| conditions.iter().any(|condition| condition.get("type").and_then(Value::as_str) == Some("Ready") && condition.get("status").and_then(Value::as_str) == Some("True")))
}

fn pod_readiness_gates(pod: &Value) -> String {
    let Some(gates) = pod.pointer("/spec/readinessGates").and_then(Value::as_array) else { return "<none>".to_string() };
    if gates.is_empty() {
        return "<none>".to_string();
    }
    let true_count = gates.iter().filter(|gate| {
        let gate_type = gate.get("conditionType").and_then(Value::as_str);
        pod.pointer("/status/conditions").and_then(Value::as_array).is_some_and(|conditions| conditions.iter().any(|condition| condition.get("type").and_then(Value::as_str) == gate_type && condition.get("status").and_then(Value::as_str) == Some("True")))
    }).count();
    format!("{true_count}/{}", gates.len())
}

fn relative_age(timestamp: Option<&str>) -> String {
    let Some(timestamp) = timestamp else { return "<unknown>".to_string() };
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(timestamp) else { return "<unknown>".to_string() };
    let seconds = (chrono::Utc::now() - parsed.with_timezone(&chrono::Utc)).num_seconds().max(0);
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

/// `Some(items)` if `object` is List-shaped (has an `items` array — the
/// only structural signal available without a real typed scheme telling
/// this function "this Kind is a List"), `None` for a single object.
fn list_items(object: &Value) -> Option<Vec<&Value>> {
    object.get("items").and_then(Value::as_array).map(|a| a.iter().collect())
}

fn row_for(item: &Value) -> Value {
    let name = item.pointer("/metadata/name").cloned().unwrap_or(Value::Null);
    let created = item.pointer("/metadata/creationTimestamp").cloned().unwrap_or(Value::Null);
    json!({
        "cells": [name, created],
        "object": item,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_object_becomes_a_one_row_table() {
        let pod = json!({"metadata": {"name": "web-1", "creationTimestamp": "2026-01-01T00:00:00Z"}});
        let table = convert_to_table(&pod);
        assert_eq!(table["kind"], "Table");
        assert_eq!(table["apiVersion"], "meta.k8s.io/v1");
        let rows = table["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["cells"], json!(["web-1", "2026-01-01T00:00:00Z"]));
        assert_eq!(rows[0]["object"], pod);
    }

    #[test]
    fn column_definitions_are_exactly_name_and_created_at() {
        let table = convert_to_table(&json!({"metadata": {"name": "x"}}));
        let cols = table["columnDefinitions"].as_array().unwrap();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0]["name"], "Name");
        assert_eq!(cols[0]["format"], "name");
        assert_eq!(cols[1]["name"], "Created At");
        assert_eq!(cols[1]["type"], "date");
    }

    #[test]
    fn a_list_shaped_object_produces_one_row_per_item() {
        let list = json!({
            "metadata": {"resourceVersion": "42"},
            "items": [
                {"metadata": {"name": "a", "creationTimestamp": "2026-01-01T00:00:00Z"}},
                {"metadata": {"name": "b", "creationTimestamp": "2026-01-02T00:00:00Z"}},
            ],
        });
        let table = convert_to_table(&list);
        let rows = table["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["cells"], json!(["a", "2026-01-01T00:00:00Z"]));
        assert_eq!(rows[1]["cells"], json!(["b", "2026-01-02T00:00:00Z"]));
    }

    #[test]
    fn list_metadata_is_carried_through_to_the_table() {
        let list = json!({
            "metadata": {"resourceVersion": "42", "continue": "abc", "remainingItemCount": 3},
            "items": [],
        });
        let table = convert_to_table(&list);
        assert_eq!(table["metadata"]["resourceVersion"], "42");
        assert_eq!(table["metadata"]["continue"], "abc");
        assert_eq!(table["metadata"]["remainingItemCount"], 3);
    }

    #[test]
    fn an_empty_list_has_no_rows_but_is_still_a_valid_table() {
        let list = json!({"metadata": {}, "items": []});
        let table = convert_to_table(&list);
        assert_eq!(table["rows"], json!([]));
    }

    #[test]
    fn a_single_object_has_no_table_metadata_field_at_all() {
        // Only Lists carry ResourceVersion/Continue/RemainingItemCount —
        // a single object's own metadata isn't the same kind of thing and
        // must not leak into table["metadata"].
        let pod = json!({"metadata": {"name": "web-1", "resourceVersion": "7"}});
        let table = convert_to_table(&pod);
        assert!(table.get("metadata").is_none());
    }

    #[test]
    fn an_item_missing_creation_timestamp_gets_a_null_cell_not_a_panic() {
        let pod = json!({"metadata": {"name": "web-1"}});
        let table = convert_to_table(&pod);
        assert_eq!(table["rows"][0]["cells"], json!(["web-1", Value::Null]));
    }

    #[test]
    fn the_pod_printer_emits_kubectl_status_columns() {
        let pod = json!({
            "metadata": {"name": "web-1"},
            "spec": {"containers": [{"name": "web"}]},
            "status": {
                "phase": "Running",
                "podIPs": [{"ip": "10.42.0.8"}],
                "containerStatuses": [{
                    "name": "web",
                    "ready": true,
                    "restartCount": 2,
                    "state": {"running": {}}
                }],
                "conditions": [{"type": "Ready", "status": "True"}]
            }
        });
        let table = convert_to_table_for_resource("", "v1", "pods", &pod);
        let columns = table["columnDefinitions"].as_array().unwrap();
        assert_eq!(columns.iter().map(|column| column["name"].as_str().unwrap()).collect::<Vec<_>>(), vec![
            "Name", "Ready", "Status", "Restarts", "Age", "IP", "Node", "Nominated Node", "Readiness Gates"
        ]);
        let cells = table["rows"][0]["cells"].as_array().unwrap();
        assert_eq!(&cells[..5], &json!(["web-1", "1/1", "Running", "2", "<unknown>"]).as_array().unwrap()[..]);
        assert_eq!(&cells[5..], &json!(["10.42.0.8", "<none>", "<none>", "<none>"]).as_array().unwrap()[..]);
        assert_eq!(table["rows"][0]["object"], pod);
    }

    #[test]
    fn the_pod_printer_reports_initialization_and_waiting_reasons() {
        let pod = json!({
            "metadata": {"name": "web-1"},
            "spec": {
                "initContainers": [{"name": "setup"}],
                "containers": [{"name": "web"}]
            },
            "status": {
                "phase": "Pending",
                "initContainerStatuses": [{
                    "name": "setup",
                    "restartCount": 1,
                    "state": {"waiting": {"reason": "CrashLoopBackOff"}}
                }]
            }
        });
        let table = convert_to_table_for_resource("", "v1", "pods", &pod);
        assert_eq!(table["rows"][0]["cells"][1], "0/1");
        assert_eq!(table["rows"][0]["cells"][2], "Init:CrashLoopBackOff");
        assert_eq!(table["rows"][0]["cells"][3], "1");
    }

    #[test]
    fn unknown_resources_keep_the_generic_printer() {
        let object = json!({"metadata": {"name": "x"}});
        let table = convert_to_table_for_resource("example.com", "v1", "pods", &object);
        assert_eq!(table["columnDefinitions"].as_array().unwrap().len(), 2);
        let table = convert_to_table_for_resource("", "v1", "configmaps", &object);
        assert_eq!(table["columnDefinitions"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn common_workload_printers_emit_kubectl_rows() {
        let template = json!({
            "spec": {
                "containers": [
                    {"name": "web", "image": "nginx:1.27"},
                    {"name": "sidecar", "image": "busybox:1.36"}
                ]
            }
        });
        let deployment = json!({
            "metadata": {"name": "web"},
            "spec": {
                "replicas": 3,
                "selector": {"matchLabels": {"app": "web"}},
                "template": template.clone()
            },
            "status": {"readyReplicas": 2, "updatedReplicas": 2, "availableReplicas": 2}
        });
        let table = convert_to_table_for_resource("apps", "v1", "deployments", &deployment);
        assert_eq!(table["rows"][0]["cells"], json!(["web", "2/3", 2, 2, "<unknown>", "web,sidecar", "nginx:1.27,busybox:1.36", "app=web"]));

        let replica_set = json!({
            "metadata": {"name": "web-abc"},
            "spec": {"replicas": 3, "selector": {"matchLabels": {"app": "web"}}, "template": template.clone()},
            "status": {"replicas": 2, "readyReplicas": 1}
        });
        let table = convert_to_table_for_resource("apps", "v1", "replicasets", &replica_set);
        assert_eq!(&table["rows"][0]["cells"].as_array().unwrap()[1..4], json!([3, 2, 1]).as_array().unwrap());

        let stateful_set = json!({
            "metadata": {"name": "web"},
            "spec": {"replicas": 2, "template": template.clone()},
            "status": {"readyReplicas": 1}
        });
        let table = convert_to_table_for_resource("apps", "v1", "statefulsets", &stateful_set);
        assert_eq!(&table["rows"][0]["cells"].as_array().unwrap()[..2], json!(["web", "1/2"]).as_array().unwrap());

        let daemon_set = json!({
            "metadata": {"name": "agent"},
            "spec": {"selector": {"matchLabels": {"app": "agent"}}, "template": {
                "spec": {"nodeSelector": {"kubernetes.io/os": "linux"}, "containers": [{"name": "agent", "image": "agent:v1"}]}
            }},
            "status": {
                "desiredNumberScheduled": 3,
                "currentNumberScheduled": 3,
                "numberReady": 2,
                "updatedNumberScheduled": 3,
                "numberAvailable": 2
            }
        });
        let table = convert_to_table_for_resource("apps", "v1", "daemonsets", &daemon_set);
        assert_eq!(&table["rows"][0]["cells"].as_array().unwrap()[1..8], json!([3, 3, 2, 3, 2, "kubernetes.io/os=linux", "<unknown>"]).as_array().unwrap());
    }

    #[test]
    fn service_node_and_namespace_printers_emit_kubectl_rows() {
        let service = json!({
            "metadata": {"name": "api"},
            "spec": {
                "type": "LoadBalancer",
                "clusterIP": "10.43.0.10",
                "externalIPs": ["192.0.2.10"],
                "ports": [
                    {"port": 80, "nodePort": 30080, "protocol": "TCP"},
                    {"port": 443, "protocol": "TCP"}
                ]
            }
        });
        let table = convert_to_table_for_resource("", "v1", "services", &service);
        assert_eq!(table["rows"][0]["cells"], json!(["api", "LoadBalancer", "10.43.0.10", "192.0.2.10", "80:30080/TCP,443/TCP", "<unknown>"]));

        let node = json!({
            "metadata": {
                "name": "worker-1",
                "labels": {"node-role.kubernetes.io/worker": ""}
            },
            "spec": {"unschedulable": true},
            "status": {
                "conditions": [{"type": "Ready", "status": "True"}],
                "addresses": [
                    {"type": "InternalIP", "address": "192.0.2.20"},
                    {"type": "ExternalIP", "address": "198.51.100.20"}
                ],
                "nodeInfo": {
                    "kubeletVersion": "v1.33.0",
                    "osImage": "Debian GNU/Linux",
                    "kernelVersion": "6.1.0",
                    "containerRuntimeVersion": "containerd://2.0.0"
                }
            }
        });
        let table = convert_to_table_for_resource("", "v1", "nodes", &node);
        assert_eq!(table["rows"][0]["cells"], json!(["worker-1", "Ready,SchedulingDisabled", "worker", "<unknown>", "v1.33.0", "192.0.2.20", "198.51.100.20", "Debian GNU/Linux", "6.1.0", "containerd://2.0.0"]));

        let namespace = json!({"metadata": {"name": "apps"}, "status": {"phase": "Active"}});
        let table = convert_to_table_for_resource("", "v1", "namespaces", &namespace);
        assert_eq!(table["rows"][0]["cells"], json!(["apps", "Active", "<unknown>"]));
    }
}
