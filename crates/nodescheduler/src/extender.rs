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
//! `filterVerb`, `prioritizeVerb`, `preemptVerb`, and `bindVerb` use the
//! upstream extender/v1 wire shapes. Extender preemption runs after the
//! built-in dry run and may narrow or replace its candidate victim sets.
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

use crate::cache::{NodeInfo, PodInfo, Snapshot};
use crate::preempt::Candidate;
use crate::framework::MAX_NODE_SCORE;
use base64::Engine;

/// Upstream's `extenderv1.MaxExtenderPriority` — an extender's own score
/// scale, `[0, 10]`, distinct from a plugin's `[0, MAX_NODE_SCORE]` one. See
/// `Extender::prioritize`'s doc comment for how the two get reconciled.
const MAX_EXTENDER_PRIORITY: i64 = 10;
use k8s_openapi::api::core::v1::{Node, NodeSpec, NodeStatus, Pod, PodSpec};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

/// One configured extender. Parsed from `NODESCHEDULER_EXTENDERS_JSON` — see
/// `config.rs`.
#[derive(Clone, Debug)]
pub struct ExtenderConfig {
    pub url_prefix: String,
    pub filter_verb: Option<String>,
    pub prioritize_verb: Option<String>,
    pub preempt_verb: Option<String>,
    pub bind_verb: Option<String>,
    pub weight: i64,
    pub node_cache_capable: bool,
    pub ignorable: bool,
    pub http_timeout: Duration,
    pub enable_https: bool,
    pub tls_config: Option<ExtenderTlsConfig>,
    /// Upstream's `managedResources`: if non-empty, this extender is only
    /// consulted for a pod that requests at least one of these extended
    /// resource names. Empty (the common case, and upstream's own default)
    /// means every pod.
    pub managed_resources: Vec<String>,
    /// Resources for which the extender, not NodeResourcesFit, owns fit
    /// validation (`managedResources[].ignoredByScheduler`).
    pub ignored_by_scheduler: Vec<String>,
}

impl ExtenderConfig {
    /// Whether this extender should be consulted for `pod` at all — see
    /// `managed_resources`'s doc comment.
    pub fn applies_to(&self, pod: &PodInfo) -> bool {
        if self.managed_resources.is_empty() {
            return true;
        }
        // Upstream's IsInterested checks both requests and limits on regular
        // and init containers. Admission normally copies an extended-resource
        // limit into requests, but the extender contract must remain correct
        // for objects which bypassed or predate that defaulting.
        if let Some(api) = &pod.api_object {
            if let Some(spec) = &api.spec {
                let interested = spec
                    .containers
                    .iter()
                    .chain(spec.init_containers.iter().flatten())
                    .filter_map(|container| container.resources.as_ref())
                    .flat_map(|resources| {
                        resources
                            .requests
                            .iter()
                            .flatten()
                            .chain(resources.limits.iter().flatten())
                    })
                    .any(|(resource, _)| self.managed_resources.contains(resource));
                if interested {
                    return true;
                }
            }
        }
        self.managed_resources.iter().any(|r| pod.requests.extended.contains_key(r))
    }
}

/// The raw JSON shape `NODESCHEDULER_EXTENDERS_JSON` is parsed from —
/// upstream's own `KubeSchedulerConfiguration` extender fields, spelled the
/// same way (`urlPrefix`, `filterVerb`, …), including upstream's duration and
/// TLS fields. The older `httpTimeoutSeconds` spelling remains accepted as a
/// compatibility alias for deployments created before exact config parity.
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
    pub tls_config: Option<RawExtenderTlsConfig>,
    // serde's mechanical camelCase spelling is `enableHttps`, but the
    // Kubernetes config API preserves the initialism as `enableHTTPS`.
    #[serde(default, rename = "enableHTTPS", alias = "enableHttps")]
    pub enable_https: bool,
    #[serde(default = "default_weight")]
    pub weight: i64,
    #[serde(default)]
    pub node_cache_capable: bool,
    #[serde(default)]
    pub ignorable: bool,
    #[serde(default)]
    pub http_timeout: Option<String>,
    #[serde(default)]
    pub http_timeout_seconds: Option<u64>,
    #[serde(default)]
    pub managed_resources: Vec<RawManagedResource>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RawManagedResource {
    pub name: String,
    #[serde(default)]
    pub ignored_by_scheduler: bool,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RawExtenderTlsConfig {
    #[serde(default)]
    pub insecure: bool,
    #[serde(default)]
    pub server_name: String,
    #[serde(default)]
    pub cert_file: String,
    #[serde(default)]
    pub key_file: String,
    #[serde(default)]
    pub ca_file: String,
    pub cert_data: Option<String>,
    pub key_data: Option<String>,
    pub ca_data: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ExtenderTlsConfig {
    pub insecure: bool,
    pub server_name: String,
    pub cert_file: String,
    pub key_file: String,
    pub ca_file: String,
    pub cert_data: Option<Vec<u8>>,
    pub key_data: Option<Vec<u8>>,
    pub ca_data: Option<Vec<u8>>,
}

fn default_weight() -> i64 {
    1
}
fn parse_go_duration(raw: &str) -> anyhow::Result<Duration> {
    if raw.is_empty() {
        anyhow::bail!("duration is empty");
    }
    let bytes = raw.as_bytes();
    let mut at = 0usize;
    let mut seconds = 0f64;
    while at < bytes.len() {
        let start = at;
        while at < bytes.len() && (bytes[at].is_ascii_digit() || bytes[at] == b'.') {
            at += 1;
        }
        if start == at {
            anyhow::bail!("expected a number at byte {at}");
        }
        let value: f64 = raw[start..at].parse()?;
        let unit_start = at;
        while at < bytes.len() && !bytes[at].is_ascii_digit() && bytes[at] != b'.' {
            at += 1;
        }
        let multiplier = match &raw[unit_start..at] {
            "ns" => 1e-9,
            "us" | "µs" | "μs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            unit => anyhow::bail!("unsupported duration unit {unit:?}"),
        };
        seconds += value * multiplier;
    }
    if !seconds.is_finite() || seconds < 0.0 {
        anyhow::bail!("duration must be finite and non-negative");
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn decode_base64(raw: &str) -> anyhow::Result<Vec<u8>> {
    let compact: String = raw.chars().filter(|ch| !ch.is_ascii_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(compact)
        .map_err(|error| anyhow::anyhow!("invalid base64 data: {error}"))
}

/// Parse `NODESCHEDULER_EXTENDERS_JSON` into configs this crate can act on.
pub fn parse_extenders(raw: &str) -> anyhow::Result<Vec<ExtenderConfig>> {
    let parsed: Vec<RawExtenderConfig> = serde_json::from_str(raw)
        .map_err(|e| anyhow::anyhow!("NODESCHEDULER_EXTENDERS_JSON is not a valid extender array: {e}"))?;
    let binders = parsed
        .iter()
        .filter(|r| r.bind_verb.as_deref().is_some_and(|verb| !verb.is_empty()))
        .count();
    if binders > 1 {
        anyhow::bail!(
            "NODESCHEDULER_EXTENDERS_JSON has {binders} extenders implementing bindVerb; upstream permits only one"
        );
    }
    let mut managed = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(parsed.len());
    for mut r in parsed {
        // The upstream config types use strings where an empty value means
        // "extension not implemented". `Option<String>` preserves the
        // useful missing/present distinction during serde, then normalizes
        // the empty spelling here to the same runtime meaning.
        r.filter_verb = r.filter_verb.filter(|verb| !verb.is_empty());
        r.prioritize_verb = r.prioritize_verb.filter(|verb| !verb.is_empty());
        r.bind_verb = r.bind_verb.filter(|verb| !verb.is_empty());
        r.preempt_verb = r.preempt_verb.filter(|verb| !verb.is_empty());
        if r.filter_verb.is_none()
            && r.prioritize_verb.is_none()
            && r.preempt_verb.is_none()
            && r.bind_verb.is_none()
        {
            anyhow::bail!(
                "extender {:?} sets neither filterVerb nor prioritizeVerb — it would never be \
                 called for anything.",
                r.url_prefix
            );
        }
        if r.prioritize_verb.is_some() && r.weight <= 0 {
            anyhow::bail!(
                "extender {:?} has prioritizeVerb but weight={} — upstream requires a positive weight",
                r.url_prefix,
                r.weight,
            );
        }
        for resource in &r.managed_resources {
            if !managed.insert(resource.name.clone()) {
                anyhow::bail!(
                    "managed resource {:?} is listed by more than one extender",
                    resource.name,
                );
            }
        }
        let timeout = match r.http_timeout.as_deref() {
            Some(raw) => {
                let parsed = parse_go_duration(raw).map_err(|error| {
                    anyhow::anyhow!("extender {:?} has invalid httpTimeout {raw:?}: {error}", r.url_prefix)
                })?;
                if parsed.is_zero() { Duration::from_secs(5) } else { parsed }
            }
            None => match r.http_timeout_seconds.unwrap_or(5) {
                0 => Duration::from_secs(5),
                seconds => Duration::from_secs(seconds),
            },
        };
        let tls_config = r.tls_config.map(|tls| -> anyhow::Result<ExtenderTlsConfig> {
            Ok(ExtenderTlsConfig {
                insecure: tls.insecure,
                server_name: tls.server_name,
                cert_file: tls.cert_file,
                key_file: tls.key_file,
                ca_file: tls.ca_file,
                cert_data: tls.cert_data.as_deref().map(decode_base64).transpose()?,
                key_data: tls.key_data.as_deref().map(decode_base64).transpose()?,
                ca_data: tls.ca_data.as_deref().map(decode_base64).transpose()?,
            })
        }).transpose()?;
        out.push(ExtenderConfig {
            url_prefix: r.url_prefix,
            filter_verb: r.filter_verb,
            prioritize_verb: r.prioritize_verb,
            preempt_verb: r.preempt_verb,
            bind_verb: r.bind_verb,
            weight: r.weight,
            node_cache_capable: r.node_cache_capable,
            ignorable: r.ignorable,
            http_timeout: timeout,
            enable_https: r.enable_https,
            tls_config,
            managed_resources: r.managed_resources.iter().map(|m| m.name.clone()).collect(),
            ignored_by_scheduler: r
                .managed_resources
                .into_iter()
                .filter(|m| m.ignored_by_scheduler)
                .map(|m| m.name)
                .collect(),
        });
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────────
// The wire types, matching upstream's `k8s.io/kube-scheduler/extender/v1`
// ─────────────────────────────────────────────────────────────────────────

// These names are intentionally PascalCase. The authoritative upstream Go
// structs carry no json tags, so encoding/json uses their exported field
// names verbatim. A lowercase fake once made this implementation and its e2e
// test agree with each other while both were incompatible with real clients.

#[derive(Serialize)]
struct ExtenderArgs {
    #[serde(rename = "Pod")]
    pod: Pod,
    #[serde(rename = "Nodes", skip_serializing_if = "Option::is_none")]
    nodes: Option<NodeListArg>,
    #[serde(rename = "NodeNames", skip_serializing_if = "Option::is_none")]
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
struct ExtenderFilterResult {
    #[serde(rename = "Nodes", default)]
    nodes: Option<NodeListArg>,
    #[serde(rename = "NodeNames", default)]
    node_names: Option<Vec<String>>,
    #[serde(rename = "FailedNodes", default)]
    failed_nodes: BTreeMap<String, String>,
    #[serde(rename = "FailedAndUnresolvableNodes", default)]
    failed_and_unresolvable_nodes: BTreeMap<String, String>,
    #[serde(rename = "Error", default)]
    error: String,
}

#[derive(Deserialize, Debug)]
struct HostPriority {
    #[serde(rename = "Host")]
    host: String,
    #[serde(rename = "Score")]
    score: i64,
}

#[derive(Serialize)]
struct ExtenderBindingArgs<'a> {
    #[serde(rename = "PodName")]
    pod_name: &'a str,
    #[serde(rename = "PodNamespace")]
    pod_namespace: &'a str,
    #[serde(rename = "PodUID")]
    pod_uid: &'a str,
    #[serde(rename = "Node")]
    node: &'a str,
}

#[derive(Deserialize)]
struct ExtenderBindingResult {
    #[serde(rename = "Error", default)]
    error: String,
}

#[derive(Serialize)]
struct WireVictims {
    #[serde(rename = "Pods")]
    pods: Vec<Pod>,
    #[serde(rename = "NumPDBViolations")]
    num_pdb_violations: i64,
}

#[derive(Serialize, Deserialize)]
struct WireMetaPod {
    #[serde(rename = "UID")]
    uid: String,
}

#[derive(Serialize, Deserialize)]
struct WireMetaVictims {
    #[serde(rename = "Pods", default)]
    pods: Vec<WireMetaPod>,
    #[serde(rename = "NumPDBViolations")]
    num_pdb_violations: i64,
}

#[derive(Serialize)]
struct ExtenderPreemptionArgs {
    #[serde(rename = "Pod")]
    pod: Pod,
    #[serde(rename = "NodeNameToVictims", skip_serializing_if = "Option::is_none")]
    node_name_to_victims: Option<BTreeMap<String, WireVictims>>,
    #[serde(rename = "NodeNameToMetaVictims", skip_serializing_if = "Option::is_none")]
    node_name_to_meta_victims: Option<BTreeMap<String, WireMetaVictims>>,
}

#[derive(Deserialize)]
struct ExtenderPreemptionResult {
    #[serde(rename = "NodeNameToMetaVictims", default)]
    node_name_to_meta_victims: BTreeMap<String, WireMetaVictims>,
}

/// One victim set returned by an extender, resolved back to scheduler keys.
pub struct ExtenderVictims {
    pub node: String,
    pub pod_keys: Vec<String>,
    pub pdb_violations: usize,
}

/// The outcome of one extender's Filter call: which node names survived,
/// and which were rejected with a reason (unresolvable ones separated, the
/// same distinction `framework::status::Code` makes for plugins — an
/// unresolvable rejection is not a preemption candidate).
pub struct FilterOutcome {
    pub passed: Vec<String>,
    /// Non-cache-capable extenders may return full replacement Node objects.
    /// Upstream builds fresh NodeInfos from those objects rather than looking
    /// their names back up in the scheduler snapshot.
    pub replacement_nodes: Option<Vec<Node>>,
    pub failed: BTreeMap<String, String>,
    pub failed_unresolvable: BTreeMap<String, String>,
}

fn pod_to_api(pod: &PodInfo) -> Pod {
    if let Some(api) = &pod.api_object {
        return (**api).clone();
    }
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
    if let Some(api) = &node.api_object {
        return (**api).clone();
    }
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
    /// Normally identical to `config.url_prefix`. With an explicit TLS
    /// serverName the URL host is rewritten so rustls uses that name for SNI
    /// and certificate verification, while the resolver and Host header keep
    /// traffic pointed at the configured endpoint.
    request_url_prefix: String,
    original_host_header: Option<String>,
}

impl Extender {
    pub fn new(config: ExtenderConfig) -> anyhow::Result<Self> {
        let mut builder = reqwest::Client::builder().timeout(config.http_timeout);
        let mut request_url_prefix = config.url_prefix.clone();
        let mut original_host_header = None;
        if let Some(tls) = &config.tls_config {
            if !tls.server_name.is_empty() {
                let mut url = reqwest::Url::parse(&config.url_prefix).map_err(|error| {
                    anyhow::anyhow!("parsing extender urlPrefix {:?}: {error}", config.url_prefix)
                })?;
                let endpoint_host = url.host_str().ok_or_else(|| {
                    anyhow::anyhow!("extender urlPrefix {:?} has no host", config.url_prefix)
                })?;
                let port = url.port_or_known_default().ok_or_else(|| {
                    anyhow::anyhow!(
                        "extender urlPrefix {:?} has no port and its scheme has no default",
                        config.url_prefix
                    )
                })?;
                let addresses: Vec<SocketAddr> = match endpoint_host.parse::<IpAddr>() {
                    Ok(ip) => vec![SocketAddr::new(ip, port)],
                    Err(_) => (endpoint_host, port)
                        .to_socket_addrs()
                        .map_err(|error| {
                            anyhow::anyhow!(
                                "resolving extender endpoint {endpoint_host:?}:{port}: {error}"
                            )
                        })?
                        .collect(),
                };
                if addresses.is_empty() {
                    anyhow::bail!(
                        "extender endpoint {endpoint_host:?}:{port} resolved to no addresses"
                    );
                }
                builder = builder.resolve_to_addrs(&tls.server_name, &addresses);

                let host_for_header = match endpoint_host.parse::<IpAddr>() {
                    Ok(IpAddr::V6(_)) => format!("[{endpoint_host}]"),
                    _ => endpoint_host.to_string(),
                };
                original_host_header = Some(match url.port() {
                    Some(explicit) => format!("{host_for_header}:{explicit}"),
                    None => host_for_header,
                });
                url.set_host(Some(&tls.server_name)).map_err(|_| {
                    anyhow::anyhow!(
                        "invalid extender tlsConfig.serverName {:?}",
                        tls.server_name
                    )
                })?;
                request_url_prefix = url.to_string();
            }
            let has_ca = tls.ca_data.as_ref().is_some_and(|data| !data.is_empty())
                || !tls.ca_file.is_empty();
            if config.enable_https && !has_ca && !tls.insecure {
                anyhow::bail!(
                    "HTTPS extender {:?} requires tlsConfig.caData/caFile or tlsConfig.insecure=true",
                    config.url_prefix
                );
            }
            builder = builder.danger_accept_invalid_certs(tls.insecure);
            let ca = if let Some(data) = tls.ca_data.as_ref().filter(|data| !data.is_empty()) {
                Some(data.clone())
            } else if !tls.ca_file.is_empty() {
                Some(std::fs::read(&tls.ca_file).map_err(|error| {
                    anyhow::anyhow!("reading extender CA file {:?}: {error}", tls.ca_file)
                })?)
            } else {
                None
            };
            if let Some(ca) = ca {
                let certificate = reqwest::Certificate::from_pem(&ca)
                    .map_err(|error| anyhow::anyhow!("parsing extender CA PEM: {error}"))?;
                builder = builder.add_root_certificate(certificate);
            }

            let cert = if let Some(data) = tls.cert_data.as_ref().filter(|data| !data.is_empty()) {
                Some(data.clone())
            } else if !tls.cert_file.is_empty() {
                Some(std::fs::read(&tls.cert_file).map_err(|error| {
                    anyhow::anyhow!("reading extender client certificate {:?}: {error}", tls.cert_file)
                })?)
            } else {
                None
            };
            let key = if let Some(data) = tls.key_data.as_ref().filter(|data| !data.is_empty()) {
                Some(data.clone())
            } else if !tls.key_file.is_empty() {
                Some(std::fs::read(&tls.key_file).map_err(|error| {
                    anyhow::anyhow!("reading extender client key {:?}: {error}", tls.key_file)
                })?)
            } else {
                None
            };
            match (cert, key) {
                (Some(mut cert), Some(key)) => {
                    cert.push(b'\n');
                    cert.extend_from_slice(&key);
                    let identity = reqwest::Identity::from_pem(&cert)
                        .map_err(|error| anyhow::anyhow!("parsing extender client certificate/key PEM: {error}"))?;
                    builder = builder.identity(identity);
                }
                (None, None) => {}
                _ => anyhow::bail!("extender TLS client authentication requires both certificate and key"),
            }
        }
        let client = builder
            .build()
            .map_err(|e| anyhow::anyhow!("building HTTP client for extender {:?}: {e}", config.url_prefix))?;
        Ok(Self { config, client, request_url_prefix, original_host_header })
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

        let (passed, replacement_nodes) = if self.config.node_cache_capable {
            if let Some(names) = result.node_names {
                let supplied: std::collections::HashSet<&str> =
                    nodes.iter().map(|node| node.name.as_str()).collect();
                if let Some(name) = names.iter().find(|name| !supplied.contains(name.as_str())) {
                    anyhow::bail!(
                        "extender {:?} claims filtered node {:?}, which was not in its input node list",
                        self.config.url_prefix,
                        name,
                    );
                }
                (names, None)
            } else if let Some(list) = result.nodes {
                let names = list
                    .items
                    .iter()
                    .filter_map(|node| node.metadata.name.clone())
                    .collect();
                (names, Some(list.items))
            } else {
                (Vec::new(), None)
            }
        } else if let Some(list) = result.nodes {
            let names = list
                .items
                .iter()
                .filter_map(|node| node.metadata.name.clone())
                .collect();
            (names, Some(list.items))
        } else {
            // Neither field set: verified against upstream's real
            // HTTPExtender.Filter (pkg/scheduler/extender.go) — `nodeResult`
            // there stays its zero value (nil/empty) in this case, so an
            // extender that reports failures without also echoing back
            // which nodes passed is read as "none of them did", not "all of
            // them did". Silently defaulting to "everyone passed" here
            // would make a `FailedNodes`-only response a no-op.
            (Vec::new(), None)
        };

        Ok(Some(FilterOutcome {
            passed,
            replacement_nodes,
            failed: result.failed_nodes,
            failed_unresolvable: result.failed_and_unresolvable_nodes,
        }))
    }

    /// Ask this extender to score `nodes`. `None` means not configured for
    /// Prioritize. Verified against upstream's real combining formula
    /// (`pkg/scheduler/schedule_one.go`'s `prioritizeNodes`):
    /// `score * weight * (MaxNodeScore / MaxExtenderPriority)`, i.e.
    /// `score * weight * 10` — an extender's own `[0, 10]` scale is
    /// rescaled onto the plugins' `[0, 100]` one before being added into
    /// the combined total, it is not added in unscaled.
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
        Ok(Some(
            result
                .into_iter()
                .map(|h| {
                    let score = h
                        .score
                        .saturating_mul(self.config.weight)
                        .saturating_mul(MAX_NODE_SCORE / MAX_EXTENDER_PRIORITY);
                    (h.host, score)
                })
                .collect(),
        ))
    }

    /// Let the extender refine preemption candidates. The request shape
    /// depends on `nodeCacheCapable`, but the response is always UID-only
    /// `NodeNameToMetaVictims`, matching extender/v1.
    pub async fn process_preemption(
        &self,
        pod: &PodInfo,
        candidates: &[Candidate],
        snapshot: &Snapshot,
    ) -> anyhow::Result<Option<Vec<ExtenderVictims>>> {
        let Some(verb) = &self.config.preempt_verb else { return Ok(None) };

        let mut full = BTreeMap::new();
        let mut meta = BTreeMap::new();
        for candidate in candidates {
            let node = snapshot
                .node(&candidate.node)
                .ok_or_else(|| anyhow::anyhow!("preemption candidate node {:?} disappeared from the snapshot", candidate.node))?;
            let victims: Vec<&PodInfo> = node
                .pods
                .iter()
                .filter(|victim| candidate.victims.pods.contains(&victim.key()))
                .map(|victim| victim.as_ref())
                .collect();
            if self.config.node_cache_capable {
                meta.insert(
                    candidate.node.clone(),
                    WireMetaVictims {
                        pods: victims
                            .iter()
                            .map(|victim| WireMetaPod { uid: victim.uid.clone() })
                            .collect(),
                        num_pdb_violations: candidate.victims.pdb_violations as i64,
                    },
                );
            } else {
                full.insert(
                    candidate.node.clone(),
                    WireVictims {
                        pods: victims.into_iter().map(pod_to_api).collect(),
                        num_pdb_violations: candidate.victims.pdb_violations as i64,
                    },
                );
            }
        }
        let args = ExtenderPreemptionArgs {
            pod: pod_to_api(pod),
            node_name_to_victims: (!self.config.node_cache_capable).then_some(full),
            node_name_to_meta_victims: self.config.node_cache_capable.then_some(meta),
        };
        let result: ExtenderPreemptionResult = self.post_body(verb, &args).await?;

        let mut resolved = Vec::with_capacity(result.node_name_to_meta_victims.len());
        for (node_name, victims) in result.node_name_to_meta_victims {
            let node = snapshot.node(&node_name).ok_or_else(|| {
                anyhow::anyhow!(
                    "extender {:?} returned preemption victims for unknown node {:?}",
                    self.config.url_prefix,
                    node_name,
                )
            })?;
            let mut pod_keys = Vec::with_capacity(victims.pods.len());
            for meta_pod in victims.pods {
                let victim = node
                    .pods
                    .iter()
                    .find(|candidate| candidate.uid == meta_pod.uid)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "extender {:?} claims to preempt pod UID {:?} on node {:?}, but that pod is not on the node",
                            self.config.url_prefix,
                            meta_pod.uid,
                            node_name,
                        )
                    })?;
                pod_keys.push(victim.key());
            }
            resolved.push(ExtenderVictims {
                node: node_name,
                pod_keys,
                pdb_violations: victims.num_pdb_violations.max(0) as usize,
            });
        }
        Ok(Some(resolved))
    }

    /// Let an interested extender take over Bind. `None` means this extender
    /// has no bindVerb; `Some(())` means the Binding was completed remotely.
    pub async fn bind(&self, pod: &PodInfo, node: &str) -> anyhow::Result<Option<()>> {
        let Some(verb) = &self.config.bind_verb else { return Ok(None) };
        let args = ExtenderBindingArgs {
            pod_name: &pod.name,
            pod_namespace: &pod.namespace,
            pod_uid: &pod.uid,
            node,
        };
        let result: ExtenderBindingResult = self.post_body(verb, &args).await?;
        if !result.error.is_empty() {
            anyhow::bail!("extender {:?} bind error: {}", self.config.url_prefix, result.error);
        }
        Ok(Some(()))
    }

    async fn post<T: serde::de::DeserializeOwned>(&self, verb: &str, args: &ExtenderArgs) -> anyhow::Result<T> {
        self.post_body(verb, args).await
    }

    async fn post_body<T: serde::de::DeserializeOwned>(
        &self,
        verb: &str,
        args: &impl serde::Serialize,
    ) -> anyhow::Result<T> {
        let url = format!("{}/{}", self.request_url_prefix.trim_end_matches('/'), verb);
        let mut request = self.client.post(&url).json(args);
        if let Some(host) = &self.original_host_header {
            request = request.header(reqwest::header::HOST, host);
        }
        let resp = request
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("calling extender {url:?}: {e}"))?;
        // Upstream accepts exactly HTTP 200. A 201/204 response is not an
        // extender result and attempting to decode it would change which
        // failures are ignorable at the caller.
        if resp.status() != reqwest::StatusCode::OK {
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
