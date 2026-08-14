//! HTTP extenders — the pre-scheduling-framework extension mechanism
//! upstream still supports and still calls from inside a normal cycle: an
//! arbitrary out-of-tree HTTP service, consulted during `Filter` and/or
//! `Prioritize` (upstream's name for `Score`), configured once at startup
//! rather than compiled in.
//!
//! # Where this sits relative to the framework
//!
//! An extender is not a [`crate::framework::FilterPlugin`]/[`crate::framework::ScorePlugin`] —
//! those traits are deliberately synchronous and pure (see `cycle.rs`'s
//! module header), and an extender call is neither: it is one HTTP round
//! trip covering every candidate node at once, not a per-node predicate.
//! `cycle.rs`'s `Scheduler::schedule_one` is `async` and calls extenders
//! directly, between the Filter phase and Score phase, exactly where
//! upstream's own `genericScheduler` does — `findNodesThatPassExtenders`
//! right after plugin `Filter`, extender `Prioritize` right after plugin
//! `Score` and before the scores are combined.
//!
//! # What this does not implement
//!
//! An extender's `bindVerb` (taking over `Bind` from `DefaultBinder`) and
//! its preemption verb are refused at config-parse time — `Config::from_env`
//! errors naming the field, rather than silently accepting a config it would
//! ignore. Both are exceptionally rare in real deployments (most extenders
//! that exist at all only implement Filter/Prioritize), and getting them
//! silently wrong — a bind that never happens, or a preemption decision the
//! extender never saw — is a worse failure mode than refusing to start.
//!
//! # Pod/Node fidelity
//!
//! This scheduler caches projections, not whole API objects (see
//! `cache/pod.rs`/`cache/node.rs`'s own module headers). An extender call
//! reconstructs a `v1.Pod`/`v1.Node` from those projections — `metadata`
//! (name, namespace, uid, labels), plus `spec.taints` and
//! `status.allocatable` for nodes — rather than holding a second full copy
//! of every object purely for the rare case an extender is configured. Real
//! extenders overwhelmingly key off identity (name/namespace/labels), which
//! this preserves exactly; one that inspects a container image or a volume
//! mount would not find it here.

use crate::cache::{NodeInfo, PodInfo};
use k8s_openapi::api::core::v1::{Node, NodeSpec, NodeStatus, Pod, PodSpec};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

/// One configured extender. Parsed from `NODESCHEDULER_EXTENDERS_JSON` — see
/// `config.rs`.
#[derive(Clone, Debug)]
pub struct ExtenderConfig {
    pub url_prefix: String,
    pub filter_verb: Option<String>,
    pub prioritize_verb: Option<String>,
    pub weight: i64,
    pub node_cache_capable: bool,
    pub ignorable: bool,
    pub http_timeout: Duration,
    /// Upstream's `managedResources`: if non-empty, this extender is only
    /// consulted for a pod that requests at least one of these extended
    /// resource names. Empty (the common case, and upstream's own default)
    /// means every pod.
    pub managed_resources: Vec<String>,
}

impl ExtenderConfig {
    /// Whether this extender should be consulted for `pod` at all — see
    /// `managed_resources`'s doc comment.
    pub fn applies_to(&self, pod: &PodInfo) -> bool {
        self.managed_resources.is_empty()
            || self.managed_resources.iter().any(|r| pod.requests.extended.contains_key(r))
    }
}

/// The raw JSON shape `NODESCHEDULER_EXTENDERS_JSON` is parsed from —
/// upstream's own `KubeSchedulerConfiguration` extender fields, spelled the
/// same way (`urlPrefix`, `filterVerb`, …) so a config an operator already
/// has for real kube-scheduler needs minimal translation. Two fields are
/// deliberately not upstream's own shape: `httpTimeoutSeconds` is a plain
/// number rather than upstream's Go duration string (`"5s"`), because this
/// project's whole configuration surface is env vars carrying plain JSON,
/// not a YAML file loader — and `enableHTTPS`/`tlsConfig` don't exist here
/// at all, because `urlPrefix` is expected to already carry its scheme
/// (`"https://…"`) the way every other URL this project's env vars accept
/// does, which makes a separate "use https" flag redundant.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RawExtenderConfig {
    pub url_prefix: String,
    pub filter_verb: Option<String>,
    pub prioritize_verb: Option<String>,
    #[serde(default)]
    pub bind_verb: Option<String>,
    #[serde(default)]
    pub preempt_verb: Option<String>,
    #[serde(default)]
    pub tls_config: Option<serde_json::Value>,
    #[serde(default = "default_weight")]
    pub weight: i64,
    #[serde(default)]
    pub node_cache_capable: bool,
    #[serde(default)]
    pub ignorable: bool,
    #[serde(default = "default_timeout_seconds")]
    pub http_timeout_seconds: u64,
    #[serde(default)]
    pub managed_resources: Vec<RawManagedResource>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RawManagedResource {
    pub name: String,
}

fn default_weight() -> i64 {
    1
}
fn default_timeout_seconds() -> u64 {
    5
}

/// Parse `NODESCHEDULER_EXTENDERS_JSON` into configs this crate can act on —
/// refusing `bindVerb`/`preemptVerb`/`tlsConfig` by name rather than
/// silently ignoring them, per this module's header.
pub fn parse_extenders(raw: &str) -> anyhow::Result<Vec<ExtenderConfig>> {
    let parsed: Vec<RawExtenderConfig> = serde_json::from_str(raw)
        .map_err(|e| anyhow::anyhow!("NODESCHEDULER_EXTENDERS_JSON is not a valid extender array: {e}"))?;
    let mut out = Vec::with_capacity(parsed.len());
    for r in parsed {
        if r.bind_verb.is_some() {
            anyhow::bail!(
                "extender {:?} sets bindVerb, which this scheduler does not implement — an \
                 extender-driven Bind would silently never run. Remove bindVerb (DefaultBinder \
                 will bind instead) or drop this extender.",
                r.url_prefix
            );
        }
        if r.preempt_verb.is_some() {
            anyhow::bail!(
                "extender {:?} sets preemptVerb, which this scheduler does not implement — \
                 preemption would silently proceed without consulting it. Remove preemptVerb \
                 or drop this extender.",
                r.url_prefix
            );
        }
        if r.tls_config.is_some() {
            anyhow::bail!(
                "extender {:?} sets tlsConfig, which this scheduler does not implement — a \
                 custom CA/client cert would silently not be applied. Terminate TLS in front of \
                 the extender with a certificate this host already trusts, or drop tlsConfig.",
                r.url_prefix
            );
        }
        if r.filter_verb.is_none() && r.prioritize_verb.is_none() {
            anyhow::bail!(
                "extender {:?} sets neither filterVerb nor prioritizeVerb — it would never be \
                 called for anything.",
                r.url_prefix
            );
        }
        out.push(ExtenderConfig {
            url_prefix: r.url_prefix,
            filter_verb: r.filter_verb,
            prioritize_verb: r.prioritize_verb,
            weight: r.weight,
            node_cache_capable: r.node_cache_capable,
            ignorable: r.ignorable,
            http_timeout: Duration::from_secs(r.http_timeout_seconds),
            managed_resources: r.managed_resources.into_iter().map(|m| m.name).collect(),
        });
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────────
// The wire types, matching upstream's `k8s.io/kube-scheduler/extender/v1`
// ─────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ExtenderArgs {
    pod: Pod,
    #[serde(skip_serializing_if = "Option::is_none")]
    nodes: Option<NodeListArg>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_names: Option<Vec<String>>,
}

/// Upstream sends a real `v1.NodeList`; only `items` is ever read by a real
/// extender, so `metadata`/`apiVersion`/`kind` are omitted rather than
/// faked. Also deserialized: a Filter response may itself echo back a
/// `NodeList` under `Nodes` instead of `NodeNames`.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct NodeListArg {
    items: Vec<Node>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "PascalCase")]
struct ExtenderFilterResult {
    #[serde(default)]
    nodes: Option<NodeListArg>,
    #[serde(default)]
    node_names: Option<Vec<String>>,
    #[serde(default)]
    failed_nodes: BTreeMap<String, String>,
    #[serde(default)]
    failed_and_unresolvable_nodes: BTreeMap<String, String>,
    #[serde(default)]
    error: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct HostPriority {
    host: String,
    score: i64,
}

/// The outcome of one extender's Filter call: which node names survived,
/// and which were rejected with a reason (unresolvable ones separated, the
/// same distinction `framework::status::Code` makes for plugins — an
/// unresolvable rejection is not a preemption candidate).
pub struct FilterOutcome {
    pub passed: Vec<String>,
    pub failed: BTreeMap<String, String>,
    pub failed_unresolvable: BTreeMap<String, String>,
}

fn pod_to_api(pod: &PodInfo) -> Pod {
    Pod {
        metadata: ObjectMeta {
            name: Some(pod.name.clone()),
            namespace: Some(pod.namespace.clone()),
            uid: Some(pod.uid.clone()),
            labels: Some(pod.labels.clone()),
            ..Default::default()
        },
        spec: Some(PodSpec {
            node_selector: Some(pod.node_selector.clone()),
            priority: Some(pod.priority),
            scheduler_name: Some(pod.scheduler_name.clone()),
            tolerations: Some(pod.tolerations.clone()),
            ..Default::default()
        }),
        status: None,
    }
}

fn node_to_api(node: &NodeInfo) -> Node {
    let mut allocatable: BTreeMap<String, Quantity> = BTreeMap::new();
    allocatable.insert("cpu".to_string(), Quantity(format!("{}m", node.allocatable.milli_cpu)));
    allocatable.insert("memory".to_string(), Quantity(node.allocatable.memory.to_string()));
    Node {
        metadata: ObjectMeta {
            name: Some(node.name.clone()),
            labels: Some(node.labels.clone()),
            ..Default::default()
        },
        spec: Some(NodeSpec {
            taints: Some(node.taints.clone()),
            unschedulable: Some(node.unschedulable),
            ..Default::default()
        }),
        status: Some(NodeStatus { allocatable: Some(allocatable), ..Default::default() }),
    }
}

pub struct Extender {
    pub config: ExtenderConfig,
    client: reqwest::Client,
}

impl Extender {
    pub fn new(config: ExtenderConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(config.http_timeout)
            .build()
            .map_err(|e| anyhow::anyhow!("building HTTP client for extender {:?}: {e}", config.url_prefix))?;
        Ok(Self { config, client })
    }

    /// Ask this extender which of `nodes` the pod may run on. `None` means
    /// the extender is not configured for Filter at all — the caller treats
    /// that as "every node passes", the same as a plugin with no Filter.
    pub async fn filter(&self, pod: &PodInfo, nodes: &[&NodeInfo]) -> anyhow::Result<Option<FilterOutcome>> {
        let Some(verb) = &self.config.filter_verb else { return Ok(None) };

        let args = ExtenderArgs {
            pod: pod_to_api(pod),
            nodes: (!self.config.node_cache_capable)
                .then(|| NodeListArg { items: nodes.iter().map(|n| node_to_api(n)).collect() }),
            node_names: self
                .config
                .node_cache_capable
                .then(|| nodes.iter().map(|n| n.name.clone()).collect()),
        };

        let result = self.post::<ExtenderFilterResult>(verb, &args).await?;
        if !result.error.is_empty() {
            anyhow::bail!("extender {:?} filter error: {}", self.config.url_prefix, result.error);
        }

        let passed = if let Some(names) = result.node_names {
            names
        } else if let Some(list) = result.nodes {
            list.items.into_iter().filter_map(|n| n.metadata.name).collect()
        } else {
            // Neither field set: upstream's own convention for "everything
            // passed, nothing was filtered out".
            nodes.iter().map(|n| n.name.clone()).collect()
        };

        Ok(Some(FilterOutcome {
            passed,
            failed: result.failed_nodes,
            failed_unresolvable: result.failed_and_unresolvable_nodes,
        }))
    }

    /// Ask this extender to score `nodes`. `None` means not configured for
    /// Prioritize. Scores are the extender's own raw values — upstream adds
    /// `score * weight` directly into the combined total without rescaling
    /// them onto `[0, MAX_NODE_SCORE]` the way plugin scores are, so neither
    /// does this.
    pub async fn prioritize(
        &self,
        pod: &PodInfo,
        nodes: &[&NodeInfo],
    ) -> anyhow::Result<Option<Vec<(String, i64)>>> {
        let Some(verb) = &self.config.prioritize_verb else { return Ok(None) };

        let args = ExtenderArgs {
            pod: pod_to_api(pod),
            nodes: (!self.config.node_cache_capable)
                .then(|| NodeListArg { items: nodes.iter().map(|n| node_to_api(n)).collect() }),
            node_names: self
                .config
                .node_cache_capable
                .then(|| nodes.iter().map(|n| n.name.clone()).collect()),
        };

        let result: Vec<HostPriority> = self.post(verb, &args).await?;
        Ok(Some(result.into_iter().map(|h| (h.host, h.score * self.config.weight)).collect()))
    }

    async fn post<T: serde::de::DeserializeOwned>(&self, verb: &str, args: &ExtenderArgs) -> anyhow::Result<T> {
        let url = format!("{}/{}", self.config.url_prefix.trim_end_matches('/'), verb);
        let resp = self
            .client
            .post(&url)
            .json(args)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("calling extender {url:?}: {e}"))?;
        if !resp.status().is_success() {
            anyhow::bail!("extender {url:?} returned HTTP {}", resp.status());
        }
        resp.json::<T>()
            .await
            .map_err(|e| anyhow::anyhow!("decoding extender {url:?} response: {e}"))
    }
}

#[cfg(test)]
#[path = "extender_tests.rs"]
mod tests;
