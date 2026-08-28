//! JSON conversion for API versions that share one storage key.
//!
//! The generated `k8s-openapi` types intentionally expose only one Kubernetes
//! release API surface, so they cannot be used as a second internal type
//! universe for older served versions. Conversion therefore happens at the
//! JSON boundary. The common case is a compatible object shape: its GVK is
//! rewritten to the requested version. Autoscaling HPA is the first version
//! pair with a real field-shape conversion, because v1's single CPU target
//! became v2's metrics list.

use serde_json::{json, Map, Value};

/// Convert a stored object to the version requested by the client.
///
/// This preserves the object in the common compatible-shape case and applies
/// the real HPA v1/v2 CPU-target mapping where the API versions differ. It is
/// deliberately a pure function so each REST and watch response can use the
/// same conversion path without another storage read.
pub fn to_version(group: &str, version: &str, kind: &str, mut object: Value) -> Value {
    let Some(map) = object.as_object_mut() else {
        return object;
    };
    let requested_api_version = if group.is_empty() {
        version.to_string()
    } else {
        format!("{group}/{version}")
    };
    let source_api_version = map.get("apiVersion").and_then(Value::as_str).map(str::to_string);
    if let Some(source_api_version) = source_api_version.as_deref() {
        let source_version = source_api_version.rsplit_once('/').map_or(source_api_version, |(_, version)| version);
        if group == "autoscaling" && kind == "HorizontalPodAutoscaler" {
            match (source_version, version) {
                ("v1", "v2") => hpa_v1_to_v2(map),
                ("v2", "v1") => hpa_v2_to_v1(map),
                _ => {}
            }
        }
    }
    map.insert("apiVersion".to_string(), Value::String(requested_api_version));
    map.insert("kind".to_string(), Value::String(kind.to_string()));
    object
}

fn hpa_v1_to_v2(object: &mut Map<String, Value>) {
    let Some(spec) = object.get_mut("spec").and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(target) = spec.remove("targetCPUUtilizationPercentage") {
        spec.insert(
            "metrics".to_string(),
            json!([{
                "type": "Resource",
                "resource": {
                    "name": "cpu",
                    "target": {
                        "type": "Utilization",
                        "averageUtilization": target,
                    },
                },
            }]),
        );
    }

    let Some(status) = object.get_mut("status").and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(current) = status.remove("currentCPUUtilizationPercentage") {
        status.insert(
            "currentMetrics".to_string(),
            json!([{
                "type": "Resource",
                "resource": {
                    "name": "cpu",
                    "current": {
                        "averageUtilization": current,
                    },
                },
            }]),
        );
    }
}

fn hpa_v2_to_v1(object: &mut Map<String, Value>) {
    let Some(spec) = object.get_mut("spec").and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(target) = take_cpu_average_utilization(spec.get("metrics")) {
        spec.insert("targetCPUUtilizationPercentage".to_string(), target);
    }
    // These fields have no v1 representation. A v1 client must not receive
    // an object containing v2-only fields that it cannot round-trip.
    spec.remove("metrics");
    spec.remove("behavior");

    let Some(status) = object.get_mut("status").and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(current) = take_cpu_average_utilization(status.get("currentMetrics")) {
        status.insert("currentCPUUtilizationPercentage".to_string(), current);
    }
    status.remove("currentMetrics");
}

fn take_cpu_average_utilization(value: Option<&Value>) -> Option<Value> {
    value?.as_array()?.iter().find_map(|metric| {
        let metric = metric.as_object()?;
        if metric.get("type").and_then(Value::as_str) != Some("Resource") {
            return None;
        }
        let resource = metric.get("resource")?.as_object()?;
        if resource.get("name").and_then(Value::as_str) != Some("cpu") {
            return None;
        }
        let target = resource.get("target").or_else(|| resource.get("current"))?.as_object()?;
        target.get("averageUtilization").cloned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatible_versions_receive_the_requested_gvk() {
        let object = to_version("coordination.k8s.io", "v1beta1", "Lease", json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": {"name": "leader"},
        }));
        assert_eq!(object["apiVersion"], "coordination.k8s.io/v1beta1");
        assert_eq!(object["kind"], "Lease");
    }

    #[test]
    fn hpa_v1_cpu_target_converts_to_the_v2_resource_metric() {
        let object = to_version("autoscaling", "v2", "HorizontalPodAutoscaler", json!({
            "apiVersion": "autoscaling/v1",
            "kind": "HorizontalPodAutoscaler",
            "spec": {"targetCPUUtilizationPercentage": 70},
            "status": {"currentCPUUtilizationPercentage": 55},
        }));
        assert_eq!(object["spec"]["metrics"][0]["resource"]["name"], "cpu");
        assert_eq!(object["spec"]["metrics"][0]["resource"]["target"]["averageUtilization"], 70);
        assert_eq!(object["status"]["currentMetrics"][0]["resource"]["current"]["averageUtilization"], 55);
        assert!(object["spec"].get("targetCPUUtilizationPercentage").is_none());
    }

    #[test]
    fn hpa_v2_cpu_metric_converts_back_to_the_v1_target() {
        let object = to_version("autoscaling", "v1", "HorizontalPodAutoscaler", json!({
            "apiVersion": "autoscaling/v2",
            "kind": "HorizontalPodAutoscaler",
            "spec": {"metrics": [{"type": "Resource", "resource": {"name": "cpu", "target": {"type": "Utilization", "averageUtilization": 80}}}], "behavior": {}},
            "status": {"currentMetrics": [{"type": "Resource", "resource": {"name": "cpu", "current": {"averageUtilization": 60}}}]},
        }));
        assert_eq!(object["spec"]["targetCPUUtilizationPercentage"], 80);
        assert_eq!(object["status"]["currentCPUUtilizationPercentage"], 60);
        assert!(object["spec"].get("metrics").is_none());
        assert!(object["status"].get("currentMetrics").is_none());
    }
}
