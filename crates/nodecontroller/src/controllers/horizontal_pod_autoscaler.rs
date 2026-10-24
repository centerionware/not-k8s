//! Autoscaling/v2 HorizontalPodAutoscaler reconciliation.
//!
//! Reads Pod and metrics data through the shared controller identity, then
//! writes the Scale subresource and HPA status through the upstream
//! horizontal-pod-autoscaler ServiceAccount identity. Reconciliation is
//! keyed by HPA and periodically requeued because metric values change
//! without a Kubernetes object watch event.

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use k8s_openapi::api::autoscaling::v2::{
    ContainerResourceMetricStatus, ExternalMetricStatus, HorizontalPodAutoscaler,
    HorizontalPodAutoscalerCondition, MetricIdentifier, MetricStatus, MetricValueStatus,
    ObjectMetricStatus, PodsMetricStatus, ResourceMetricStatus,
};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DynamicObject, ListParams, Patch, PatchParams};
use kube::discovery::{ApiCapabilities, ApiResource, Discovery};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;

const SYNC_PERIOD: Duration = Duration::from_secs(15);
const DEFAULT_CPU_TARGET_PERCENT: i32 = 80;
const DEFAULT_TOLERANCE: f64 = 0.1;

#[derive(Clone)]
struct ScaleResource {
    resource: ApiResource,
}

#[derive(Debug, Deserialize)]
struct PodMetricsList {
    #[serde(default)]
    items: Vec<PodMetrics>,
}

#[derive(Debug, Deserialize)]
struct PodMetrics {
    metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
    #[serde(default)]
    containers: Vec<ContainerMetrics>,
}

#[derive(Debug, Deserialize)]
struct ContainerMetrics {
    name: String,
    #[serde(default)]
    usage: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct ExternalMetricList {
    #[serde(default)]
    items: Vec<ExternalMetricValue>,
}

#[derive(Debug, Deserialize)]
struct ExternalMetricValue {
    #[serde(default)]
    value: String,
}

#[derive(Debug, Deserialize)]
struct CustomMetricList {
    #[serde(default)]
    items: Vec<ExternalMetricValue>,
}

fn quantity_value(quantity: &str) -> Option<f64> {
    let (number, multiplier) = [
        ("Ei", 1.152921504606847e18),
        ("Pi", 1.125899906842624e15),
        ("Ti", 1.099511627776e12),
        ("Gi", 1.073741824e9),
        ("Mi", 1.048576e6),
        ("Ki", 1024.0),
        ("E", 1e18),
        ("P", 1e15),
        ("T", 1e12),
        ("G", 1e9),
        ("M", 1e6),
        ("k", 1e3),
        ("m", 1e-3),
        ("u", 1e-6),
        ("n", 1e-9),
    ]
    .into_iter()
    .find_map(|(suffix, multiplier)| {
        quantity
            .strip_suffix(suffix)
            .map(|number| (number, multiplier))
    })
    .unwrap_or((quantity, 1.0));
    let value = number.parse::<f64>().ok()? * multiplier;
    value.is_finite().then_some(value)
}

fn metric_selector(
    selector: Option<&k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector>,
) -> Result<String> {
    selector
        .cloned()
        .map(kube::core::Selector::try_from)
        .transpose()
        .context("invalid HPA external metric selector")
        .map(|selector| selector.map_or_else(String::new, |selector| selector.to_string()))
}

fn escape_query_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            escaped.push(char::from(byte));
        } else {
            escaped.push_str(&format!("%{byte:02X}"));
        }
    }
    escaped
}

async fn custom_metric_values(
    client: &Client,
    namespace: &str,
    path: &str,
    metric: &MetricIdentifier,
    pod_selector: Option<&str>,
) -> Result<Vec<f64>> {
    let selector = metric_selector(metric.selector.as_ref())?;
    let mut query = Vec::new();
    if !selector.is_empty() {
        query.push(format!(
            "metricLabelSelector={}",
            escape_query_value(&selector)
        ));
    }
    if let Some(pod_selector) = pod_selector.filter(|selector| !selector.is_empty()) {
        query.push(format!(
            "labelSelector={}",
            escape_query_value(pod_selector)
        ));
    }
    let query = if query.is_empty() {
        String::new()
    } else {
        format!("?{}", query.join("&"))
    };
    let uri = format!(
        "/apis/custom.metrics.k8s.io/v1beta1/namespaces/{namespace}/{path}/{}{}",
        metric.name, query
    );
    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(uri)
        .body(Vec::new())?;
    let response: serde_json::Value = client
        .request(request)
        .await
        .with_context(|| format!("reading custom metric {}", metric.name))?;
    let values: CustomMetricList =
        serde_json::from_value(response).context("decoding custom.metrics.k8s.io metric list")?;
    let values = values
        .items
        .iter()
        .map(|item| {
            quantity_value(&item.value).ok_or_else(|| {
                anyhow!(
                    "custom metric {} returned invalid quantity {:?}",
                    metric.name,
                    item.value
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if values.is_empty() {
        anyhow::bail!("custom metric {} returned no values", metric.name);
    }
    Ok(values)
}

async fn custom_object_metric_value(
    client: &Client,
    namespace: &str,
    path: &str,
    metric: &MetricIdentifier,
) -> Result<f64> {
    let selector = metric_selector(metric.selector.as_ref())?;
    let query = if selector.is_empty() {
        String::new()
    } else {
        format!("?metricLabelSelector={}", escape_query_value(&selector))
    };
    let uri = format!(
        "/apis/custom.metrics.k8s.io/v1beta1/namespaces/{namespace}/{path}/{}{}",
        metric.name, query
    );
    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(uri)
        .body(Vec::new())?;
    let response: serde_json::Value = client
        .request(request)
        .await
        .with_context(|| format!("reading custom object metric {}", metric.name))?;
    let value: ExternalMetricValue =
        serde_json::from_value(response).context("decoding custom.metrics.k8s.io object metric")?;
    quantity_value(&value.value).ok_or_else(|| {
        anyhow!(
            "custom object metric {} returned invalid quantity {:?}",
            metric.name,
            value.value
        )
    })
}

async fn object_metric_plural(client: &Client, api_version: &str, kind: &str) -> Result<String> {
    let discovery = Discovery::new(client.clone())
        .run_aggregated()
        .await
        .context("discovering object resource for custom metric")?;
    let plural = discovery
        .groups()
        .flat_map(|group| {
            group
                .recommended_resources()
                .into_iter()
                .map(|(resource, _)| resource)
        })
        .find(|resource| resource.api_version == api_version && resource.kind == kind)
        .map(|resource| resource.plural.clone());
    plural.ok_or_else(|| anyhow!("custom metric object {api_version}/{kind} is not discoverable"))
}

async fn external_metric_value(
    client: &Client,
    namespace: &str,
    metric: &MetricIdentifier,
) -> Result<f64> {
    let resource = ApiResource::from_gvk_with_plural(
        &kube::core::gvk::GroupVersionKind::gvk(
            "external.metrics.k8s.io",
            "v1beta1",
            "ExternalMetricValue",
        ),
        &metric.name,
    );
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), namespace, &resource);
    let selector = metric_selector(metric.selector.as_ref())?;
    let params = if selector.is_empty() {
        ListParams::default()
    } else {
        ListParams::default().labels(&selector)
    };
    let response = api
        .list(&params)
        .await
        .with_context(|| format!("reading external metric {}", metric.name))?;
    let values: ExternalMetricList = serde_json::from_value(serde_json::to_value(response)?)
        .context("decoding external.metrics.k8s.io response")?;
    if values.items.is_empty() {
        anyhow::bail!("external metric {} returned no values", metric.name);
    }
    values.items.iter().try_fold(0.0, |sum, item| {
        let value = quantity_value(&item.value).ok_or_else(|| {
            anyhow!(
                "external metric {} returned invalid quantity {:?}",
                metric.name,
                item.value
            )
        })?;
        Ok(sum + value)
    })
}

fn metric_recommendation(
    current: i32,
    value: f64,
    target: f64,
    average: bool,
    tolerance_up: f64,
    tolerance_down: f64,
) -> Result<i32> {
    if !target.is_finite() || target <= 0.0 || !value.is_finite() {
        anyhow::bail!("HPA metric value must be finite and its target must be finite and positive");
    }
    let metric_ratio = if average && current > 0 {
        value / (target * f64::from(current))
    } else {
        value / target
    };
    let tolerance = if metric_ratio > 1.0 {
        tolerance_up
    } else {
        tolerance_down
    };
    let desired = if current > 0 && (metric_ratio - 1.0).abs() <= tolerance {
        f64::from(current)
    } else if average {
        (value / target).ceil()
    } else if current == 0 {
        (value / target).ceil()
    } else {
        (f64::from(current) * value / target).ceil()
    };
    Ok(desired.clamp(0.0, f64::from(i32::MAX)) as i32)
}

struct MetricRecommendation {
    desired: i32,
    status: MetricStatus,
}

async fn evaluate_metric(
    metric: &k8s_openapi::api::autoscaling::v2::MetricSpec,
    hpa: &HorizontalPodAutoscaler,
    selector: &str,
    current: i32,
    base_client: &Client,
    tolerances: (f64, f64),
) -> Result<MetricRecommendation> {
    match metric.type_.as_str() {
        "External" => {
            let source = metric
                .external
                .as_ref()
                .ok_or_else(|| anyhow!("External metric source is missing"))?;
            let value = external_metric_value(
                base_client,
                &hpa.namespace().unwrap_or_default(),
                &source.metric,
            )
            .await?;
            let target = &source.target;
            let (target_value, average) = match target.type_.as_str() {
                "Value" => (target.value.as_ref(), false),
                "AverageValue" => (target.average_value.as_ref(), true),
                other => anyhow::bail!("unsupported External metric target type {other}"),
            };
            let target_value = target_value
                .and_then(|quantity| quantity_value(&quantity.0))
                .ok_or_else(|| anyhow!("External metric target is missing a valid quantity"))?;
            let desired = metric_recommendation(
                current,
                value,
                target_value,
                average,
                tolerances.0,
                tolerances.1,
            )?;
            let current_value =
                k8s_openapi::apimachinery::pkg::api::resource::Quantity(value.to_string());
            Ok(MetricRecommendation {
                desired,
                status: MetricStatus {
                    type_: "External".to_string(),
                    external: Some(ExternalMetricStatus {
                        metric: source.metric.clone(),
                        current: MetricValueStatus {
                            value: Some(current_value),
                            ..Default::default()
                        },
                    }),
                    ..Default::default()
                },
            })
        }
        "Resource" => {
            let source = metric
                .resource
                .as_ref()
                .ok_or_else(|| anyhow!("Resource metric source is missing"))?;
            let (total_usage, total_request, pod_count) =
                current_resource_values(hpa, selector, &source.name, None, base_client).await?;
            let current_status =
                resource_metric_value(&source.target, total_usage, total_request, pod_count)?;
            let desired = match source.target.type_.as_str() {
                "Utilization" => {
                    let target = source.target.average_utilization.ok_or_else(|| {
                        anyhow!("Resource Utilization target requires averageUtilization")
                    })?;
                    if target <= 0 {
                        anyhow::bail!("Resource averageUtilization must be positive");
                    }
                    let utilization = ((total_usage / total_request) * 100.0).floor() as i32;
                    desired_replicas(
                        current,
                        utilization,
                        target,
                        0,
                        i32::MAX,
                        tolerances.0,
                        tolerances.1,
                    )
                    .0
                }
                "AverageValue" => metric_recommendation(
                    current,
                    total_usage,
                    source
                        .target
                        .average_value
                        .as_ref()
                        .and_then(|value| quantity_value(&value.0))
                        .ok_or_else(|| anyhow!("Resource AverageValue target is missing"))?,
                    true,
                    tolerances.0,
                    tolerances.1,
                )?,
                other => anyhow::bail!("unsupported Resource metric target type {other}"),
            };
            Ok(MetricRecommendation {
                desired,
                status: MetricStatus {
                    type_: "Resource".to_string(),
                    resource: Some(ResourceMetricStatus {
                        name: source.name.clone(),
                        current: current_status,
                    }),
                    ..Default::default()
                },
            })
        }
        "ContainerResource" => {
            let source = metric
                .container_resource
                .as_ref()
                .ok_or_else(|| anyhow!("ContainerResource metric source is missing"))?;
            let (total_usage, total_request, pod_count) = current_resource_values(
                hpa,
                selector,
                &source.name,
                Some(&source.container),
                base_client,
            )
            .await?;
            let current_status =
                resource_metric_value(&source.target, total_usage, total_request, pod_count)?;
            let desired = match source.target.type_.as_str() {
                "Utilization" => {
                    let target = source.target.average_utilization.ok_or_else(|| {
                        anyhow!("ContainerResource Utilization target requires averageUtilization")
                    })?;
                    if target <= 0 {
                        anyhow::bail!("ContainerResource averageUtilization must be positive");
                    }
                    let utilization = ((total_usage / total_request) * 100.0).floor() as i32;
                    desired_replicas(
                        current,
                        utilization,
                        target,
                        0,
                        i32::MAX,
                        tolerances.0,
                        tolerances.1,
                    )
                    .0
                }
                "AverageValue" => metric_recommendation(
                    current,
                    total_usage,
                    source
                        .target
                        .average_value
                        .as_ref()
                        .and_then(|value| quantity_value(&value.0))
                        .ok_or_else(|| {
                            anyhow!("ContainerResource AverageValue target is missing")
                        })?,
                    true,
                    tolerances.0,
                    tolerances.1,
                )?,
                other => anyhow::bail!("unsupported ContainerResource target type {other}"),
            };
            Ok(MetricRecommendation {
                desired,
                status: MetricStatus {
                    type_: "ContainerResource".to_string(),
                    container_resource: Some(ContainerResourceMetricStatus {
                        container: source.container.clone(),
                        name: source.name.clone(),
                        current: current_status,
                    }),
                    ..Default::default()
                },
            })
        }
        "Pods" => {
            let source = metric
                .pods
                .as_ref()
                .ok_or_else(|| anyhow!("Pods metric source is missing"))?;
            if selector.is_empty() {
                anyhow::bail!("HPA target Scale has no Pod selector for its Pods metric");
            }
            let values = custom_metric_values(
                base_client,
                &hpa.namespace().unwrap_or_default(),
                &format!("pods/*"),
                &source.metric,
                Some(selector),
            )
            .await?;
            let total = values.iter().sum::<f64>();
            let target = source
                .target
                .average_value
                .as_ref()
                .and_then(|value| quantity_value(&value.0))
                .ok_or_else(|| anyhow!("Pods metric requires a valid AverageValue target"))?;
            let desired =
                metric_recommendation(current, total, target, true, tolerances.0, tolerances.1)?;
            Ok(MetricRecommendation {
                desired,
                status: MetricStatus {
                    type_: "Pods".to_string(),
                    pods: Some(PodsMetricStatus {
                        metric: source.metric.clone(),
                        current: MetricValueStatus {
                            average_value: Some(
                                k8s_openapi::apimachinery::pkg::api::resource::Quantity(
                                    (total / values.len() as f64).to_string(),
                                ),
                            ),
                            ..Default::default()
                        },
                    }),
                    ..Default::default()
                },
            })
        }
        "Object" => {
            let source = metric
                .object
                .as_ref()
                .ok_or_else(|| anyhow!("Object metric source is missing"))?;
            let object = &source.described_object;
            let api_version = object.api_version.as_deref().unwrap_or("v1");
            let plural = object_metric_plural(base_client, api_version, &object.kind).await?;
            let value = custom_object_metric_value(
                base_client,
                &hpa.namespace().unwrap_or_default(),
                &format!("{plural}/{}", object.name),
                &source.metric,
            )
            .await?;
            let target = &source.target;
            let (target_value, average) = match target.type_.as_str() {
                "Value" => (target.value.as_ref(), false),
                "AverageValue" => (target.average_value.as_ref(), true),
                other => anyhow::bail!("unsupported Object metric target type {other}"),
            };
            let target_value = target_value
                .and_then(|quantity| quantity_value(&quantity.0))
                .ok_or_else(|| anyhow!("Object metric target is missing a valid quantity"))?;
            let desired = metric_recommendation(
                current,
                value,
                target_value,
                average,
                tolerances.0,
                tolerances.1,
            )?;
            Ok(MetricRecommendation {
                desired,
                status: MetricStatus {
                    type_: "Object".to_string(),
                    object: Some(ObjectMetricStatus {
                        described_object: object.clone(),
                        metric: source.metric.clone(),
                        current: MetricValueStatus {
                            value: Some(k8s_openapi::apimachinery::pkg::api::resource::Quantity(
                                value.to_string(),
                            )),
                            ..Default::default()
                        },
                    }),
                    ..Default::default()
                },
            })
        }
        other => anyhow::bail!("HPA metric type {other} is not supported yet"),
    }
}

/// HPA's integer replica recommendation, including the default 10% deadband.
/// Inputs are an aggregated metric value and an API Scale's current desired
/// replica count; keeping this pure makes scaling arithmetic deterministic.
fn desired_replicas(
    current: i32,
    utilization_percent: i32,
    target_percent: i32,
    min_replicas: i32,
    max_replicas: i32,
    tolerance_up: f64,
    tolerance_down: f64,
) -> (i32, bool) {
    let current = current.max(0);
    let min_replicas = min_replicas.max(0);
    let max_replicas = max_replicas.max(min_replicas);
    if target_percent <= 0 {
        let desired = current.clamp(min_replicas, max_replicas);
        return (desired, desired != current);
    }
    let ratio = f64::from(utilization_percent) / f64::from(target_percent);
    let tolerance = if ratio > 1.0 {
        tolerance_up
    } else {
        tolerance_down
    };
    if (ratio - 1.0).abs() <= tolerance {
        let desired = current.clamp(min_replicas, max_replicas);
        return (desired, desired != current);
    }
    let computed = ((current as i64 * utilization_percent.max(0) as i64) + target_percent as i64
        - 1)
        / target_percent as i64;
    let computed = computed.clamp(0, i32::MAX as i64) as i32;
    let desired = computed.clamp(min_replicas, max_replicas);
    (desired, desired != computed)
}

fn stabilized_recommendation(
    key: &str,
    current: i32,
    recommendation: i32,
    window: Duration,
    now: Instant,
    history: &mut HashMap<String, Vec<(Instant, i32)>>,
) -> i32 {
    let first_recommendation = !history.contains_key(key);
    let samples = history.entry(key.to_string()).or_default();
    samples.retain(|(recorded_at, _)| now.saturating_duration_since(*recorded_at) < window);
    // Seed scale-down stabilization with the current replica count. This
    // matches the HPA controller's initial recommendation and prevents the
    // first low metric sample from immediately dropping a workload to zero.
    if first_recommendation && recommendation < current {
        samples.push((now, current));
    }
    samples.push((now, recommendation));
    if recommendation < current {
        samples
            .iter()
            .map(|(_, replicas)| *replicas)
            .max()
            .unwrap_or(recommendation)
    } else if recommendation > current {
        samples
            .iter()
            .map(|(_, replicas)| *replicas)
            .min()
            .unwrap_or(recommendation)
    } else {
        recommendation
    }
}

#[derive(Clone, Copy)]
struct ScaleEvent {
    at: Instant,
    from: i32,
    to: i32,
}

fn apply_rate_policy(
    current: i32,
    desired: i32,
    scale_up: bool,
    rules: Option<&k8s_openapi::api::autoscaling::v2::HPAScalingRules>,
    now: Instant,
    history: &mut HashMap<String, Vec<ScaleEvent>>,
    key: &str,
) -> (i32, bool) {
    if desired == current {
        return (desired, false);
    }
    if rules.and_then(|rules| rules.select_policy.as_deref()) == Some("Disabled") {
        return (current, true);
    }
    let configured = rules.and_then(|rules| rules.policies.as_ref());
    let defaults = if scale_up {
        vec![("Pods", 4, 15), ("Percent", 100, 15)]
    } else {
        vec![("Percent", 100, 15)]
    };
    let policies = configured
        .filter(|policies| !policies.is_empty())
        .map(|policies| {
            policies
                .iter()
                .map(|policy| (policy.type_.as_str(), policy.value, policy.period_seconds))
                .collect::<Vec<_>>()
        })
        .unwrap_or(defaults);
    let events = history.entry(key.to_string()).or_default();
    if events.last().is_some_and(|event| event.to != current) {
        events.clear();
    }
    events.retain(|event| now.saturating_duration_since(event.at) < Duration::from_secs(1800));
    let mut limits = Vec::new();
    for (policy_type, value, period_seconds) in policies {
        if period_seconds <= 0 || value <= 0 {
            continue;
        }
        let period = Duration::from_secs(period_seconds as u64);
        let recent_delta = events
            .iter()
            .filter(|event| now.saturating_duration_since(event.at) < period)
            .map(|event| event.to - event.from)
            .sum::<i32>();
        let baseline = current.saturating_sub(recent_delta).max(0);
        let limit = match (scale_up, policy_type) {
            (true, "Pods") => baseline.saturating_add(value),
            (true, "Percent") => ((i64::from(baseline) * (100 + i64::from(value)) + 99) / 100)
                .clamp(0, i64::from(i32::MAX)) as i32,
            (false, "Pods") => baseline.saturating_sub(value).max(0),
            (false, "Percent") => (i64::from(baseline) * (100 - i64::from(value)) / 100)
                .clamp(0, i64::from(i32::MAX)) as i32,
            _ => continue,
        };
        limits.push(limit);
    }
    if limits.is_empty() {
        return (current, true);
    }
    let selected = match rules.and_then(|rules| rules.select_policy.as_deref()) {
        Some("Min") => *limits.iter().min().expect("limits is non-empty"),
        _ => *limits.iter().max().expect("limits is non-empty"),
    };
    let limited = if scale_up {
        desired.min(selected)
    } else {
        desired.max(selected)
    };
    (limited, limited != desired)
}

fn ready(pod: &Pod) -> bool {
    pod.status.as_ref().is_some_and(|status| {
        status.phase.as_deref() == Some("Running")
            && status.conditions.as_ref().is_some_and(|conditions| {
                conditions
                    .iter()
                    .any(|condition| condition.type_ == "Ready" && condition.status == "True")
            })
    })
}

fn resource_metric_value(
    target: &k8s_openapi::api::autoscaling::v2::MetricTarget,
    total_usage: f64,
    total_request: f64,
    pod_count: usize,
) -> Result<MetricValueStatus> {
    match target.type_.as_str() {
        "Utilization" => {
            if total_request <= 0.0 {
                anyhow::bail!("Resource Utilization metric requires positive requests");
            }
            Ok(MetricValueStatus {
                average_utilization: Some(((total_usage / total_request) * 100.0).floor() as i32),
                ..Default::default()
            })
        }
        "AverageValue" => {
            if pod_count == 0 {
                anyhow::bail!("Resource AverageValue metric has no Ready Pods");
            }
            Ok(MetricValueStatus {
                average_value: Some(k8s_openapi::apimachinery::pkg::api::resource::Quantity(
                    (total_usage / pod_count as f64).to_string(),
                )),
                ..Default::default()
            })
        }
        other => anyhow::bail!("unsupported resource metric target type {other}"),
    }
}

async fn current_resource_values(
    hpa: &HorizontalPodAutoscaler,
    selector: &str,
    resource: &str,
    container: Option<&str>,
    base_client: &Client,
) -> Result<(f64, f64, usize)> {
    if selector.is_empty() {
        anyhow::bail!("HPA target Scale has no Pod selector for its resource metric");
    }
    let namespace = hpa.namespace().unwrap_or_default();
    let pods_api: Api<Pod> = Api::namespaced(base_client.clone(), &namespace);
    let pods = pods_api
        .list(&ListParams::default().labels(selector))
        .await
        .context("listing HPA target Pods")?;
    let metrics_resource = ApiResource::from_gvk_with_plural(
        &kube::core::gvk::GroupVersionKind::gvk("metrics.k8s.io", "v1beta1", "PodMetrics"),
        "pods",
    );
    let metrics_api: Api<DynamicObject> =
        Api::namespaced_with(base_client.clone(), &namespace, &metrics_resource);
    let metrics = metrics_api
        .list(&ListParams::default().labels(selector))
        .await
        .context("reading Pod resource metrics from metrics.k8s.io")?;
    let metrics: PodMetricsList = serde_json::from_value(serde_json::to_value(metrics)?)
        .context("decoding metrics.k8s.io PodMetrics response")?;
    let metrics_by_name = metrics
        .items
        .into_iter()
        .filter_map(|pod| Some((pod.metadata.name?, pod.containers)))
        .collect::<HashMap<_, _>>();

    let mut total_usage = 0.0;
    let mut total_request = 0.0;
    let mut pod_count = 0;
    for pod in pods.items.iter().filter(|pod| ready(pod)) {
        let name = pod
            .metadata
            .name
            .as_deref()
            .ok_or_else(|| anyhow!("ready HPA target Pod is missing its name"))?;
        let spec = pod
            .spec
            .as_ref()
            .ok_or_else(|| anyhow!("ready HPA target Pod {name} has no spec"))?;
        let matching_containers = spec
            .containers
            .iter()
            .filter(|candidate| container.is_none_or(|wanted| candidate.name == wanted));
        let mut pod_usage = 0.0;
        let mut pod_request = 0.0;
        for candidate in matching_containers {
            let request_quantity = candidate
                .resources
                .as_ref()
                .and_then(|resources| resources.requests.as_ref())
                .and_then(|requests| requests.get(resource));
            let request = request_quantity
                .map(|quantity| {
                    quantity_value(&quantity.0).ok_or_else(|| {
                        anyhow!("ready HPA target Pod {name} container {} has an invalid {resource} request", candidate.name)
                    })
                })
                .transpose()?
                .unwrap_or(0.0);
            let usage = metrics_by_name
                .get(name)
                .and_then(|containers| {
                    containers
                        .iter()
                        .find(|metric| metric.name == candidate.name)
                })
                .and_then(|metric| metric.usage.get(resource))
                .and_then(|quantity| quantity_value(quantity))
                .ok_or_else(|| {
                    anyhow!(
                        "metrics.k8s.io has no {resource} sample for Pod {name} container {}",
                        candidate.name
                    )
                })?;
            pod_usage += usage;
            pod_request += request;
        }
        if container.is_some() && pod_usage == 0.0 && pod_request == 0.0 {
            anyhow::bail!("ready HPA target Pod {name} is missing the requested container");
        }
        total_usage += pod_usage;
        total_request += pod_request;
        pod_count += 1;
    }
    if pod_count == 0 {
        anyhow::bail!("HPA target has no Ready Pods with resource metrics");
    }
    Ok((total_usage, total_request, pod_count))
}

fn is_scale_subresource(caps: &ApiCapabilities) -> bool {
    caps.subresources
        .iter()
        .any(|(resource, _)| resource.plural == "scale")
}

async fn discover_scale_resources(
    client: &Client,
) -> Result<HashMap<(String, String), ScaleResource>> {
    let discovery = Discovery::new(client.clone())
        .run_aggregated()
        .await
        .context("discovering resources with Scale subresources for HPA")?;
    let mut resources = HashMap::new();
    for group in discovery.groups() {
        for (resource, capabilities) in group.recommended_resources() {
            if is_scale_subresource(&capabilities) {
                resources.insert(
                    (resource.api_version.clone(), resource.kind.clone()),
                    ScaleResource { resource },
                );
            }
        }
    }
    Ok(resources)
}

fn target_api_version(hpa: &HorizontalPodAutoscaler) -> Result<(String, String)> {
    let target = &hpa
        .spec
        .as_ref()
        .ok_or_else(|| anyhow!("HPA has no spec"))?
        .scale_target_ref;
    let api_version = target.api_version.as_deref().unwrap_or("v1");
    let (group, version) = api_version
        .split_once('/')
        .map_or(("", api_version), |(group, version)| (group, version));
    let api_version = if group.is_empty() {
        version.to_string()
    } else {
        format!("{group}/{version}")
    };
    Ok((api_version, target.kind.clone()))
}

fn set_hpa_condition(
    conditions: &mut Vec<HorizontalPodAutoscalerCondition>,
    condition_type: &str,
    status: &str,
    reason: &str,
    message: &str,
    now: &k8s_openapi::apimachinery::pkg::apis::meta::v1::Time,
) {
    let transition_time = conditions
        .iter()
        .find(|condition| condition.type_ == condition_type)
        .filter(|condition| condition.status == status)
        .and_then(|condition| condition.last_transition_time.clone())
        .unwrap_or_else(|| now.clone());
    let value = HorizontalPodAutoscalerCondition {
        type_: condition_type.to_string(),
        status: status.to_string(),
        reason: Some(reason.to_string()),
        message: Some(message.to_string()),
        last_transition_time: Some(transition_time),
    };
    if let Some(slot) = conditions
        .iter_mut()
        .find(|condition| condition.type_ == condition_type)
    {
        *slot = value;
    } else {
        conditions.push(value);
    }
}

fn behavior_tolerance(rules: Option<&k8s_openapi::api::autoscaling::v2::HPAScalingRules>) -> f64 {
    rules
        .and_then(|rules| rules.tolerance.as_ref())
        .and_then(|quantity| quantity_value(&quantity.0))
        .filter(|tolerance| (0.0..=1.0).contains(tolerance))
        .unwrap_or(DEFAULT_TOLERANCE)
}

fn is_hpa_owned_zero_state(
    desired_replicas: i32,
    conditions: &[HorizontalPodAutoscalerCondition],
) -> bool {
    desired_replicas == 0
        && conditions
            .iter()
            .any(|condition| condition.type_ == "ScaledToZero" && condition.status == "True")
}

async fn reconcile_hpa(
    client: &Client,
    base_client: &Client,
    hpa: &HorizontalPodAutoscaler,
    scale_resources: &mut HashMap<(String, String), ScaleResource>,
    recommendations: &mut HashMap<String, Vec<(Instant, i32)>>,
    scale_events: &mut HashMap<String, Vec<ScaleEvent>>,
) {
    let namespace = hpa.namespace().unwrap_or_default();
    let name = hpa.name_any();
    let result = async {
        let spec = hpa
            .spec
            .as_ref()
            .ok_or_else(|| anyhow!("HPA has no spec"))?;
        let target_key = target_api_version(hpa)?;
        if !scale_resources.contains_key(&target_key) {
            *scale_resources = discover_scale_resources(base_client).await?;
        }
        let target = scale_resources
            .get(&target_key)
            .ok_or_else(|| anyhow!("HPA target does not expose a Scale subresource"))?;
        let target_api: Api<DynamicObject> =
            Api::namespaced_with(client.clone(), &namespace, &target.resource);
        let scale = target_api
            .get_scale(&spec.scale_target_ref.name)
            .await
            .context("getting HPA target Scale")?;
        let scale_status = scale
            .status
            .as_ref()
            .ok_or_else(|| anyhow!("HPA target Scale has no status"))?;
        let selector = scale_status.selector.as_deref().unwrap_or("");
        let current_desired = scale.spec.and_then(|spec| spec.replicas).unwrap_or(0);
        let current_actual = scale_status.replicas;
        let minimum = spec.min_replicas.unwrap_or(1);
        let metrics = spec.metrics.clone().unwrap_or_else(|| {
            vec![k8s_openapi::api::autoscaling::v2::MetricSpec {
                type_: "Resource".to_string(),
                resource: Some(k8s_openapi::api::autoscaling::v2::ResourceMetricSource {
                    name: "cpu".to_string(),
                    target: k8s_openapi::api::autoscaling::v2::MetricTarget {
                        type_: "Utilization".to_string(),
                        average_utilization: Some(DEFAULT_CPU_TARGET_PERCENT),
                        ..Default::default()
                    },
                }),
                ..Default::default()
            }]
        });
        let mut scaled_to_zero = hpa
            .status
            .as_ref()
            .and_then(|status| status.conditions.as_ref())
            .is_some_and(|conditions| is_hpa_owned_zero_state(current_desired, conditions));
        // Kubernetes 1.37 distinguishes a target the HPA scaled to zero from
        // one an operator paused manually. Object and External metrics make
        // scaling from an HPA-owned zero possible, but do not override a
        // manual pause. The Scale write and HPA status write are separate API
        // operations, so a requeue can observe zero before the watch cache has
        // delivered ScaledToZero. Read the latest HPA status on that edge
        // before classifying the zero as manual.
        if current_desired == 0 && !scaled_to_zero {
            let hpas: Api<HorizontalPodAutoscaler> =
                Api::namespaced(base_client.clone(), &namespace);
            let latest_hpa = hpas
                .get(&name)
                .await
                .context("checking the latest HPA zero-replica condition")?;
            scaled_to_zero = latest_hpa
                .status
                .as_ref()
                .and_then(|status| status.conditions.as_ref())
                .is_some_and(|conditions| is_hpa_owned_zero_state(current_desired, conditions));
        }
        if current_desired == 0 && !scaled_to_zero {
            anyhow::bail!("HPA target is manually scaled to zero; scaling is paused until it is scaled above zero");
        }
        let tolerances = (
            behavior_tolerance(
                spec.behavior
                    .as_ref()
                    .and_then(|behavior| behavior.scale_up.as_ref()),
            ),
            behavior_tolerance(
                spec.behavior
                    .as_ref()
                    .and_then(|behavior| behavior.scale_down.as_ref()),
            ),
        );
        let mut metric_recommendations = Vec::with_capacity(metrics.len());
        let mut metric_errors = Vec::new();
        for metric in &metrics {
            match evaluate_metric(
                metric,
                hpa,
                selector,
                current_desired,
                base_client,
                tolerances,
            )
            .await
            {
                Ok(recommendation) => metric_recommendations.push(recommendation),
                Err(error) => metric_errors.push(error.to_string()),
            }
        }
        if metric_recommendations.is_empty() {
            anyhow::bail!("no HPA metrics could be evaluated: {}", metric_errors.join("; "));
        }
        let recommendation = metric_recommendations
            .iter()
            .map(|recommendation| recommendation.desired)
            .max()
            .unwrap_or(current_desired);
        let mut desired = recommendation.clamp(minimum, spec.max_replicas.max(minimum));
        let mut limited = recommendation != desired;
        let mut limit_reason = if limited && recommendation < minimum {
            "TooFewReplicas"
        } else if limited {
            "TooManyReplicas"
        } else {
            "DesiredWithinRange"
        };
        // A failed metric must never cause a downscale. Upstream HPA uses the
        // best valid metric for scale-up while holding current replicas when
        // any remaining metric failed.
        let metric_error = !metric_errors.is_empty();
        if metric_error && desired < current_desired {
            desired = current_desired;
        }
        let key = format!("{namespace}/{name}");
        let stabilization_window = if desired < current_desired {
            spec.behavior
                .as_ref()
                .and_then(|behavior| behavior.scale_down.as_ref())
                .and_then(|rules| rules.stabilization_window_seconds)
                .map(|seconds| seconds.max(0) as u64)
                .unwrap_or(300)
        } else if desired > current_desired {
            spec.behavior
                .as_ref()
                .and_then(|behavior| behavior.scale_up.as_ref())
                .and_then(|rules| rules.stabilization_window_seconds)
                .map(|seconds| seconds.max(0) as u64)
                .unwrap_or(0)
        } else {
            0
        };
        let now = Instant::now();
        let desired = stabilized_recommendation(
            &key,
            current_desired,
            desired,
            Duration::from_secs(stabilization_window),
            now,
            recommendations,
        );
        let scale_up = desired > current_desired;
        let rules = spec.behavior.as_ref().and_then(|behavior| {
            if scale_up {
                behavior.scale_up.as_ref()
            } else {
                behavior.scale_down.as_ref()
            }
        });
        let (desired, rate_limited) = apply_rate_policy(
            current_desired,
            desired,
            scale_up,
            rules,
            now,
            scale_events,
            &key,
        );
        if rate_limited {
            limited = true;
            limit_reason = if scale_up { "ScaleUpLimit" } else { "ScaleDownLimit" };
        }
        if desired != current_desired {
            target_api
                .patch_scale(
                    &spec.scale_target_ref.name,
                    &PatchParams::default(),
                    &Patch::Merge(&serde_json::json!({"spec": {"replicas": desired}})),
                )
                .await
                .context("patching HPA target Scale")?;
            scale_events
                .entry(key.clone())
                .or_default()
                .push(ScaleEvent {
                    at: now,
                    from: current_desired,
                    to: desired,
                });
        }
        Ok::<_, anyhow::Error>((
            current_actual,
            current_desired,
            desired,
            limited,
            limit_reason,
            metric_recommendations,
            metric_error,
        ))
    }
    .await;

    match result {
        Ok((
            current_actual,
            current_desired,
            desired,
            limited,
            limit_reason,
            metric_recommendations,
            metric_error,
        )) => {
            let now = k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::now(),
            );
            let mut status = hpa.status.clone().unwrap_or_default();
            let mut conditions = status.conditions.take().unwrap_or_default();
            for (condition_type, condition_status, reason, message) in [
                (
                    "AbleToScale",
                    "True",
                    "SucceededGetScale",
                    "the target Scale was read",
                ),
                (
                    "ScalingActive",
                    "True",
                    "ValidMetricFound",
                    if metric_error {
                        "one or more metrics could not be read"
                    } else {
                        "the configured metrics are available"
                    },
                ),
                (
                    "ScalingLimited",
                    if limited { "True" } else { "False" },
                    limit_reason,
                    if limited {
                        if limit_reason == "ScaleUpLimit" || limit_reason == "ScaleDownLimit" {
                            "the desired replica count was constrained by the scale rate policy"
                        } else {
                            "the desired replica count was constrained by the HPA bounds"
                        }
                    } else {
                        "the desired replica count is within the HPA bounds"
                    },
                ),
            ] {
                let previous = conditions
                    .iter()
                    .find(|condition| condition.type_ == condition_type);
                let transition_time = previous
                    .filter(|condition| condition.status == condition_status)
                    .and_then(|condition| condition.last_transition_time.clone())
                    .unwrap_or_else(|| now.clone());
                let value = HorizontalPodAutoscalerCondition {
                    type_: condition_type.to_string(),
                    status: condition_status.to_string(),
                    reason: Some(reason.to_string()),
                    message: Some(message.to_string()),
                    last_transition_time: Some(transition_time),
                };
                if let Some(slot) = conditions
                    .iter_mut()
                    .find(|condition| condition.type_ == condition_type)
                {
                    *slot = value;
                } else {
                    conditions.push(value);
                }
            }
            let scaled_to_zero_status = if desired == 0 { "True" } else { "False" };
            let scaled_to_zero_reason = if desired == 0 {
                "ScaledToZero"
            } else {
                "ScaledAboveZero"
            };
            let scaled_to_zero_message = if desired == 0 {
                "the HPA scaled the target to zero replicas"
            } else {
                "the target has a non-zero desired replica count"
            };
            let previous = conditions
                .iter()
                .find(|condition| condition.type_ == "ScaledToZero");
            let transition_time = previous
                .filter(|condition| condition.status == scaled_to_zero_status)
                .and_then(|condition| condition.last_transition_time.clone())
                .unwrap_or_else(|| now.clone());
            let scaled_to_zero_condition = HorizontalPodAutoscalerCondition {
                type_: "ScaledToZero".to_string(),
                status: scaled_to_zero_status.to_string(),
                reason: Some(scaled_to_zero_reason.to_string()),
                message: Some(scaled_to_zero_message.to_string()),
                last_transition_time: Some(transition_time),
            };
            if let Some(slot) = conditions
                .iter_mut()
                .find(|condition| condition.type_ == "ScaledToZero")
            {
                *slot = scaled_to_zero_condition;
            } else {
                conditions.push(scaled_to_zero_condition);
            }
            status.current_replicas = Some(current_actual);
            status.desired_replicas = desired;
            status.current_metrics = Some(
                metric_recommendations
                    .into_iter()
                    .map(|item| item.status)
                    .collect(),
            );
            status.observed_generation = hpa.metadata.generation;
            status.conditions = Some(conditions);
            if desired != current_desired {
                status.last_scale_time = Some(now);
            }
            if hpa.status.as_ref() != Some(&status) {
                let api: Api<HorizontalPodAutoscaler> = Api::namespaced(client.clone(), &namespace);
                if let Err(error) = api
                    .patch_status(
                        &name,
                        &PatchParams::default(),
                        &Patch::Merge(&serde_json::json!({"status": status})),
                    )
                    .await
                {
                    tracing::warn!(namespace = %namespace, hpa = %name, error = ?error, "failed to update HPA status");
                    return;
                }
            }
        }
        Err(error) => {
            tracing::warn!(namespace = %namespace, hpa = %name, error = ?error, "HPA reconciliation failed; will retry");
            let mut status = hpa.status.clone().unwrap_or_default();
            let mut conditions = status.conditions.take().unwrap_or_default();
            let now = k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::now(),
            );
            let message = error.to_string();
            let reason = if message.contains("manually scaled to zero") {
                "ScalingDisabled"
            } else {
                "FailedGetMetrics"
            };
            set_hpa_condition(
                &mut conditions,
                "ScalingActive",
                "False",
                reason,
                &message,
                &now,
            );
            status.observed_generation = hpa.metadata.generation;
            status.conditions = Some(conditions);
            if hpa.status.as_ref() != Some(&status) {
                let api: Api<HorizontalPodAutoscaler> = Api::namespaced(client.clone(), &namespace);
                if let Err(status_error) = api
                    .patch_status(
                        &name,
                        &PatchParams::default(),
                        &Patch::Merge(&serde_json::json!({"status": status})),
                    )
                    .await
                {
                    tracing::warn!(namespace = %namespace, hpa = %name, error = ?status_error, "failed to publish HPA metric error condition");
                }
            }
        }
    }
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let base_client = crate::watch::base_client();
    let mut hpas = HashMap::<String, HorizontalPodAutoscaler>::new();
    let mut scale_resources = HashMap::new();
    let mut recommendations = HashMap::<String, Vec<(Instant, i32)>>::new();
    let mut scale_events = HashMap::<String, Vec<ScaleEvent>>::new();
    let queue = std::sync::Arc::new(crate::workqueue::KeyedWorkQueue::default());
    let mut hpa_stream = crate::watch::watch_horizontal_pod_autoscalers(&client);
    let mut pod_stream = crate::watch::watch_pods(&client);
    let mut crd_stream = crate::watch::watch_custom_resource_definitions(&client);
    let mut resync = tokio::time::interval(SYNC_PERIOD);
    resync.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            event = hpa_stream.next() => match event {
                Some(Ok(Event::Apply(hpa))) | Some(Ok(Event::InitApply(hpa))) => {
                    let key = format!("{}/{}", hpa.namespace().unwrap_or_default(), hpa.name_any());
                    hpas.insert(key.clone(), hpa);
                    queue.enqueue(key);
                }
                Some(Ok(Event::Delete(hpa))) => {
                    let key = format!("{}/{}", hpa.namespace().unwrap_or_default(), hpa.name_any());
                    hpas.remove(&key);
                    recommendations.remove(&key);
                    scale_events.remove(&key);
                }
                Some(Ok(Event::Init | Event::InitDone)) => {}
                Some(Err(error)) => tracing::warn!(error = ?error, "HPA watch error"),
                None => return Ok(()),
            },
            event = pod_stream.next() => match event {
                Some(Ok(Event::Apply(pod))) | Some(Ok(Event::InitApply(pod))) | Some(Ok(Event::Delete(pod))) => {
                    let namespace = pod.namespace().unwrap_or_default();
                    for (key, hpa) in &hpas {
                        if hpa.namespace().as_deref() == Some(namespace.as_str()) { queue.enqueue(key.clone()); }
                    }
                }
                Some(Ok(Event::Init | Event::InitDone)) => {}
                Some(Err(error)) => tracing::warn!(error = ?error, "Pod watch error in HPA controller"),
                None => return Ok(()),
            },
            event = crd_stream.next() => match event {
                Some(Ok(Event::Apply(_))) | Some(Ok(Event::InitApply(_))) | Some(Ok(Event::Delete(_))) => {
                    scale_resources.clear();
                    for key in hpas.keys() { queue.enqueue(key.clone()); }
                }
                Some(Ok(Event::Init | Event::InitDone)) => {}
                Some(Err(error)) => tracing::warn!(error = ?error, "CRD watch error in HPA controller"),
                None => return Ok(()),
            },
            _ = resync.tick() => {
                for key in hpas.keys() { queue.enqueue(key.clone()); }
            }
            key = queue.pop() => {
                if let Some(hpa) = hpas.get(&key).cloned() {
                    reconcile_hpa(
                        &client,
                        &base_client,
                        &hpa,
                        &mut scale_resources,
                        &mut recommendations,
                        &mut scale_events,
                    )
                    .await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{desired_replicas, stabilized_recommendation};
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::time::Instant;

    #[test]
    fn recommendation_scales_up_down_and_respects_default_tolerance() {
        assert_eq!(desired_replicas(2, 100, 50, 1, 10, 0.1, 0.1), (4, false));
        assert_eq!(desired_replicas(4, 25, 50, 1, 10, 0.1, 0.1), (2, false));
        assert_eq!(desired_replicas(3, 54, 50, 1, 10, 0.1, 0.1), (3, false));
    }

    #[test]
    fn recommendation_clamps_to_hpa_bounds() {
        assert_eq!(desired_replicas(2, 900, 50, 1, 5, 0.1, 0.1), (5, true));
        assert_eq!(desired_replicas(5, 10, 50, 2, 10, 0.1, 0.1), (2, true));
        assert_eq!(desired_replicas(1, 0, 50, 0, 5, 0.1, 0.1), (0, false));
    }

    #[test]
    fn hpa_behavior_can_override_directional_tolerance() {
        assert_eq!(
            desired_replicas(10, 105, 100, 1, 20, 0.02, 0.1),
            (11, false)
        );
        assert_eq!(desired_replicas(10, 90, 100, 1, 20, 0.1, 0.05), (9, false));
    }

    #[test]
    fn only_hpa_owned_zero_state_can_scale_from_zero() {
        use super::is_hpa_owned_zero_state;

        let condition =
            |status: &str| k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscalerCondition {
                type_: "ScaledToZero".to_string(),
                status: status.to_string(),
                ..Default::default()
            };
        assert!(!is_hpa_owned_zero_state(0, &[]));
        assert!(!is_hpa_owned_zero_state(0, &[condition("False")]));
        assert!(is_hpa_owned_zero_state(0, &[condition("True")]));
        assert!(!is_hpa_owned_zero_state(1, &[condition("True")]));
    }

    #[test]
    fn external_metric_recommendations_support_scale_to_zero() {
        use super::{metric_recommendation, quantity_value};

        assert_eq!(quantity_value("1500m"), Some(1.5));
        assert_eq!(quantity_value("2Ki"), Some(2048.0));
        assert_eq!(
            metric_recommendation(0, 0.0, 10.0, false, 0.1, 0.1).unwrap(),
            0
        );
        assert_eq!(
            metric_recommendation(0, 21.0, 10.0, false, 0.1, 0.1).unwrap(),
            3
        );
        assert_eq!(
            metric_recommendation(4, 21.0, 10.0, false, 0.1, 0.1).unwrap(),
            9
        );
        assert_eq!(
            metric_recommendation(4, 21.0, 10.0, true, 0.1, 0.1).unwrap(),
            3
        );
        assert_eq!(
            metric_recommendation(4, 36.0, 10.0, true, 0.1, 0.1).unwrap(),
            4
        );
    }

    #[test]
    fn scale_down_stabilization_starts_from_current_replicas() {
        use super::stabilized_recommendation;
        use std::collections::HashMap;
        use std::time::Duration;
        use tokio::time::Instant;

        let start = Instant::now();
        let mut history = HashMap::new();
        let window = Duration::from_secs(15);

        assert_eq!(
            stabilized_recommendation("ns/hpa", 1, 0, window, start, &mut history),
            1
        );
        assert_eq!(
            stabilized_recommendation(
                "ns/hpa",
                1,
                0,
                window,
                start + Duration::from_secs(14),
                &mut history,
            ),
            1
        );
        assert_eq!(
            stabilized_recommendation(
                "ns/hpa",
                1,
                0,
                window,
                start + Duration::from_secs(16),
                &mut history,
            ),
            0
        );
    }

    #[test]
    fn stabilization_uses_highest_downscale_and_lowest_upscale_recommendations() {
        let now = Instant::now();
        let mut history = HashMap::new();
        assert_eq!(
            stabilized_recommendation("ns/hpa", 1, 3, Duration::ZERO, now, &mut history),
            3
        );
        assert_eq!(
            stabilized_recommendation(
                "ns/hpa",
                3,
                1,
                Duration::from_secs(300),
                now + Duration::from_secs(15),
                &mut history,
            ),
            3
        );
        assert_eq!(
            stabilized_recommendation(
                "ns/hpa",
                3,
                1,
                Duration::ZERO,
                now + Duration::from_secs(16),
                &mut history,
            ),
            1
        );
        assert_eq!(
            stabilized_recommendation(
                "ns/hpa",
                1,
                4,
                Duration::from_secs(300),
                now + Duration::from_secs(30),
                &mut history,
            ),
            1
        );
    }

    #[test]
    fn scaling_policies_limit_velocity_and_select_policy() {
        use super::{apply_rate_policy, ScaleEvent};
        use k8s_openapi::api::autoscaling::v2::{HPAScalingPolicy, HPAScalingRules};

        let now = Instant::now();
        let mut events = HashMap::<String, Vec<ScaleEvent>>::new();
        let rules = HPAScalingRules {
            policies: Some(vec![
                HPAScalingPolicy {
                    type_: "Pods".to_string(),
                    value: 2,
                    period_seconds: 60,
                },
                HPAScalingPolicy {
                    type_: "Percent".to_string(),
                    value: 50,
                    period_seconds: 60,
                },
            ]),
            select_policy: Some("Max".to_string()),
            ..Default::default()
        };
        assert_eq!(
            apply_rate_policy(2, 10, true, Some(&rules), now, &mut events, "ns/hpa"),
            (4, true)
        );
        events
            .entry("ns/hpa".to_string())
            .or_default()
            .push(ScaleEvent {
                at: now,
                from: 2,
                to: 4,
            });
        assert_eq!(
            apply_rate_policy(
                4,
                10,
                true,
                Some(&rules),
                now + Duration::from_secs(10),
                &mut events,
                "ns/hpa",
            ),
            (4, true)
        );
        let min_rules = HPAScalingRules {
            select_policy: Some("Min".to_string()),
            ..rules
        };
        assert_eq!(
            apply_rate_policy(
                2,
                10,
                true,
                Some(&min_rules),
                now,
                &mut HashMap::new(),
                "ns/hpa",
            ),
            (3, true)
        );
    }
}
