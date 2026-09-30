//! Kubernetes API object transfer. Datastore files are distribution-specific;
//! reading and applying resources through the API avoids copying Kine/etcd
//! storage into nodestore.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, ensure, Context, Result};
use base64::Engine;
use k8s_openapi::api::core::v1::{ConfigMap, Namespace, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::{
    api::{
        Api, DeleteParams, DynamicObject, ListParams, LogParams, Patch, PatchParams, PostParams,
        Preconditions,
    },
    config::Kubeconfig,
    discovery::{verbs, ApiResource, Discovery},
    Client,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    detect::{Installation, K3sDatastore},
    request::Distribution,
};

// These object kinds are not durable API inputs for the replacement:
// ComponentStatus is read-only, Events and metrics are observations, Nodes
// are re-registered from the protected scheduling snapshot, and
// VolumeAttachments must be recreated by the destination CSI attacher.
// Leases and endpoint objects need object-level checks because users and
// add-ons can own durable instances of those kinds. CiliumEndpoint,
// CiliumIdentity, and CiliumNode records are Cilium-managed datapath,
// identity-allocation, and IPAM state; Cilium rebuilds them from destination
// Pods and Nodes while declarative Cilium policies remain migratable.
const SKIP_KINDS: &[&str] = &[
    "ComponentStatus",
    "Event",
    "Node",
    "NodeMetrics",
    "PodMetrics",
    "VolumeAttachment",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SkipReason {
    ReadOnlyComponentStatus,
    EphemeralEvents,
    NodeReregistration,
    LiveMetrics,
    CsiReattachment,
    ApiServiceRouting,
    ControllerManagedEndpoints,
    DestinationTrustBundle,
    NodeHeartbeatLease,
    StaticPodMirror,
    ControllerOwnedPod,
    CiliumEndpointReconciliation,
    CiliumIdentityReconciliation,
    CiliumNodeReconciliation,
    NodebootstrapRuntimeRbac,
}

impl SkipReason {
    fn description(self) -> &'static str {
        match self {
            Self::ReadOnlyComponentStatus => {
                "read-only status reported by the source control plane"
            }
            Self::EphemeralEvents => {
                "transient observations; event history is not durable workload state"
            }
            Self::NodeReregistration => {
                "nodes register with the destination and scheduling metadata is restored separately"
            }
            Self::LiveMetrics => "samples are refreshed by the destination metrics provider",
            Self::CsiReattachment => "destination CSI controllers recreate attachment state",
            Self::ApiServiceRouting => {
                "the destination control plane recreates its Kubernetes API routing"
            }
            Self::ControllerManagedEndpoints => {
                "the owning service controller recalculates endpoint state"
            }
            Self::DestinationTrustBundle => {
                "the destination regenerates this control-plane trust bundle from its own PKI"
            }
            Self::NodeHeartbeatLease => {
                "the destination kubelet creates a lease for the re-registered node"
            }
            Self::StaticPodMirror => "the retained static pod manifest recreates its API mirror",
            Self::ControllerOwnedPod => "the durable workload controller recreates this pod",
            Self::CiliumEndpointReconciliation => {
                "Cilium regenerates this pod's datapath identity and endpoint from the destination runtime"
            }
            Self::CiliumIdentityReconciliation => {
                "Cilium reallocates security identities from destination endpoint labels"
            }
            Self::CiliumNodeReconciliation => {
                "Cilium reconciles node addressing and IPAM state for the destination cluster"
            }
            Self::NodebootstrapRuntimeRbac => {
                "nodebootstrap recreates this runtime-owned RBAC policy on the destination"
            }
        }
    }
}

#[derive(Debug, Default)]
struct ResourceExportSummary {
    migrated_objects: usize,
    skipped_objects: BTreeMap<SkipReason, usize>,
    skipped_resource: Option<SkipReason>,
}

const IMPORT_RETRY_ATTEMPTS: u32 = 60;
const IMPORT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(5);
const DISCOVERY_RETRY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const DISCOVERY_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const DISCOVERY_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(5);

async fn discover_apis(client: &Client) -> Result<Discovery> {
    match tokio::time::timeout(
        DISCOVERY_PROBE_TIMEOUT,
        Discovery::new(client.clone()).run_aggregated(),
    )
    .await
    {
        Ok(Ok(discovery)) => Ok(discovery),
        aggregate_result => {
            let aggregate_error = match aggregate_result {
                Ok(Err(error)) => {
                    anyhow::Error::new(error).context("using aggregated Kubernetes API discovery")
                }
                Err(error) => anyhow::Error::new(error)
                    .context("aggregated Kubernetes API discovery exceeded 10 seconds"),
                Ok(Ok(_)) => unreachable!("successful aggregate discovery returned above"),
            };
            eprintln!(
                "nodemigrate: aggregated API discovery unavailable; falling back to per-group discovery: {aggregate_error:#}"
            );
            tokio::time::timeout(
                DISCOVERY_PROBE_TIMEOUT,
                Discovery::new(client.clone()).run(),
            )
            .await
            .context("per-group Kubernetes API discovery exceeded 10 seconds")?
            .context("per-group Kubernetes API discovery failed after aggregate discovery")
        }
    }
}

#[derive(Debug, Clone)]
pub struct KubeApi {
    kubeconfig: PathBuf,
}

async fn wait_for_discovery(client: &Client) -> Result<Discovery> {
    let started = std::time::Instant::now();
    let mut last_error = None;
    let mut attempt = 0;
    while started.elapsed() < DISCOVERY_RETRY_TIMEOUT {
        attempt += 1;
        match discover_apis(client).await {
            Ok(discovery) => return Ok(discovery),
            Err(error) => {
                let error = error.context("discovering destination APIs");
                eprintln!(
                    "nodemigrate: destination API discovery probe {attempt} failed: {error:#}"
                );
                last_error = Some(error);
            }
        }
        let remaining = DISCOVERY_RETRY_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        tokio::time::sleep(DISCOVERY_RETRY_DELAY.min(remaining)).await;
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("destination discovery did not return")))
        .context("waiting for destination API discovery to become ready")
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NodeSchedulingState {
    pub uid: Option<String>,
    pub labels: HashMap<String, String>,
    pub annotations: HashMap<String, String>,
    pub taints: Vec<Value>,
    pub unschedulable: Option<bool>,
}

impl NodeSchedulingState {
    fn from_node(node: &DynamicObject) -> Self {
        let uid = node.metadata.uid.clone();
        let labels = node
            .metadata
            .labels
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let annotations = node
            .metadata
            .annotations
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let taints = node
            .data
            .pointer("/spec/taints")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let unschedulable = node
            .data
            .pointer("/spec/unschedulable")
            .and_then(Value::as_bool);
        Self {
            uid,
            labels,
            annotations,
            taints,
            unschedulable,
        }
    }
}

fn remapped_node_owner_references(
    source_references: &[Value],
    destination_references: &[Value],
    node_name: &str,
    source_uid: &str,
    destination_uid: &str,
) -> Vec<Value> {
    let mut references = destination_references.to_vec();
    for source_reference in source_references {
        let is_migrating_node = source_reference.get("kind").and_then(Value::as_str)
            == Some("Node")
            && source_reference.get("name").and_then(Value::as_str) == Some(node_name)
            && source_reference.get("uid").and_then(Value::as_str) == Some(source_uid);
        if !is_migrating_node {
            continue;
        }
        let mut reference = source_reference.clone();
        reference["uid"] = Value::String(destination_uid.to_owned());
        let already_present = references.iter().any(|existing| {
            existing.get("kind") == reference.get("kind")
                && existing.get("name") == reference.get("name")
                && existing.get("uid") == reference.get("uid")
        });
        if !already_present {
            references.push(reference);
        }
    }
    references
}

fn node_scheduling_patch(state: &NodeSchedulingState) -> Value {
    let mut patch = serde_json::json!({
        "metadata": {
            "labels": &state.labels,
            "annotations": &state.annotations
        },
        "spec": {"taints": &state.taints}
    });
    if let Some(unschedulable) = state.unschedulable {
        patch["spec"]["unschedulable"] = Value::Bool(unschedulable);
    }
    patch
}

fn node_is_ready(node: &DynamicObject) -> bool {
    node.data
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .is_some_and(|conditions| {
            conditions.iter().any(|condition| {
                condition.get("type").and_then(Value::as_str) == Some("Ready")
                    && condition.get("status").and_then(Value::as_str) == Some("True")
            })
        })
}

fn node_is_ready_replacement(node: &DynamicObject, previous_uid: &str) -> bool {
    node.metadata
        .uid
        .as_deref()
        .is_some_and(|uid| uid != previous_uid)
        && node_is_ready(node)
}

fn node_uid_has_been_replaced(node: Option<&DynamicObject>, previous_uid: &str) -> bool {
    node.is_none_or(|node| {
        node.metadata.uid.as_deref().is_some_and(|uid| uid != previous_uid)
    })
}

fn owner_reference_repair_patch(
    current: &Value,
    references: Vec<Value>,
    is_csinode: bool,
) -> Value {
    let mut patch = serde_json::json!({
        "metadata": {"ownerReferences": references}
    });
    if is_csinode {
        // Some API servers validate the required CSINode driver list against
        // the complete patched object. Preserve the current list, or
        // materialize the valid empty list if the source object omitted it.
        let drivers = current
            .pointer("/spec/drivers")
            .filter(|drivers| drivers.is_array())
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        patch["spec"] = serde_json::json!({"drivers": drivers});
    }
    patch
}

impl KubeApi {
    fn source_kubeconfig_path(installation: &Installation) -> PathBuf {
        std::env::var_os("NODEMIGRATE_SOURCE_KUBECONFIG")
            .map(PathBuf::from)
            .or_else(|| {
                installation
                    .cluster
                    .as_ref()
                    .and_then(|cluster| cluster.kubeconfig.clone())
            })
            .or_else(|| std::env::var_os("KUBECONFIG").map(PathBuf::from))
            .unwrap_or_else(|| match installation.distribution {
                crate::request::Distribution::K3s => PathBuf::from("/etc/rancher/k3s/k3s.yaml"),
                crate::request::Distribution::Nodestore => {
                    PathBuf::from("/etc/nodebootstrap/admin.kubeconfig")
                }
                crate::request::Distribution::Kubernetes => {
                    PathBuf::from("/etc/kubernetes/admin.conf")
                }
            })
    }

    pub fn source(installation: &Installation) -> Result<Self> {
        let kubeconfig = Self::source_kubeconfig_path(installation);
        ensure!(
            kubeconfig.is_file(),
            "source kubeconfig {} does not exist",
            kubeconfig.display()
        );
        Ok(Self { kubeconfig })
    }

    pub fn source_api_server_name(installation: &Installation) -> Result<Option<String>> {
        let kubeconfig = Self::source_kubeconfig_path(installation);
        if !kubeconfig.is_file() {
            return Ok(None);
        }
        let api = Self { kubeconfig };
        api.api_server_name().map(Some)
    }

    fn api_server_name(&self) -> Result<String> {
        let contents = fs::read_to_string(&self.kubeconfig)
            .with_context(|| format!("reading Kubernetes config {}", self.kubeconfig.display()))?;
        let config: serde_yaml::Value = serde_yaml::from_str(&contents)
            .with_context(|| format!("parsing Kubernetes config {}", self.kubeconfig.display()))?;
        let current_context = config
            .get("current-context")
            .and_then(serde_yaml::Value::as_str)
            .context("source kubeconfig has no current-context")?;
        let contexts = config
            .get("contexts")
            .and_then(serde_yaml::Value::as_sequence)
            .context("source kubeconfig has no contexts list")?;
        let cluster_name = contexts
            .iter()
            .find(|context| {
                context.get("name").and_then(serde_yaml::Value::as_str) == Some(current_context)
            })
            .and_then(|context| context.get("context"))
            .and_then(|context| context.get("cluster"))
            .and_then(serde_yaml::Value::as_str)
            .context("source kubeconfig current context has no cluster")?;
        let clusters = config
            .get("clusters")
            .and_then(serde_yaml::Value::as_sequence)
            .context("source kubeconfig has no clusters list")?;
        let server = clusters
            .iter()
            .find(|cluster| {
                cluster.get("name").and_then(serde_yaml::Value::as_str) == Some(cluster_name)
            })
            .and_then(|cluster| cluster.get("cluster"))
            .and_then(|cluster| cluster.get("server"))
            .and_then(serde_yaml::Value::as_str)
            .context("source kubeconfig current cluster has no API server URL")?;
        let authority = server
            .split_once("://")
            .map_or(server, |(_, rest)| rest)
            .split('/')
            .next()
            .unwrap_or_default();
        let host = if let Some(bracketed) = authority.strip_prefix('[') {
            bracketed
                .split_once(']')
                .map(|(host, _)| host)
                .context("source API server URL has an invalid IPv6 authority")?
        } else {
            authority
                .rsplit_once(':')
                .filter(|(_, port)| port.bytes().all(|byte| byte.is_ascii_digit()))
                .map_or(authority, |(host, _)| host)
        };
        ensure!(!host.is_empty(), "source API server URL has no host");
        Ok(host.to_owned())
    }

    pub fn destination(distribution: crate::request::Distribution) -> Result<Self> {
        if let Some(path) = std::env::var_os("NODEMIGRATE_DESTINATION_KUBECONFIG") {
            return Ok(Self {
                kubeconfig: PathBuf::from(path),
            });
        }
        let directory = std::env::var_os("NODEBOOTSTRAP_KUBECONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/etc/nodebootstrap"));
        let kubeconfig = match distribution {
            crate::request::Distribution::K3s => PathBuf::from("/etc/rancher/k3s/k3s.yaml"),
            crate::request::Distribution::Kubernetes => PathBuf::from("/etc/kubernetes/admin.conf"),
            crate::request::Distribution::Nodestore => directory.join("admin.kubeconfig"),
        };
        Ok(Self { kubeconfig })
    }

    fn connected(&self) -> Result<(tokio::runtime::Runtime, Client)> {
        let kubeconfig = Kubeconfig::read_from(&self.kubeconfig)
            .with_context(|| format!("reading Kubernetes config {}", self.kubeconfig.display()))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building Kubernetes API runtime")?;
        let client = {
            let _runtime_context = runtime.enter();
            Client::try_from(kubeconfig).context("building Kubernetes API client")?
        };
        Ok((runtime, client))
    }

    pub fn ready(&self) -> Result<()> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                // Readiness must not discover every installed API group: clusters
                // with many CRDs can take longer than the probe deadline to
                // enumerate discovery endpoints even when the core API is ready.
                let api: Api<Namespace> = Api::all(client);
                api.list(&ListParams::default())
                    .await
                    .context("checking Kubernetes API readiness")?;
                Ok(())
            })
            .await
            .context("Kubernetes API readiness probe exceeded 10 seconds")?
        })
    }

    /// Read Cilium's cluster-wide Service datapath setting from the ConfigMap
    /// that its agent consumes. Cilium defaults this setting to false when
    /// the ConfigMap omits it.
    pub fn cilium_kube_proxy_replacement(&self) -> Result<bool> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let config_maps: Api<ConfigMap> = Api::namespaced(client, "kube-system");
            let config = config_maps
                .get_opt("cilium-config")
                .await
                .context("reading kube-system/cilium-config Service datapath mode")?;
            parse_cilium_kube_proxy_replacement(
                config
                    .as_ref()
                    .and_then(|config| config.data.as_ref())
                    .and_then(|data| data.get("kube-proxy-replacement"))
                    .map(String::as_str),
            )
        })
    }

    /// Whether the source API still owns Service routing through kube-proxy.
    pub fn kube_proxy_daemonset_present(&self) -> Result<bool> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let discovery = wait_for_discovery(&client).await?;
            let (resource, capabilities) = find_resource(&discovery, "DaemonSet", "apps/v1")
                .context("Kubernetes API does not expose apps/v1 DaemonSet")?;
            ensure!(
                capabilities.supports_operation(verbs::GET),
                "Kubernetes API cannot inspect kube-proxy DaemonSet"
            );
            let api: Api<DynamicObject> =
                Api::namespaced_with(client, "kube-system", &resource);
            Ok(api.get_opt("kube-proxy").await?.is_some())
        })
    }

    pub fn node_count(&self) -> Result<usize> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let discovery = wait_for_discovery(&client).await?;
            let (resource, capabilities) = find_resource(&discovery, "Node", "v1")
                .context("Kubernetes API does not expose Node")?;
            ensure!(
                capabilities.supports_operation(verbs::LIST),
                "Kubernetes API cannot list nodes"
            );
            let api: Api<DynamicObject> = Api::all_with(client, &resource);
            let nodes = api
                .list(&ListParams::default())
                .await
                .context("listing cluster nodes")?;
            Ok(nodes.items.len())
        })
    }

    pub fn node_ready(&self, name: &str) -> Result<bool> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let discovery = wait_for_discovery(&client).await?;
            let (resource, capabilities) = find_resource(&discovery, "Node", "v1")
                .context("Kubernetes API does not expose Node")?;
            ensure!(
                capabilities.supports_operation(verbs::GET),
                "Kubernetes API cannot read nodes"
            );
            let api: Api<DynamicObject> = Api::all_with(client, &resource);
            let Some(node) = api
                .get_opt(name)
                .await
                .context("checking migrated node readiness")?
            else {
                return Ok(false);
            };
            Ok(node_is_ready(&node))
        })
    }

    pub fn replacement_node_ready(&self, name: &str, previous_uid: &str) -> Result<bool> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let discovery = wait_for_discovery(&client).await?;
            let (resource, capabilities) = find_resource(&discovery, "Node", "v1")
                .context("Kubernetes API does not expose Node")?;
            ensure!(
                capabilities.supports_operation(verbs::GET),
                "Kubernetes API cannot read nodes"
            );
            let api: Api<DynamicObject> = Api::all_with(client, &resource);
            let Some(node) = api
                .get_opt(name)
                .await
                .context("checking replacement node registration")?
            else {
                return Ok(false);
            };
            Ok(node_is_ready_replacement(&node, previous_uid))
        })
    }

    pub fn node_exists(&self, name: &str) -> Result<bool> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let discovery = wait_for_discovery(&client).await?;
            let (resource, capabilities) = find_resource(&discovery, "Node", "v1")
                .context("Kubernetes API does not expose Node")?;
            ensure!(
                capabilities.supports_operation(verbs::GET),
                "Kubernetes API cannot read nodes"
            );
            let api: Api<DynamicObject> = Api::all_with(client, &resource);
            Ok(api
                .get_opt(name)
                .await
                .context("checking for an existing destination node")?
                .is_some())
        })
    }

    pub fn node_scheduling_state(&self, name: &str) -> Result<Option<NodeSchedulingState>> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let discovery = wait_for_discovery(&client).await?;
            let (resource, capabilities) = find_resource(&discovery, "Node", "v1")
                .context("Kubernetes API does not expose Node")?;
            ensure!(
                capabilities.supports_operation(verbs::GET),
                "Kubernetes API cannot read nodes"
            );
            let api: Api<DynamicObject> = Api::all_with(client, &resource);
            Ok(api
                .get_opt(name)
                .await
                .with_context(|| format!("reading scheduling state for node {name}"))?
                .as_ref()
                .map(NodeSchedulingState::from_node))
        })
    }

    pub fn restore_node_scheduling_state(
        &self,
        name: &str,
        state: &NodeSchedulingState,
    ) -> Result<()> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let discovery = wait_for_discovery(&client).await?;
            let (resource, capabilities) = find_resource(&discovery, "Node", "v1")
                .context("Kubernetes API does not expose Node")?;
            ensure!(
                capabilities.supports_operation(verbs::PATCH),
                "Kubernetes API cannot restore node scheduling state"
            );
            let api: Api<DynamicObject> = Api::all_with(client, &resource);
            let patch = node_scheduling_patch(state);
            api.patch(name, &PatchParams::default(), &Patch::Merge(&patch))
                .await
                .with_context(|| format!("restoring scheduling state for node {name}"))?;
            Ok(())
        })
    }

    /// Restore owner references to a source Node after the replacement Node is
    /// registered. Nodes are regenerated rather than imported, so their child
    /// objects cannot be safely rebound until the destination Node has a new
    /// UID. The protected export retains the source UID for this repair.
    pub fn restore_node_owner_references(&self, export: &Export, node_name: &str) -> Result<usize> {
        let Some(source_uid) = export
            .node_state(node_name)
            .and_then(|state| state.uid.as_deref())
        else {
            return Ok(0);
        };
        let source_uid = source_uid.to_owned();
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let discovery = wait_for_discovery(&client).await?;
            let (node_resource, node_capabilities) = find_resource(&discovery, "Node", "v1")
                .context("destination Kubernetes API does not expose Node")?;
            ensure!(
                node_capabilities.supports_operation(verbs::GET),
                "destination Kubernetes API cannot read the replacement Node"
            );
            let nodes: Api<DynamicObject> = Api::all_with(client.clone(), &node_resource);
            let node = nodes
                .get_opt(node_name)
                .await
                .with_context(|| format!("reading replacement Node {node_name}"))?
                .with_context(|| format!("replacement Node {node_name} is not registered"))?;
            let destination_uid = node.metadata.uid.context("replacement Node has no UID")?;

            let mut repaired = 0;
            for exported in &export.objects {
                let source: Value = serde_json::from_slice(
                    &fs::read(&exported.path)
                        .with_context(|| format!("reading {}", exported.path.display()))?,
                )
                .context("decoding protected object while restoring Node owner references")?;
                let source_references = source
                    .pointer("/metadata/ownerReferences")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                if !source_references.iter().any(|reference| {
                    reference.get("kind").and_then(Value::as_str) == Some("Node")
                        && reference.get("name").and_then(Value::as_str) == Some(node_name)
                        && reference.get("uid").and_then(Value::as_str) == Some(source_uid.as_str())
                }) {
                    continue;
                }
                let api_version = source
                    .get("apiVersion")
                    .and_then(Value::as_str)
                    .context("protected object has no apiVersion")?;
                let kind = source
                    .get("kind")
                    .and_then(Value::as_str)
                    .context("protected object has no kind")?;
                let (resource, capabilities) = find_resource(&discovery, kind, api_version)
                    .or_else(|| find_compatible_resource(&discovery, kind, api_version))
                    .with_context(|| format!("destination does not expose {api_version}/{kind}"))?;
                ensure!(
                    capabilities.supports_operation(verbs::GET)
                        && capabilities.supports_operation(verbs::PATCH),
                    "destination cannot repair owner references on {kind} objects"
                );
                let name = source
                    .pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .context("protected object has no metadata.name")?;
                let api: Api<DynamicObject> = if let Some(namespace) = source
                    .pointer("/metadata/namespace")
                    .and_then(Value::as_str)
                {
                    Api::namespaced_with(client.clone(), namespace, &resource)
                } else {
                    Api::all_with(client.clone(), &resource)
                };
                let mut completed = false;
                let csinode_deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_secs(60);
                let is_csinode = kind == "CSINode" && api_version.starts_with("storage.k8s.io/");
                let mut conflict_retries = 0;
                loop {
                    let current = loop {
                        if let Some(current) = api
                            .get_opt(name)
                            .await
                            .with_context(|| format!("reading migrated {kind} {name}"))?
                        {
                            break current;
                        }
                        // The kubelet can mark the replacement Node Ready
                        // before the CSI node-driver registrar recreates its
                        // CSINode object. Keep the source owner reference
                        // repair bounded while that node-scoped object is
                        // being registered; missing other resource kinds is
                        // still an immediate migration error.
                        if !is_csinode {
                            bail!("migrated {kind} {name} is missing");
                        }
                        if tokio::time::Instant::now() >= csinode_deadline {
                            bail!(
                                "migrated CSINode {name} did not appear while restoring Node owner references"
                            );
                        }
                        eprintln!(
                            "nodemigrate: waiting for replacement-node CSINode {name} before repairing its owner reference"
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    };
                    let current: Value = serde_json::to_value(current)
                        .context("serializing migrated object owner references")?;
                    let existing_references = current
                        .pointer("/metadata/ownerReferences")
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or_default();
                    let references = remapped_node_owner_references(
                        source_references,
                        existing_references,
                        node_name,
                        &source_uid,
                        &destination_uid,
                    );
                    if references.len() == existing_references.len()
                        && references
                            .iter()
                            .zip(existing_references)
                            .all(|(left, right)| left == right)
                    {
                        completed = true;
                        break;
                    }
                    let resource_version = current
                        .pointer("/metadata/resourceVersion")
                        .and_then(Value::as_str)
                        .context("migrated object has no resourceVersion")?;
                    let mut patch = owner_reference_repair_patch(&current, references, is_csinode);
                    patch["metadata"]["resourceVersion"] =
                        Value::String(resource_version.to_owned());
                    match api
                        .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
                        .await
                    {
                        Ok(_) => {
                            repaired += 1;
                            completed = true;
                            break;
                        }
                        Err(kube::Error::Api(response))
                            if response.code == 409 && conflict_retries < 2 =>
                        {
                            // Re-read and recompute the complete reference list.
                            conflict_retries += 1;
                        }
                        Err(kube::Error::Api(response))
                            if response.code == 404 && is_csinode =>
                        {
                            // A CSI registrar can delete and recreate CSINode
                            // while this repair is in flight. Re-read it under
                            // the same bounded registration deadline used above.
                            if tokio::time::Instant::now() >= csinode_deadline {
                                bail!(
                                    "migrated CSINode {name} kept disappearing while restoring Node owner references"
                                );
                            }
                            eprintln!(
                                "nodemigrate: replacement-node CSINode {name} disappeared during owner-reference repair; retrying"
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        }
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!("repairing Node owner reference on {kind} {name}")
                            });
                        }
                    }
                }
                ensure!(
                    completed,
                    "migrated {kind} {name} kept changing during Node reference repair"
                );
            }
            Ok(repaired)
        })
    }

    pub fn delete_node(&self, name: &str, expected_uid: &str) -> Result<()> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let discovery = wait_for_discovery(&client).await?;
            let (resource, capabilities) = find_resource(&discovery, "Node", "v1")
                .context("Kubernetes API does not expose Node")?;
            ensure!(
                capabilities.supports_operation(verbs::DELETE),
                "Kubernetes API cannot delete nodes"
            );
            let api: Api<DynamicObject> = Api::all_with(client, &resource);
            api.delete(
                name,
                &DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: Some(expected_uid.to_owned()),
                        resource_version: None,
                    }),
                    ..Default::default()
                },
            )
            .await
            .with_context(|| format!("removing stale destination node {name}"))?;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                let current = api
                    .get_opt(name)
                    .await
                    .with_context(|| format!("waiting for stale destination node {name} deletion"))?;
                if node_uid_has_been_replaced(current.as_ref(), expected_uid) {
                    return Ok(());
                }
                if tokio::time::Instant::now() >= deadline {
                    bail!("stale destination node {name} with UID {expected_uid} remained after deletion");
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        })
    }

    /// Reset this node's Cilium host state by briefly enabling Cilium's
    /// documented cleanup init and replacing its agent Pod. The cleanup flag
    /// lives in a cluster-wide ConfigMap, but changing it does not roll the
    /// DaemonSet; restore the original value as soon as the local init starts
    /// so later Pod restarts on peer nodes do not erase their datapaths.
    pub fn reset_cilium_agent_state(&self, node_name: &str) -> Result<()> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let pods: Api<Pod> = Api::namespaced(client.clone(), "kube-system");
            let config: Api<ConfigMap> = Api::namespaced(client, "kube-system");
            // A joining node is registered before its Cilium DaemonSet Pod is
            // necessarily scheduled. Do not roll back a healthy node join just
            // because the controller has not observed the new Node yet.
            let schedule_deadline =
                tokio::time::Instant::now() + std::time::Duration::from_secs(300);
            let pod = loop {
                let current_pods = pods
                    .list(&ListParams::default().labels("k8s-app=cilium"))
                    .await
                    .context("listing Cilium agent Pods")?;
                if let Some(pod) = current_pods.items.into_iter().find(|pod| {
                    pod.spec
                        .as_ref()
                        .and_then(|spec| spec.node_name.as_deref())
                        == Some(node_name)
                        && pod
                            .metadata
                            .owner_references
                            .as_ref()
                            .is_some_and(|owners| owners.iter().any(|owner| owner.kind == "DaemonSet"))
                }) {
                    break pod;
                }
                if tokio::time::Instant::now() >= schedule_deadline {
                    bail!("no DaemonSet-managed Cilium agent Pod was scheduled on node {node_name} within 300 seconds");
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            };
            let name = pod
                .metadata
                .name
                .as_deref()
                .context("Cilium agent Pod has no name")?
                .to_owned();
            let uid = pod
                .metadata
                .uid
                .as_deref()
                .context("Cilium agent Pod has no UID")?
                .to_owned();
            let has_clean_state_init = pod
                .spec
                .as_ref()
                .and_then(|spec| spec.init_containers.as_ref())
                .is_some_and(|containers| {
                    containers
                        .iter()
                        .any(|container| container.name == "clean-cilium-state")
                });
            ensure!(
                has_clean_state_init,
                "Cilium agent Pod {name} has no clean-cilium-state init container; refusing a cross-cluster handoff with shared host datapath state"
            );

            let original_config = config
                .get("cilium-config")
                .await
                .context("reading kube-system/cilium-config before host-state reset")?;
            let original_clean_state = original_config
                .data
                .as_ref()
                .and_then(|data| data.get("clean-cilium-state"))
                .cloned();
            let mut cleanup_flag_changed = false;

            let operation = async {
                if original_clean_state.as_deref() != Some("true") {
                    let resource_version = original_config
                        .metadata
                        .resource_version
                        .as_deref()
                        .context("Cilium ConfigMap has no resourceVersion")?;
                    let patch = serde_json::json!({
                        "metadata": {"resourceVersion": resource_version},
                        "data": {"clean-cilium-state": "true"}
                    });
                    config
                        .patch(
                            "cilium-config",
                            &PatchParams::default(),
                            &Patch::Merge(&patch),
                        )
                        .await
                        .context("temporarily enabling Cilium's per-node state cleanup")?;
                    cleanup_flag_changed = true;
                }

                pods.delete(
                    &name,
                    &DeleteParams {
                        grace_period_seconds: Some(0),
                        preconditions: Some(Preconditions {
                            uid: Some(uid.clone()),
                            resource_version: None,
                        }),
                        ..Default::default()
                    },
                )
                .await
                .with_context(|| format!("recreating Cilium agent Pod {name} for state cleanup"))?;
                eprintln!(
                    "nodemigrate: deleted prior Cilium agent Pod {name}; waiting for its replacement on node {node_name}"
                );

                let deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_secs(300);
                let mut cleanup_init_exit_code = None;
                let mut cleanup_flag_restored = !cleanup_flag_changed;
                let mut ready_since = None;
                let mut last_failed_init_restart_count = None;
                let mut cleanup_init_failure_details = None;
                let mut replacement_pod_logged = false;
                loop {
                    let current_pods = pods
                        .list(&ListParams::default().labels("k8s-app=cilium"))
                        .await
                        .with_context(|| format!("waiting for replacement Cilium agent on {node_name}"))?;
                    let replacement = current_pods.items.into_iter().find(|pod| {
                        pod.spec
                            .as_ref()
                            .and_then(|spec| spec.node_name.as_deref())
                            == Some(node_name)
                            && pod
                                .metadata
                                .owner_references
                                .as_ref()
                                .is_some_and(|owners| {
                                    owners.iter().any(|owner| owner.kind == "DaemonSet")
                                })
                            && pod.metadata.uid.as_deref().is_some_and(|pod_uid| pod_uid != uid)
                    });
                    if let Some(current) = replacement {
                        if !replacement_pod_logged {
                            eprintln!(
                                "nodemigrate: replacement Cilium agent Pod {} appeared on node {node_name}",
                                current.metadata.name.as_deref().unwrap_or("<unnamed>")
                            );
                            replacement_pod_logged = true;
                        }
                        let init_state = current
                            .status
                            .as_ref()
                            .and_then(|status| status.init_container_statuses.as_ref())
                            .and_then(|statuses| {
                                statuses.iter().find(|status| status.name == "clean-cilium-state")
                            });
                        if let Some(init_state) = init_state {
                            if let Some(terminated) = init_state
                                .state
                                .as_ref()
                                .and_then(|state| state.terminated.as_ref())
                            {
                                if terminated.exit_code == 0 {
                                    cleanup_init_exit_code = Some(0);
                                } else if last_failed_init_restart_count
                                    != Some(init_state.restart_count)
                                {
                                    last_failed_init_restart_count =
                                        Some(init_state.restart_count);
                                    let params = LogParams {
                                        container: Some("clean-cilium-state".to_owned()),
                                        tail_lines: Some(80),
                                        ..Default::default()
                                    };
                                    let init_logs = match tokio::time::timeout(
                                        std::time::Duration::from_secs(10),
                                        pods.logs(
                                            current.metadata.name.as_deref().unwrap_or(&name),
                                            &params,
                                        ),
                                    )
                                    .await
                                    {
                                        Ok(Ok(logs)) => logs,
                                        Ok(Err(error)) => {
                                            format!("unable to read init logs: {error:#}")
                                        }
                                        Err(_) => "reading init logs timed out".to_owned(),
                                    };
                                    cleanup_init_failure_details = Some(format!(
                                        "attempt={}, reason={}, message={:?}, logs={:?}",
                                        init_state.restart_count + 1,
                                        terminated.reason.as_deref().unwrap_or("<unknown>"),
                                        terminated.message,
                                        init_logs.trim()
                                    ));
                                }
                            }
                            let init_started = init_state.state.as_ref().is_some_and(|state| {
                                state.running.is_some() || state.terminated.is_some()
                            });
                            if init_started && !cleanup_flag_restored {
                                restore_cilium_clean_state_flag(&config, original_clean_state.as_deref())
                                    .await?;
                                cleanup_flag_restored = true;
                                cleanup_flag_changed = false;
                            }
                        }
                        if cleanup_init_exit_code == Some(0) {
                            let ready = current.status.as_ref().is_some_and(|status| {
                                status.conditions.as_ref().is_some_and(|conditions| {
                                    conditions.iter().any(|condition| {
                                        condition.type_ == "Ready" && condition.status == "True"
                                    })
                                })
                            });
                            if ready {
                                let now = tokio::time::Instant::now();
                                let stable_since = ready_since.get_or_insert(now);
                                if now.duration_since(*stable_since)
                                    < std::time::Duration::from_secs(10)
                                {
                                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                                    continue;
                                }
                                eprintln!(
                                    "nodemigrate: rebuilt Cilium host state on node {node_name}; replacement agent remained Ready for 10 seconds after clean-cilium-state"
                                );
                                return Ok(());
                            }
                            ready_since = None;
                        }
                    } else {
                        ready_since = None;
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    bail!(
                        "Cilium clean-cilium-state did not complete and the replacement agent did not remain Ready on node {node_name}; last failed init: {}",
                        cleanup_init_failure_details.as_deref().unwrap_or("no failed init attempt was reported")
                    );
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            .await;

            let restore_result = if cleanup_flag_changed {
                restore_cilium_clean_state_flag(&config, original_clean_state.as_deref()).await
            } else {
                Ok(())
            };
            match (operation, restore_result) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(error), Ok(())) => Err(error),
                (Ok(()), Err(error)) => Err(error),
                (Err(operation_error), Err(restore_error)) => Err(operation_error.context(
                    format!("restoring original Cilium clean-state flag also failed ({restore_error:#})"),
                )),
            }
        })
    }

    /// Back up hostPath/local PV payloads present on this node without
    /// exporting or re-applying cluster-wide API objects from a worker.
    pub fn node_labels(&self, node_name: &str) -> Result<HashMap<String, String>> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let discovery = wait_for_discovery(&client).await?;
            node_labels(&client, &discovery, node_name).await
        })
    }

    /// Back up the hostPath/local PV payloads on this host, using node labels
    /// from the source cluster even when the destination has no Node object yet.
    pub fn snapshot_host_paths(
        &self,
        node_labels: Option<&HashMap<String, String>>,
    ) -> Result<HostPathSnapshot> {
        let (runtime, client) = self.connected()?;
        let objects = runtime.block_on(async {
            let discovery = wait_for_discovery(&client).await?;
            let (resource, capabilities) = find_resource(&discovery, "PersistentVolume", "v1")
                .context("Kubernetes API does not expose PersistentVolume")?;
            ensure!(
                capabilities.supports_operation(verbs::LIST),
                "Kubernetes API cannot list PersistentVolumes"
            );
            let api: Api<DynamicObject> = Api::all_with(client, &resource);
            let mut objects = Vec::new();
            let mut continue_token = None;
            loop {
                let mut params = ListParams::default().limit(500);
                if let Some(token) = continue_token.as_deref() {
                    params = params.continue_token(token);
                }
                let page = api
                    .list(&params)
                    .await
                    .context("listing destination PersistentVolumes for local data backup")?;
                for object in page.items {
                    objects.push(
                        serde_json::to_value(object)
                            .context("serializing destination PersistentVolume")?,
                    );
                }
                continue_token = page.metadata.continue_.filter(|token| !token.is_empty());
                if continue_token.is_none() {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(objects)
        })?;

        let directory = export_directory()?;
        if node_labels.is_none() {
            tracing::warn!(
                "could not read local Node labels; backing up every safe hostPath/local PV path present on this host"
            );
        }
        let host_paths = persistent_host_paths(&objects, node_labels);
        let backups = backup_host_paths(&directory, &host_paths)?;
        Ok(HostPathSnapshot {
            directory,
            backups,
            cni_path_backups: Vec::new(),
        })
    }

    pub fn export(&self, installation: &Installation) -> Result<Export> {
        self.ready()?;
        let node_name = installation_node_name(installation);
        let (runtime, client) = self.connected()?;
        let (objects, labels, node_states) = runtime.block_on(async {
            let discovery = wait_for_discovery(&client).await?;
            let labels = match node_labels(&client, &discovery, &node_name).await {
                Ok(labels) => Some(labels),
                Err(error) => {
                    tracing::warn!(
                        node = %node_name,
                        error = %error,
                        "could not read local Node labels; will retain every safe hostPath/local PV path"
                    );
                    None
                }
            };
            let mut objects = Vec::new();
            let (node_resource, node_capabilities) = find_resource(&discovery, "Node", "v1")
                .context("Kubernetes API does not expose Node")?;
            ensure!(
                node_capabilities.supports_operation(verbs::LIST),
                "Kubernetes API cannot list nodes for the protected migration export"
            );
            let node_api: Api<DynamicObject> = Api::all_with(client.clone(), &node_resource);
            let mut node_states = BTreeMap::new();
            let mut continue_token = None;
            loop {
                let mut params = ListParams::default().limit(500);
                if let Some(token) = continue_token.as_deref() {
                    params = params.continue_token(token);
                }
                let page = node_api
                    .list(&params)
                    .await
                    .context("listing source nodes for protected migration metadata")?;
                for node in page.items {
                    if let Some(name) = node.metadata.name.clone() {
                        node_states.insert(name, NodeSchedulingState::from_node(&node));
                    }
                }
                continue_token = page.metadata.continue_.filter(|token| !token.is_empty());
                if continue_token.is_none() {
                    break;
                }
            }
            let mut listed_resources = BTreeSet::new();
            let mut resource_summaries = BTreeMap::<String, ResourceExportSummary>::new();
            for group in discovery.groups() {
                for version in group.versions() {
                    for (resource, capabilities) in group.versioned_resources(version) {
                        if !capabilities.supports_operation(verbs::LIST) {
                            continue;
                        }
                        if !listed_resources
                            .insert((resource.group.clone(), resource.plural.clone()))
                        {
                            continue;
                        }
                        let resource_name = if resource.group.is_empty() {
                            resource.plural.clone()
                        } else {
                            format!("{}.{}", resource.plural, resource.group)
                        };
                        let summary = resource_summaries
                            .entry(resource_name)
                            .or_default();
                        if let Some(reason) = skip_kind_reason(&resource.kind) {
                            summary.skipped_resource = Some(reason);
                            continue;
                        }
                        let api: Api<DynamicObject> = Api::all_with(client.clone(), &resource);
                        let mut continue_token = None;
                        loop {
                            let mut params = ListParams::default().limit(500);
                            if let Some(token) = continue_token.as_deref() {
                                params = params.continue_token(token);
                            }
                            let page = api.list(&params).await.with_context(|| {
                                format!("listing {} objects from the source API", resource.kind)
                            })?;
                            for object in page.items {
                                let mut value = serde_json::to_value(object)
                                    .context("serializing Kubernetes object")?;
                                preserve_discovered_type_meta(&mut value, &resource)?;
                                if let Some(reason) = object_skip_reason(&value) {
                                    *summary.skipped_objects.entry(reason).or_default() += 1;
                                } else {
                                    summary.migrated_objects += 1;
                                    objects.push(value);
                                }
                            }
                            continue_token =
                                page.metadata.continue_.filter(|token| !token.is_empty());
                            if continue_token.is_none() {
                                break;
                            }
                        }
                    }
                }
            }
            for (resource, summary) in resource_summaries {
                if let Some(reason) = summary.skipped_resource {
                    eprintln!(
                        "nodemigrate: source API resource {resource}: skipped all objects; reason={}",
                        reason.description()
                    );
                    continue;
                }
                eprintln!(
                    "nodemigrate: source API resource {resource}: {} objects selected for migration",
                    summary.migrated_objects
                );
                for (reason, count) in summary.skipped_objects {
                    eprintln!(
                        "nodemigrate: source API resource {resource}: skipped {count} objects; reason={}",
                        reason.description()
                    );
                }
            }
            Ok::<_, anyhow::Error>((objects, labels, node_states))
        })?;

        if let Some(cluster) = &installation.cluster {
            if cluster.datastore == Some(K3sDatastore::Etcd) {
                tracing::warn!(
                    "K3s uses embedded/external etcd; only Kubernetes API objects will be migrated"
                );
            }
        }
        ensure!(
            !objects.is_empty(),
            "source API returned no migratable objects"
        );
        let host_paths = persistent_host_paths(&objects, labels.as_ref());
        let mut objects: Vec<SanitizedObject> = objects.into_iter().filter_map(sanitize).collect();
        objects.sort_by_key(|object| object_rank(&object.value));
        let custom_resource_definitions = objects
            .iter()
            .filter(|object| {
                object.value.get("kind").and_then(Value::as_str) == Some("CustomResourceDefinition")
            })
            .count();
        eprintln!(
            "nodemigrate: captured {} Kubernetes API objects, including {} CustomResourceDefinitions",
            objects.len(),
            custom_resource_definitions
        );

        let dir = export_directory()?;
        let mut exported_objects = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            let path = dir.join(format!("{index:08}.json"));
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .with_context(|| format!("creating migration object {}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
            }
            serde_json::to_writer(&mut file, &object.value)
                .context("writing protected Kubernetes object")?;
            file.sync_all().with_context(|| {
                format!("syncing protected Kubernetes object {}", path.display())
            })?;
            exported_objects.push(ExportedObject {
                path,
                source_uid: object.source_uid.clone(),
            });
        }
        write_export_manifest(&dir, &exported_objects, &node_states)?;
        Ok(Export {
            dir,
            objects: exported_objects,
            node_states,
            host_paths,
            host_path_backups: Vec::new(),
            cni_path_backups: Vec::new(),
        })
    }

    pub fn import(&self, export: &Export) -> Result<()> {
        let (runtime, client) = self.connected()?;
        let destination_ca = kubeconfig_root_ca(&self.kubeconfig)?;
        let mut namespace_objects = Vec::new();
        let mut namespace_names = Vec::new();
        let mut pending = Vec::new();
        for object in export.objects.clone() {
            let value: Value = serde_json::from_slice(
                &fs::read(&object.path)
                    .with_context(|| format!("reading {}", object.path.display()))?,
            )
            .context("decoding protected migration object")?;
            if value.get("kind").and_then(Value::as_str) == Some("Namespace") {
                let name = value
                    .pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .context("exported Namespace has no metadata.name")?;
                namespace_names.push(name.to_owned());
                namespace_objects.push(object);
            } else {
                pending.push(object);
            }
        }
        eprintln!("nodemigrate: starting protected API import operation");
        let import_result = runtime.block_on(async {
            let mut last_error = String::new();
            let mut uid_map = HashMap::new();
            let mut source_crd_apis = BTreeSet::new();
            let discovery = wait_for_discovery(&client)
                .await
                .context("discovering destination APIs before namespace import")?;
            let mut namespace_failures = Vec::new();
            for exported in namespace_objects {
                let value: Value = serde_json::from_slice(
                    &fs::read(&exported.path)
                        .with_context(|| format!("reading {}", exported.path.display()))?,
                )
                .context("decoding protected Namespace")?;
                let mut namespace = value.clone();
                if let Some(metadata) = namespace
                    .pointer_mut("/metadata")
                    .and_then(Value::as_object_mut)
                {
                    metadata.remove("ownerReferences");
                }
                match apply_object(&client, &discovery, &namespace).await {
                    Ok(applied) => {
                        if let (Some(source_uid), Some(destination_uid)) =
                            (exported.source_uid, applied.metadata.uid)
                        {
                            uid_map.insert(source_uid, destination_uid);
                        }
                    }
                    Err(error) => namespace_failures.push(format!(
                        "{}: {error:#}",
                        namespace.pointer("/metadata/name")
                            .and_then(Value::as_str)
                            .unwrap_or("<unnamed>")
                    )),
                }
            }
            if !namespace_failures.is_empty() {
                bail!(
                    "could not prepare source namespaces before resource import; export retained at {}: {}",
                    export.dir.display(),
                    namespace_failures.join("; ")
                );
            }
            ensure_namespace_ca_bundles(
                &client,
                &destination_ca,
                &namespace_names,
            )
            .await
            .with_context(|| {
                format!(
                    "preparing destination CA bundles before importing workloads; export retained at {}",
                    export.dir.display()
                )
            })?;
            let mut permanent_failures = Vec::new();
            for attempt in 0..IMPORT_RETRY_ATTEMPTS {
                let discovery = discover_apis(&client)
                    .await
                    .context("discovering destination Kubernetes APIs")?;
                let mut retry = Vec::new();
                let mut failures = Vec::new();
                let mut crd_applied = 0;
                let mut crd_failed = 0;
                for object in pending {
                    let value: Value = serde_json::from_slice(
                        &fs::read(&object.path)
                            .with_context(|| format!("reading {}", object.path.display()))?
                    ).context("decoding protected migration object")?;
                    let initial = prepare_initial_import_object(&value, &uid_map);
                    if attempt == 0 {
                        source_crd_apis.extend(custom_resource_gvks(&initial));
                    }
                    let is_crd = initial.get("kind").and_then(Value::as_str)
                        == Some("CustomResourceDefinition");
                    match apply_object(&client, &discovery, &initial).await {
                        Ok(applied) => {
                            if is_crd {
                                crd_applied += 1;
                            }
                            if let (Some(source_uid), Some(destination_uid)) = (
                                object.source_uid,
                                applied.metadata.uid,
                            ) {
                                uid_map.insert(source_uid, destination_uid);
                            }
                        }
                        Err(error) => {
                            if is_crd {
                                crd_failed += 1;
                            }
                            let failure = (object_type_label(&initial), format!("{error:#}"));
                            failures.push(failure.clone());
                            // A not-found response can be a CRD establishment race only for an
                            // API declared by the source. Built-in resources must fail fast so
                            // a broken destination route is not retried for the full timeout.
                            let retry_not_found = is_source_custom_resource(&initial, &source_crd_apis);
                            if retryable_import_error(&error, retry_not_found) {
                                retry.push(object);
                            } else {
                                permanent_failures.push(failure);
                            }
                        }
                    }
                }
                pending = retry;
                if !failures.is_empty() {
                    last_error = summarize_import_failures(&failures);
                }
                let crd_apply_failed = failures
                    .iter()
                    .any(|(object_type, _)| object_type.ends_with("/CustomResourceDefinition"));
                if attempt == 0 && !source_crd_apis.is_empty() {
                    eprintln!(
                        "nodemigrate: destination accepted {crd_applied} CustomResourceDefinition apply requests; {crd_failed} failed"
                    );
                }
                if attempt == 0 && !source_crd_apis.is_empty() && !crd_apply_failed {
                    let missing = wait_for_custom_resource_apis(
                        &client,
                        &source_crd_apis,
                        std::time::Duration::from_secs(60),
                    )
                    .await?;
                    if !missing.is_empty() {
                        let crd_readback = destination_crd_readback(&client, &discovery)
                            .await
                            .unwrap_or_else(|error| format!("readback failed: {error:#}"));
                        eprintln!("nodemigrate: destination CRD readback: {crd_readback}");
                        let missing = missing
                            .iter()
                            .map(|(group, version, kind)| format!("{group}/{version}/{kind}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        bail!(
                            "{} Kubernetes objects could not be restored; export retained at {}. Destination did not expose these source CRD APIs after 60 seconds: {missing}. Destination CRD readback: {crd_readback}. Last-attempt failures: {}",
                            pending.len(), export.dir.display(), last_error
                        );
                    }
                }
                if pending.is_empty() {
                    break;
                }
                if attempt + 1 < IMPORT_RETRY_ATTEMPTS {
                    eprintln!(
                        "nodemigrate: {}/{} objects are waiting on destination readiness; retry {}/{} in {} seconds",
                        pending.len(),
                        pending.len() + permanent_failures.len(),
                        attempt + 1,
                        IMPORT_RETRY_ATTEMPTS - 1,
                        IMPORT_RETRY_DELAY.as_secs()
                    );
                    tokio::time::sleep(IMPORT_RETRY_DELAY).await;
                }
            }
            if !permanent_failures.is_empty() {
                let summary = summarize_import_failures(&permanent_failures);
                bail!("{} Kubernetes objects had permanent destination errors and {} remained transiently unavailable; export retained at {}. Permanent failures: {}. Last-attempt failures: {}", permanent_failures.len(), pending.len(), export.dir.display(), summary, last_error)
            }
            if !pending.is_empty() {
                bail!("{} Kubernetes objects could not be restored; export retained at {}. Last-attempt failures: {}", pending.len(), export.dir.display(), last_error)
            }
            let discovery = discover_apis(&client)
                .await
                .context("discovering destination APIs for reference repair")?;
            for object in &export.objects {
                let mut value: Value = serde_json::from_slice(&fs::read(&object.path)
                    .with_context(|| format!("reading {}", object.path.display()))?)
                    .context("decoding protected migration object")?;
                let mut changed = false;
                {
                    let metadata = value.pointer_mut("/metadata").and_then(Value::as_object_mut)
                        .context("migration object is missing metadata")?;
                    if let Some(owner_refs) = metadata.get_mut("ownerReferences").and_then(Value::as_array_mut) {
                        owner_refs.retain_mut(|reference| {
                            let Some(old_uid) = reference.get("uid").and_then(Value::as_str) else { return false };
                            let Some(new_uid) = uid_map.get(old_uid) else { return false };
                            reference["uid"] = Value::String(new_uid.clone());
                            changed = true;
                            true
                        });
                        if owner_refs.is_empty() { metadata.remove("ownerReferences"); }
                    }
                }
                if let Some(old_uid) = value.pointer("/spec/claimRef/uid").and_then(Value::as_str).map(str::to_owned) {
                    if let Some(claim_ref) = value.pointer_mut("/spec/claimRef").and_then(Value::as_object_mut) {
                        if let Some(new_uid) = uid_map.get(&old_uid) {
                            claim_ref.insert("uid".to_string(), Value::String(new_uid.clone()));
                        } else {
                            claim_ref.remove("uid");
                        }
                    }
                    changed = true;
                }
                if changed {
                    for attempt in 0..IMPORT_RETRY_ATTEMPTS {
                        match apply_object(&client, &discovery, &value).await {
                            Ok(_) => break,
                            Err(error)
                                if retryable_import_error(&error, false)
                                    && attempt + 1 < IMPORT_RETRY_ATTEMPTS =>
                            {
                                if attempt == 0 || (attempt + 1) % 6 == 0 {
                                    eprintln!(
                                        "nodemigrate: destination is temporarily unavailable while repairing references; retry {}/{} in {} seconds: {error:#}",
                                        attempt + 1,
                                        IMPORT_RETRY_ATTEMPTS - 1,
                                        IMPORT_RETRY_DELAY.as_secs()
                                    );
                                }
                                tokio::time::sleep(IMPORT_RETRY_DELAY).await;
                            }
                            Err(error) => {
                                return Err(error).with_context(|| {
                                    format!("repairing references in {}", object.path.display())
                                });
                            }
                        }
                    }
                }
            }
            refresh_service_account_token_secrets(
                &client,
                &discovery,
                export,
                &destination_ca,
            )
            .await
            .with_context(|| {
                format!(
                    "refreshing migrated ServiceAccount token Secrets; export retained at {}",
                    export.dir.display()
                )
            })?;
            Ok(())
        });
        eprintln!("nodemigrate: protected API import operation returned");
        eprintln!("nodemigrate: shutting down protected API import runtime");
        runtime.shutdown_timeout(std::time::Duration::from_secs(5));
        eprintln!("nodemigrate: protected API import runtime shutdown returned");
        import_result
    }
}

async fn restore_cilium_clean_state_flag(
    api: &Api<ConfigMap>,
    original: Option<&str>,
) -> Result<()> {
    let current = api
        .get("cilium-config")
        .await
        .context("reading Cilium ConfigMap while restoring clean-state flag")?;
    let current_value = current
        .data
        .as_ref()
        .and_then(|data| data.get("clean-cilium-state"))
        .map(String::as_str);
    if current_value == original {
        return Ok(());
    }
    ensure!(
        current_value == Some("true"),
        "Cilium clean-state flag changed concurrently; leaving the operator's value untouched"
    );
    let resource_version = current
        .metadata
        .resource_version
        .as_deref()
        .context("Cilium ConfigMap has no resourceVersion while restoring clean-state flag")?;
    let value = original.map_or(serde_json::Value::Null, |value| {
        serde_json::Value::String(value.to_owned())
    });
    let patch = serde_json::json!({
        "metadata": {"resourceVersion": resource_version},
        "data": {"clean-cilium-state": value}
    });
    api.patch(
        "cilium-config",
        &PatchParams::default(),
        &Patch::Merge(&patch),
    )
    .await
    .context("restoring original Cilium clean-state flag")?;
    Ok(())
}

fn parse_cilium_kube_proxy_replacement(value: Option<&str>) -> Result<bool> {
    match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        None | Some("false") => Ok(false),
        Some("true" | "strict") => Ok(true),
        Some(other) => bail!("unrecognized Cilium kube-proxy-replacement setting '{other}'"),
    }
}

const LEGACY_TOKEN_EXPIRATION_SECONDS: i64 = 31_536_000;

/// Reissue legacy Secret-backed credentials against the destination cluster.
/// The source JWT is bound to a different issuer, signing key, and
/// ServiceAccount UID, so copying it verbatim would preserve a Secret that can
/// no longer authenticate. Keep the Secret's name and other data while
/// replacing Kubernetes-managed token fields with a destination TokenRequest.
async fn refresh_service_account_token_secrets(
    client: &Client,
    discovery: &Discovery,
    export: &Export,
    destination_ca: &str,
) -> Result<usize> {
    let (secret_resource, secret_capabilities) =
        find_resource(discovery, "Secret", "v1").context("destination does not expose Secret")?;
    ensure!(
        secret_capabilities.supports_operation(verbs::GET)
            && secret_capabilities.supports_operation(verbs::PATCH),
        "destination cannot update migrated ServiceAccount token Secrets"
    );
    let (account_resource, account_capabilities) = find_resource(discovery, "ServiceAccount", "v1")
        .context("destination does not expose ServiceAccount")?;
    ensure!(
        account_capabilities.supports_operation(verbs::GET)
            && account_capabilities.supports_operation(verbs::CREATE),
        "destination cannot issue ServiceAccount tokens"
    );

    let mut refreshed = 0;
    for object in &export.objects {
        let value: Value = serde_json::from_slice(
            &fs::read(&object.path)
                .with_context(|| format!("reading exported object {}", object.path.display()))?,
        )
        .context("decoding exported object while refreshing ServiceAccount tokens")?;
        if value.get("kind").and_then(Value::as_str) != Some("Secret")
            || value.pointer("/type").and_then(Value::as_str)
                != Some("kubernetes.io/service-account-token")
        {
            continue;
        }
        let name = value
            .pointer("/metadata/name")
            .and_then(Value::as_str)
            .context("ServiceAccount token Secret has no name")?;
        let namespace = value
            .pointer("/metadata/namespace")
            .and_then(Value::as_str)
            .context("ServiceAccount token Secret has no namespace")?;
        let account_name = value
            .pointer("/metadata/annotations/kubernetes.io~1service-account.name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .with_context(|| {
                format!(
                    "ServiceAccount token Secret {namespace}/{name} has no service-account.name annotation"
                )
            })?;

        let service_accounts: Api<DynamicObject> =
            Api::namespaced_with(client.clone(), namespace, &account_resource);
        let account = service_accounts
            .get_opt(account_name)
            .await
            .with_context(|| {
                format!("reading destination ServiceAccount {namespace}/{account_name}")
            })?
            .with_context(|| {
                format!("destination ServiceAccount {namespace}/{account_name} is missing")
            })?;
        let account_uid = account
            .metadata
            .uid
            .context("destination ServiceAccount has no UID")?;

        let request = serde_json::json!({
            "apiVersion": "authentication.k8s.io/v1",
            "kind": "TokenRequest",
            "spec": {
                "audiences": [],
                "expirationSeconds": LEGACY_TOKEN_EXPIRATION_SECONDS
            }
        });
        let response: Value = service_accounts
            .create_subresource("token", account_name, &PostParams::default(), &request)
            .await
            .with_context(|| {
                format!(
                    "requesting destination token for ServiceAccount {namespace}/{account_name}"
                )
            })?;
        let token = response
            .pointer("/status/token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .with_context(|| {
                format!(
                    "TokenRequest for ServiceAccount {namespace}/{account_name} returned no token"
                )
            })?;

        let secrets: Api<DynamicObject> =
            Api::namespaced_with(client.clone(), namespace, &secret_resource);
        if let Some(secret) = secrets
            .get_opt(name)
            .await
            .with_context(|| {
                format!("reading migrated ServiceAccount token Secret {namespace}/{name}")
            })?
        {
            patch_service_account_token_secret(
                &secrets,
                &secret,
                namespace,
                &account_uid,
                token,
                destination_ca,
            )
            .await?;
        } else {
            ensure!(
                secret_capabilities.supports_operation(verbs::CREATE),
                "destination cannot recreate migrated ServiceAccount token Secrets"
            );
            let reissued = service_account_token_secret_value(
                &value,
                namespace,
                &account_uid,
                token,
                destination_ca,
            )?;
            let object: DynamicObject =
                serde_json::from_value(reissued).context("decoding reissued ServiceAccount token Secret")?;
            match secrets.create(&PostParams::default(), &object).await {
                Ok(_) => {}
                Err(kube::Error::Api(response)) if response.code == 409 => {
                    // The token controller or another writer may have recreated
                    // the name after the preceding GET. Refresh that current
                    // object using its resourceVersion instead of overwriting it
                    // with the stale export.
                    let current = secrets
                        .get(name)
                        .await
                        .with_context(|| {
                            format!("reading concurrently recreated token Secret {namespace}/{name}")
                        })?;
                    patch_service_account_token_secret(
                        &secrets,
                        &current,
                        namespace,
                        &account_uid,
                        token,
                        destination_ca,
                    )
                    .await?;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("recreating migrated ServiceAccount token Secret {namespace}/{name}")
                    });
                }
            }
            eprintln!(
                "nodemigrate: recreated missing ServiceAccount token Secret {namespace}/{name} with destination credentials"
            );
        }
        refreshed += 1;
    }
    if refreshed > 0 {
        eprintln!(
            "nodemigrate: refreshed {refreshed} ServiceAccount token Secret(s) for the destination cluster"
        );
    }
    Ok(refreshed)
}

fn service_account_token_secret_patch(
    secret: &DynamicObject,
    namespace: &str,
    service_account_uid: &str,
    token: &str,
    destination_ca: &str,
) -> Result<Value> {
    let resource_version = secret
        .metadata
        .resource_version
        .as_deref()
        .context("migrated ServiceAccount token Secret has no resourceVersion")?;
    Ok(serde_json::json!({
        "metadata": {
            "resourceVersion": resource_version,
            "annotations": {
                "kubernetes.io/service-account.uid": service_account_uid
            }
        },
        "data": {
            "token": base64::engine::general_purpose::STANDARD.encode(token),
            "namespace": base64::engine::general_purpose::STANDARD.encode(namespace),
            "ca.crt": base64::engine::general_purpose::STANDARD.encode(destination_ca)
        }
    }))
}

fn service_account_token_secret_value(
    source: &Value,
    namespace: &str,
    service_account_uid: &str,
    token: &str,
    destination_ca: &str,
) -> Result<Value> {
    let mut secret = source.clone();
    let metadata = secret
        .pointer_mut("/metadata")
        .and_then(Value::as_object_mut)
        .context("exported ServiceAccount token Secret has no metadata")?;
    metadata.remove("uid");
    metadata.remove("resourceVersion");
    metadata.remove("managedFields");
    let annotations = metadata
        .entry("annotations")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .context("ServiceAccount token Secret annotations are not an object")?;
    annotations.insert(
        "kubernetes.io/service-account.uid".to_string(),
        Value::String(service_account_uid.to_string()),
    );

    let data = secret
        .as_object_mut()
        .context("exported ServiceAccount token Secret is not an object")?
        .entry("data")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .context("ServiceAccount token Secret data is not an object")?;
    data.insert(
        "token".to_string(),
        Value::String(base64::engine::general_purpose::STANDARD.encode(token)),
    );
    data.insert(
        "namespace".to_string(),
        Value::String(base64::engine::general_purpose::STANDARD.encode(namespace)),
    );
    data.insert(
        "ca.crt".to_string(),
        Value::String(base64::engine::general_purpose::STANDARD.encode(destination_ca)),
    );
    Ok(secret)
}

async fn patch_service_account_token_secret(
    secrets: &Api<DynamicObject>,
    secret: &DynamicObject,
    namespace: &str,
    service_account_uid: &str,
    token: &str,
    destination_ca: &str,
) -> Result<()> {
    let patch = service_account_token_secret_patch(
        secret,
        namespace,
        service_account_uid,
        token,
        destination_ca,
    )?;
    let name = secret
        .metadata
        .name
        .as_deref()
        .context("migrated Secret has no metadata.name")?;
    secrets
        .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .with_context(|| {
            format!("writing refreshed token data to Secret {namespace}/{name}")
        })?;
    Ok(())
}

fn kubeconfig_root_ca(kubeconfig_path: &Path) -> Result<String> {
    let kubeconfig = Kubeconfig::read_from(kubeconfig_path).with_context(|| {
        format!(
            "reading destination kubeconfig {}",
            kubeconfig_path.display()
        )
    })?;
    let context_name = kubeconfig
        .current_context
        .as_deref()
        .context("destination kubeconfig has no current-context")?;
    let context = kubeconfig
        .contexts
        .iter()
        .find(|context| context.name == context_name)
        .and_then(|context| context.context.as_ref())
        .context("destination kubeconfig current-context is missing")?;
    let cluster = kubeconfig
        .clusters
        .iter()
        .find(|cluster| cluster.name == context.cluster)
        .and_then(|cluster| cluster.cluster.as_ref())
        .context("destination kubeconfig current cluster is missing")?;
    let ca = if let Some(data) = &cluster.certificate_authority_data {
        base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .context("decoding destination certificate-authority-data")?
    } else if let Some(path) = &cluster.certificate_authority {
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            kubeconfig_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(path)
        };
        fs::read(&path)
            .with_context(|| format!("reading destination CA file {}", path.display()))?
    } else {
        bail!("destination kubeconfig has no certificate authority data or file");
    };
    String::from_utf8(ca).context("destination CA data is not UTF-8 PEM")
}

async fn ensure_namespace_ca_bundles(
    client: &Client,
    expected_ca: &str,
    expected_namespaces: &[String],
) -> Result<()> {
    let mut namespaces = expected_namespaces.to_vec();
    namespaces.sort();
    namespaces.dedup();
    for namespace in namespaces {
        let configmaps: Api<ConfigMap> = Api::namespaced(client.clone(), &namespace);
        let mut ready = false;
        for attempt in 0..5 {
            if let Some(configmap) = configmaps
                .get_opt("kube-root-ca.crt")
                .await
                .with_context(|| format!("reading {namespace}/kube-root-ca.crt"))?
            {
                if namespace_ca_bundle_matches(&configmap, expected_ca) {
                    ready = true;
                    break;
                }
                let patch = serde_json::json!({"data": {"ca.crt": expected_ca}});
                match configmaps
                    .patch(
                        "kube-root-ca.crt",
                        &PatchParams::default(),
                        &Patch::Merge(&patch),
                    )
                    .await
                {
                    Ok(configmap) if namespace_ca_bundle_matches(&configmap, expected_ca) => {
                        ready = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(kube::Error::Api(response))
                        if response.code == 404 || response.code == 409 => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("updating {namespace}/kube-root-ca.crt"))
                    }
                }
            } else {
                let configmap = ConfigMap {
                    metadata: ObjectMeta {
                        name: Some("kube-root-ca.crt".to_string()),
                        namespace: Some(namespace.clone()),
                        ..Default::default()
                    },
                    data: Some(BTreeMap::from([(
                        "ca.crt".to_string(),
                        expected_ca.to_string(),
                    )])),
                    ..Default::default()
                };
                match configmaps.create(&PostParams::default(), &configmap).await {
                    Ok(configmap) if namespace_ca_bundle_matches(&configmap, expected_ca) => {
                        ready = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(kube::Error::Api(response))
                        if response.code == 404 || response.code == 409 => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("creating {namespace}/kube-root-ca.crt"))
                    }
                }
            }
            if attempt < 4 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
        ensure!(
            ready,
            "destination kube-root-ca.crt in namespace {namespace} did not match its admin kubeconfig CA after retries"
        );
    }
    Ok(())
}

fn namespace_ca_bundle_matches(configmap: &ConfigMap, expected_ca: &str) -> bool {
    configmap
        .data
        .as_ref()
        .and_then(|data| data.get("ca.crt"))
        .is_some_and(|ca| ca == expected_ca)
}

#[derive(Debug)]
pub struct Export {
    pub dir: PathBuf,
    objects: Vec<ExportedObject>,
    node_states: BTreeMap<String, NodeSchedulingState>,
    host_paths: Vec<PathBuf>,
    host_path_backups: Vec<HostPathBackup>,
    cni_path_backups: Vec<CniPathBackup>,
}

#[derive(Debug, Clone)]
struct ExportedObject {
    path: PathBuf,
    source_uid: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
struct ExportManifest {
    format_version: u32,
    objects: Vec<ExportManifestObject>,
    #[serde(default)]
    node_states: BTreeMap<String, NodeSchedulingState>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
struct ExportManifestObject {
    file: String,
    source_uid: Option<String>,
}

#[derive(Debug)]
struct SanitizedObject {
    value: Value,
    source_uid: Option<String>,
}

#[derive(Debug)]
pub struct HostPathSnapshot {
    directory: PathBuf,
    backups: Vec<HostPathBackup>,
    cni_path_backups: Vec<CniPathBackup>,
}

#[derive(Debug)]
struct HostPathBackup {
    source: PathBuf,
    backup: PathBuf,
    directory: bool,
}

impl Export {
    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    pub(crate) fn node_state_count(&self) -> usize {
        self.node_states.len()
    }

    pub(crate) fn node_state(&self, name: &str) -> Option<&NodeSchedulingState> {
        self.node_states.get(name)
    }

    /// Read Cilium's Service datapath mode from a protected source export.
    /// Missing configuration uses Cilium's default, which keeps kube-proxy
    /// replacement disabled.
    pub(crate) fn cilium_kube_proxy_replacement(&self) -> Result<bool> {
        for object in &self.objects {
            let value: Value =
                serde_json::from_slice(&fs::read(&object.path).with_context(|| {
                    format!("reading migration object {}", object.path.display())
                })?)
                .with_context(|| format!("decoding migration object {}", object.path.display()))?;
            if value.get("kind").and_then(Value::as_str) != Some("ConfigMap")
                || value.pointer("/metadata/namespace").and_then(Value::as_str)
                    != Some("kube-system")
                || value.pointer("/metadata/name").and_then(Value::as_str) != Some("cilium-config")
            {
                continue;
            }
            return parse_cilium_kube_proxy_replacement(
                value
                    .pointer("/data/kube-proxy-replacement")
                    .and_then(Value::as_str),
            );
        }
        Ok(false)
    }

    pub(crate) fn kube_proxy_daemonset_present(&self) -> Result<bool> {
        for object in &self.objects {
            let value: Value = serde_json::from_slice(
                &fs::read(&object.path)
                    .with_context(|| {
                        format!("reading migration object {}", object.path.display())
                    })?,
            )
            .with_context(|| format!("decoding migration object {}", object.path.display()))?;
            if value.get("kind").and_then(Value::as_str) == Some("DaemonSet")
                && value.pointer("/metadata/namespace").and_then(Value::as_str)
                    == Some("kube-system")
                && value.pointer("/metadata/name").and_then(Value::as_str) == Some("kube-proxy")
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn control_plane_node_names(&self) -> Vec<String> {
        self.node_states
            .iter()
            .filter(|(_, state)| {
                state
                    .labels
                    .contains_key("node-role.kubernetes.io/control-plane")
                    || state.labels.contains_key("node-role.kubernetes.io/master")
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub fn snapshot_host_paths_for_node(&mut self, node_name: &str) -> Result<()> {
        let labels = &self
            .node_state(node_name)
            .with_context(|| {
                format!("migration export has no scheduling metadata for source node {node_name}")
            })?
            .labels;
        let values = self
            .objects
            .iter()
            .map(|object| {
                serde_json::from_slice::<Value>(&fs::read(&object.path).with_context(|| {
                    format!("reading migration object {}", object.path.display())
                })?)
                .with_context(|| format!("decoding migration object {}", object.path.display()))
            })
            .collect::<Result<Vec<_>>>()?;
        self.host_paths = persistent_host_paths(&values, Some(labels));
        self.host_path_backups = backup_host_paths_in(
            &self
                .dir
                .join(format!("host-paths-{}", safe_backup_name(node_name))),
            &self.host_paths,
        )?;
        Ok(())
    }

    /// Make a node-private protected copy before attaching host and CNI recovery
    /// data. Later cluster nodes must not mutate the first node's export.
    pub(crate) fn private_copy_for_node(&self, node_name: &str) -> Result<Self> {
        ensure!(!node_name.is_empty(), "source node name is empty");
        let directory = export_directory()?.join(format!("node-{}", safe_backup_name(node_name)));
        self.copy_to_directory(directory)
    }

    fn copy_to_directory(&self, directory: PathBuf) -> Result<Self> {
        fs::create_dir(&directory).with_context(|| {
            format!(
                "creating node-private migration export {}",
                directory.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        }

        let mut objects = Vec::with_capacity(self.objects.len());
        for (index, object) in self.objects.iter().enumerate() {
            let path = directory.join(format!("{index:08}.json"));
            fs::copy(&object.path, &path).with_context(|| {
                format!(
                    "copying protected migration object {}",
                    object.path.display()
                )
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            }
            fs::File::open(&path)
                .with_context(|| format!("opening copied migration object {}", path.display()))?
                .sync_all()
                .with_context(|| format!("syncing copied migration object {}", path.display()))?;
            objects.push(ExportedObject {
                path,
                source_uid: object.source_uid.clone(),
            });
        }
        write_export_manifest(&directory, &objects, &self.node_states)?;
        Self::load(directory)
    }

    pub fn load(directory: impl Into<PathBuf>) -> Result<Self> {
        let directory = directory.into();
        let directory_metadata = fs::symlink_metadata(&directory).with_context(|| {
            format!("reading migration export directory {}", directory.display())
        })?;
        ensure!(
            directory_metadata.is_dir() && !directory_metadata.file_type().is_symlink(),
            "migration export path {} must be a real directory",
            directory.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                directory_metadata.permissions().mode() & 0o077 == 0,
                "migration export directory {} is accessible to group or other users",
                directory.display()
            );
        }

        let manifest_path = directory.join("manifest.json");
        let manifest_metadata = fs::symlink_metadata(&manifest_path)
            .with_context(|| format!("reading migration manifest {}", manifest_path.display()))?;
        ensure!(
            manifest_metadata.is_file() && !manifest_metadata.file_type().is_symlink(),
            "migration manifest {} must be a regular file",
            manifest_path.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                manifest_metadata.permissions().mode() & 0o077 == 0,
                "migration manifest {} is accessible to group or other users",
                manifest_path.display()
            );
        }
        let manifest: ExportManifest =
            serde_json::from_slice(&fs::read(&manifest_path).with_context(|| {
                format!("reading migration manifest {}", manifest_path.display())
            })?)
            .with_context(|| format!("decoding migration manifest {}", manifest_path.display()))?;
        ensure!(
            matches!(manifest.format_version, 1 | 2),
            "unsupported migration export format version {}",
            manifest.format_version
        );
        ensure!(
            !manifest.objects.is_empty(),
            "migration export manifest {} contains no objects",
            manifest_path.display()
        );

        let mut filenames = std::collections::HashSet::new();
        let mut source_uids = std::collections::HashSet::new();
        let mut objects = Vec::with_capacity(manifest.objects.len());
        for entry in manifest.objects {
            ensure!(
                is_export_object_filename(&entry.file),
                "invalid migration object filename '{}' in {}",
                entry.file,
                manifest_path.display()
            );
            ensure!(
                filenames.insert(entry.file.clone()),
                "duplicate migration object filename '{}' in {}",
                entry.file,
                manifest_path.display()
            );
            if let Some(uid) = &entry.source_uid {
                ensure!(
                    source_uids.insert(uid.clone()),
                    "duplicate source UID in migration manifest {}",
                    manifest_path.display()
                );
            }
            let path = directory.join(&entry.file);
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("reading migration object {}", path.display()))?;
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "migration object {} must be a regular file",
                path.display()
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                ensure!(
                    metadata.permissions().mode() & 0o077 == 0,
                    "migration object {} is accessible to group or other users",
                    path.display()
                );
            }
            let value: Value = serde_json::from_slice(
                &fs::read(&path)
                    .with_context(|| format!("reading migration object {}", path.display()))?,
            )
            .with_context(|| format!("decoding migration object {}", path.display()))?;
            ensure!(
                value.get("apiVersion").and_then(Value::as_str).is_some()
                    && value.get("kind").and_then(Value::as_str).is_some()
                    && value
                        .pointer("/metadata/name")
                        .and_then(Value::as_str)
                        .is_some(),
                "migration object {} is missing apiVersion, kind, or metadata.name",
                path.display()
            );
            objects.push(ExportedObject {
                path,
                source_uid: entry.source_uid,
            });
        }

        Ok(Self {
            dir: directory,
            objects,
            node_states: manifest.node_states,
            host_paths: Vec::new(),
            host_path_backups: Vec::new(),
            cni_path_backups: Vec::new(),
        })
    }

    pub fn snapshot_host_paths(&mut self) -> Result<()> {
        self.host_path_backups = backup_host_paths(&self.dir, &self.host_paths)?;
        Ok(())
    }

    pub fn restore_host_paths(&self) -> Result<()> {
        restore_host_path_backups(&self.host_path_backups)
    }

    pub fn snapshot_k3s_cni_paths(&mut self, installation: &Installation) -> Result<()> {
        let node_name = installation_node_name(installation);
        let backup_directory = self
            .dir
            .join(format!("cni-node-{}", safe_backup_name(&node_name)));
        self.cni_path_backups = snapshot_k3s_cni_paths(&backup_directory, installation)?;
        Ok(())
    }

    pub fn restore_k3s_cni_paths(&self) -> Result<()> {
        restore_cni_path_backups(&self.cni_path_backups)
    }
}

#[derive(Debug)]
struct CniPathBackup {
    source: PathBuf,
    backup: PathBuf,
}

fn backup_cni_paths(directory: &Path, paths: &[PathBuf]) -> Result<Vec<CniPathBackup>> {
    let existing: Vec<_> = paths.iter().filter(|path| path.is_dir()).cloned().collect();
    if existing.is_empty() {
        return Ok(Vec::new());
    }
    let backup_root = directory.join("cni-paths");
    fs::create_dir(&backup_root).context("creating protected CNI path backup")?;
    let mut backups = Vec::with_capacity(existing.len());
    for (index, source) in existing.into_iter().enumerate() {
        let backup = backup_root.join(format!("{index:08}"));
        fs::create_dir(&backup).with_context(|| {
            format!(
                "creating protected CNI backup directory {}",
                backup.display()
            )
        })?;
        run_cp_following_symlinks(&source.join("."), &backup)
            .with_context(|| format!("backing up CNI directory {}", source.display()))?;
        backups.push(CniPathBackup { source, backup });
    }
    Ok(backups)
}

fn run_cp_following_symlinks(source: &Path, destination: &Path) -> Result<()> {
    let output = Command::new("cp")
        .args(["-aL", "--"])
        .arg(source)
        .arg(destination)
        .output()
        .context("running cp to preserve CNI files")?;
    ensure!(
        output.status.success(),
        "cp failed while preserving CNI files: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn restore_cni_path_backups(backups: &[CniPathBackup]) -> Result<()> {
    for item in backups {
        match fs::symlink_metadata(&item.source) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                fs::remove_file(&item.source).with_context(|| {
                    format!("removing stale CNI path link {}", item.source.display())
                })?;
                fs::create_dir_all(&item.source).with_context(|| {
                    format!("recreating CNI directory {}", item.source.display())
                })?;
                run_cp_following_symlinks(&item.backup.join("."), &item.source)?;
            }
            Ok(metadata) if metadata.is_dir() => {
                run_cp_following_symlinks(&item.backup.join("."), &item.source)?;
            }
            Ok(_) => bail!(
                "cannot restore CNI path {} because a non-directory exists there",
                item.source.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(&item.source).with_context(|| {
                    format!("recreating CNI directory {}", item.source.display())
                })?;
                run_cp_following_symlinks(&item.backup.join("."), &item.source)?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("checking CNI path {}", item.source.display()))
            }
        }
    }
    Ok(())
}

impl HostPathSnapshot {
    pub fn recovery_directory(&self) -> &Path {
        &self.directory
    }

    pub fn restore(&self) -> Result<()> {
        restore_host_path_backups(&self.backups)
    }

    pub fn snapshot_k3s_cni_paths(&mut self, installation: &Installation) -> Result<()> {
        self.cni_path_backups = snapshot_k3s_cni_paths(&self.directory, installation)?;
        Ok(())
    }

    pub fn restore_k3s_cni_paths(&self) -> Result<()> {
        restore_cni_path_backups(&self.cni_path_backups)
    }
}

fn snapshot_k3s_cni_paths(
    directory: &Path,
    installation: &Installation,
) -> Result<Vec<CniPathBackup>> {
    if installation.distribution != Distribution::K3s {
        return Ok(Vec::new());
    }
    let Some(cluster) = &installation.cluster else {
        return Ok(Vec::new());
    };
    let paths = [
        cluster.cni_conf_dir.as_deref(),
        cluster.cni_bin_dir.as_deref(),
    ];
    for path in paths.into_iter().flatten() {
        if path.starts_with(&cluster.data_dir) {
            ensure!(
                path != cluster.data_dir.as_path(),
                "detected CNI path {} is the whole K3s data directory; refusing to uninstall without a bounded CNI backup",
                path.display()
            );
            ensure!(
                path.is_dir(),
                "detected CNI directory {} is missing; refusing to uninstall K3s without a complete CNI backup",
                path.display()
            );
        }
    }
    let mut candidates: Vec<_> = paths
        .into_iter()
        .flatten()
        .filter(|path| path.starts_with(&cluster.data_dir))
        .map(Path::to_path_buf)
        .collect();
    candidates.sort_by_key(|path| path.components().count());
    candidates.dedup();
    let mut roots = Vec::<PathBuf>::new();
    for path in candidates {
        if !roots.iter().any(|root| path.starts_with(root)) {
            roots.push(path);
        }
    }
    backup_cni_paths(directory, &roots)
}

fn restore_host_path_backups(backups: &[HostPathBackup]) -> Result<()> {
    for item in backups {
        if item.directory {
            fs::create_dir_all(&item.source).with_context(|| {
                format!(
                    "recreating persistent volume path {}",
                    item.source.display()
                )
            })?;
            run_cp(&item.backup.join("."), &item.source)?;
        } else {
            if let Some(parent) = item.source.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("creating persistent volume parent {}", parent.display())
                })?;
            }
            run_cp(&item.backup, &item.source)?;
        }
    }
    Ok(())
}

async fn node_labels(
    client: &Client,
    discovery: &Discovery,
    name: &str,
) -> Result<HashMap<String, String>> {
    let (resource, capabilities) = find_resource(discovery, "Node", "v1")
        .context("Kubernetes API does not expose Node while selecting local PV data")?;
    ensure!(
        capabilities.supports_operation(verbs::GET),
        "Kubernetes API cannot read Node labels while selecting local PV data"
    );
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &resource);
    let node = api
        .get_opt(name)
        .await
        .with_context(|| format!("reading local Node {name} labels for PV selection"))?
        .with_context(|| format!("local Node {name} is absent from the API during PV selection"))?;
    let mut labels: HashMap<String, String> = node
        .data
        .pointer("/metadata/labels")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_owned())))
        .collect();
    labels.insert("metadata.name".to_string(), name.to_string());
    Ok(labels)
}

fn installation_node_name(installation: &Installation) -> String {
    std::env::var("NODEMIGRATE_NODE_NAME")
        .ok()
        .filter(|name| !name.is_empty())
        .or_else(|| {
            installation
                .cluster
                .as_ref()
                .and_then(|cluster| cluster.node_name.clone())
        })
        .or_else(|| std::env::var("NODELET_NODE_NAME").ok())
        .or_else(|| {
            std::process::Command::new("uname")
                .arg("-n")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
                .filter(|name| !name.is_empty())
        })
        .unwrap_or_else(|| "localhost".to_string())
}

fn persistent_host_paths(
    objects: &[Value],
    node_labels: Option<&HashMap<String, String>>,
) -> Vec<PathBuf> {
    let mut paths = std::collections::BTreeSet::new();
    for object in objects {
        if object.get("kind").and_then(Value::as_str) != Some("PersistentVolume") {
            continue;
        }
        if node_labels.is_some_and(|labels| !pv_matches_node(object, labels)) {
            continue;
        }
        for path in [
            object.pointer("/spec/hostPath/path"),
            object.pointer("/spec/local/path"),
        ]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        {
            let path = PathBuf::from(path);
            if path.is_absolute()
                && !path
                    .components()
                    .any(|component| component == std::path::Component::ParentDir)
                && path != Path::new("/")
            {
                paths.insert(path);
            }
        }
    }
    let mut paths: Vec<_> = paths.into_iter().collect();
    paths.sort_by_key(|path| path.components().count());
    let mut roots: Vec<PathBuf> = Vec::new();
    for path in paths {
        if !roots.iter().any(|root| path.starts_with(root)) {
            roots.push(path);
        }
    }
    roots
}

fn pv_matches_node(object: &Value, node_labels: &HashMap<String, String>) -> bool {
    let Some(terms) = object
        .pointer("/spec/nodeAffinity/required/nodeSelectorTerms")
        .and_then(Value::as_array)
    else {
        return true;
    };
    terms.iter().any(|term| {
        let expressions = term
            .get("matchExpressions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        let fields = term
            .get("matchFields")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        let requirements: Vec<_> = expressions.chain(fields).collect();
        !requirements.is_empty()
            && requirements
                .into_iter()
                .all(|requirement| node_selector_requirement_matches(requirement, node_labels))
    })
}

fn node_selector_requirement_matches(
    requirement: &Value,
    node_labels: &HashMap<String, String>,
) -> bool {
    let Some(key) = requirement.get("key").and_then(Value::as_str) else {
        return false;
    };
    let value = node_labels.get(key);
    let values: Vec<&str> = requirement
        .get("values")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let operator = requirement
        .get("operator")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match operator {
        "In" => value.is_some_and(|value| values.contains(&value.as_str())),
        "NotIn" => value.is_none_or(|value| !values.contains(&value.as_str())),
        "Exists" => value.is_some(),
        "DoesNotExist" => value.is_none(),
        "Gt" | "Lt" => {
            let Some(threshold) = values.first().and_then(|value| value.parse::<i64>().ok()) else {
                return false;
            };
            value
                .and_then(|value| value.parse::<i64>().ok())
                .is_some_and(|value| match operator {
                    "Gt" => value > threshold,
                    _ => value < threshold,
                })
        }
        _ => false,
    }
}

fn backup_host_paths(directory: &Path, paths: &[PathBuf]) -> Result<Vec<HostPathBackup>> {
    backup_host_paths_in(&directory.join("host-paths"), paths)
}

fn backup_host_paths_in(backup_root: &Path, paths: &[PathBuf]) -> Result<Vec<HostPathBackup>> {
    fs::create_dir(&backup_root).context("creating protected persistent volume backup")?;
    let mut backups = Vec::new();
    for (index, source) in paths.iter().enumerate() {
        if !source.exists() {
            continue;
        }
        let backup = backup_root.join(format!("{index:08}"));
        run_cp(source, &backup)
            .with_context(|| format!("backing up persistent volume path {}", source.display()))?;
        backups.push(HostPathBackup {
            source: source.clone(),
            backup,
            directory: source.is_dir(),
        });
    }
    Ok(backups)
}

fn safe_backup_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn run_cp(source: &Path, destination: &Path) -> Result<()> {
    let output = Command::new("cp")
        .args(["-a", "--"])
        .arg(source)
        .arg(destination)
        .output()
        .context("running cp to preserve persistent volume data")?;
    ensure!(
        output.status.success(),
        "cp failed while preserving persistent volume data: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn find_resource(
    discovery: &Discovery,
    kind: &str,
    api_version: &str,
) -> Option<(ApiResource, kube::discovery::ApiCapabilities)> {
    for group in discovery.groups() {
        for version in group.versions() {
            if let Some(resource) = group
                .versioned_resources(version)
                .into_iter()
                .find(|(resource, _)| resource.kind == kind && resource.api_version == api_version)
            {
                return Some(resource);
            }
        }
    }
    None
}

fn same_group_kind(resource: &ApiResource, kind: &str, api_version: &str) -> bool {
    let source_group = api_version
        .split_once('/')
        .map_or("", |(group, _version)| group);
    resource.kind == kind && resource.group == source_group
}

fn find_compatible_resource(
    discovery: &Discovery,
    kind: &str,
    api_version: &str,
) -> Option<(ApiResource, kube::discovery::ApiCapabilities)> {
    for group in discovery.groups() {
        for version in group.versions() {
            if let Some(resource) = group
                .versioned_resources(version)
                .into_iter()
                .find(|(resource, _)| same_group_kind(resource, kind, api_version))
            {
                return Some(resource);
            }
        }
    }
    None
}

fn preserve_discovered_type_meta(value: &mut Value, resource: &ApiResource) -> Result<()> {
    let object = value
        .as_object_mut()
        .context("serialized Kubernetes object is not a JSON object")?;
    if object
        .get("apiVersion")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        object.insert(
            "apiVersion".to_string(),
            Value::String(resource.api_version.clone()),
        );
    }
    if object
        .get("kind")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        object.insert("kind".to_string(), Value::String(resource.kind.clone()));
    }
    Ok(())
}

async fn apply_object(
    client: &Client,
    discovery: &Discovery,
    value: &Value,
) -> Result<DynamicObject> {
    let type_meta = value
        .get("apiVersion")
        .and_then(Value::as_str)
        .context("object has no apiVersion")?;
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .context("object has no kind")?;
    let (resource, capabilities) = find_resource(discovery, kind, type_meta)
        .or_else(|| find_compatible_resource(discovery, kind, type_meta))
        .with_context(|| {
            format!("destination does not expose {type_meta}/{kind} or another version of it")
        })?;
    ensure!(
        capabilities.supports_operation(verbs::GET),
        "destination does not allow reading existing {kind} objects before migration"
    );
    let mut apply_value = value.clone();
    let source_was_running = apply_value
        .get("_nodemigrateSourceWasRunning")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(object) = apply_value.as_object_mut() {
        object.remove("_nodemigrateSourceWasRunning");
    }
    let ephemeral_containers = take_pod_ephemeral_containers(&mut apply_value)
        .context("preparing Pod ephemeral containers for migration")?;
    if resource.api_version != type_meta {
        let name = apply_value
            .pointer("/metadata/name")
            .and_then(Value::as_str)
            .unwrap_or("<unnamed>");
        eprintln!(
            "nodemigrate: destination serves {kind} as {}, applying source version {type_meta} through that API version for {name}",
            resource.api_version
        );
        apply_value["apiVersion"] = Value::String(resource.api_version.clone());
    }
    let object: DynamicObject =
        serde_json::from_value(apply_value).context("decoding migration object")?;
    let api: Api<DynamicObject> = if let Some(namespace) = object.metadata.namespace.as_deref() {
        Api::namespaced_with(client.clone(), namespace, &resource)
    } else {
        Api::all_with(client.clone(), &resource)
    };
    let name = object
        .metadata
        .name
        .as_deref()
        .context("migration object has no metadata.name")?;
    let api_root = if resource.group.is_empty() {
        format!("/api/{}", resource.version)
    } else {
        format!("/apis/{}/{}", resource.group, resource.version)
    };
    let api_path = if let Some(namespace) = object.metadata.namespace.as_deref() {
        format!(
            "{api_root}/namespaces/{namespace}/{}/{name}",
            resource.plural
        )
    } else {
        format!("{api_root}/{}/{name}", resource.plural)
    };
    // SSA only replaces fields owned by this field manager. On a destination
    // with a same-name bootstrap object (for example, CoreDNS), omitted source
    // fields remain behind and silently produce a hybrid object. Use an
    // optimistic, full-object update for collisions so the source object is
    // authoritative; use a normal create for objects that are absent.
    const WRITE_ATTEMPTS: usize = 3;
    for attempt in 0..WRITE_ATTEMPTS {
        let existing = api
            .get_opt(name)
            .await
            .with_context(|| format!("reading destination {type_meta}/{kind} {name}"))?;
        let result = if let Some(existing) = existing {
            if kind == "CustomResourceDefinition"
                && matches!(name, "ingressroutes.traefik.io" | "ingressroutetcps.traefik.io")
                && !crd_schema_matches(&existing, &object)
            {
                const PRIORITY_MAXIMUM: &str = "/versions/0/schema/openAPIV3Schema/properties/spec/properties/routes/items/properties/priority/maximum";
                eprintln!(
                    "nodemigrate: CRD schema comparison {name}: existing maximum={:?}, desired maximum={:?}, existing_spec_matches_desired={}",
                    existing.data.pointer(&format!("/spec{PRIORITY_MAXIMUM}")),
                    object.data.pointer(&format!("/spec{PRIORITY_MAXIMUM}")),
                    existing.data.get("spec") == object.data.get("spec"),
                );
            }
            if crd_schema_matches(&existing, &object) {
                if can_preserve_existing_crd(&existing, &object) {
                    eprintln!(
                        "nodemigrate: preserved unchanged CustomResourceDefinition {name} without rewriting its schema"
                    );
                    return Ok(existing);
                }

                let patch = crd_metadata_merge_patch(&existing, &object);
                match api
                    .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
                    .await
                {
                    Ok(applied) => {
                        eprintln!(
                            "nodemigrate: updated metadata on CustomResourceDefinition {name} without rewriting its schema"
                        );
                        return Ok(applied);
                    }
                    Err(kube::Error::Api(response))
                        if response.code == 409 && attempt + 1 < WRITE_ATTEMPTS =>
                    {
                        continue;
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "updating metadata on CustomResourceDefinition {name} without rewriting its schema"
                            )
                        });
                    }
                }
            }
            if source_was_running
                && kind == "Pod"
                && pod_status_is_terminal_or_exited(&existing)
            {
                let uid = existing
                    .metadata
                    .uid
                    .clone()
                    .context("terminal destination Pod has no UID")?;
                match api
                    .delete(
                        name,
                        &DeleteParams {
                            grace_period_seconds: Some(0),
                            propagation_policy: Some(
                                kube::api::PropagationPolicy::Background,
                            ),
                            preconditions: Some(Preconditions {
                                uid: Some(uid),
                                resource_version: None,
                            }),
                            ..Default::default()
                        },
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(kube::Error::Api(response)) if response.code == 404 => {}
                    Err(kube::Error::Api(response))
                        if response.code == 409 && attempt + 1 < WRITE_ATTEMPTS =>
                    {
                        continue;
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("removing terminal destination Pod {name} before restart")
                        });
                    }
                }
                let mut removed = false;
                for _ in 0..50 {
                    if api
                        .get_opt(name)
                        .await
                        .with_context(|| {
                            format!("checking removal of terminal destination Pod {name}")
                        })?
                        .is_none()
                    {
                        removed = true;
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                ensure!(
                    removed,
                    "terminal destination Pod {name} was not removed before restart"
                );
                eprintln!(
                    "nodemigrate: recreated terminal Pod {name} that was Running in the source cluster"
                );
                continue;
            }
            ensure!(
                capabilities.supports_operation(verbs::UPDATE),
                "destination does not allow replacing existing {kind} objects"
            );
            ensure!(
                existing.metadata.deletion_timestamp.is_none(),
                "destination {type_meta}/{kind} {name} is terminating"
            );
            let mut replacement = object.clone();
            replacement.metadata.uid = existing.metadata.uid;
            replacement.metadata.resource_version = existing.metadata.resource_version;
            api.replace(name, &PostParams::default(), &replacement)
                .await
        } else {
            ensure!(
                capabilities.supports_operation(verbs::CREATE),
                "destination does not allow creating {kind} objects"
            );
            api.create(&PostParams::default(), &object).await
        };
        match result {
            Ok(applied) => {
                if let Some(ephemeral_containers) = &ephemeral_containers {
                    return restore_pod_ephemeral_containers(&api, name, ephemeral_containers)
                        .await
                        .with_context(|| format!("restoring ephemeral containers on Pod {name}"));
                }
                return Ok(applied);
            }
            Err(kube::Error::Api(response))
                if response.code == 409 && attempt + 1 < WRITE_ATTEMPTS =>
            {
                // Re-read before retrying: a concurrent create/update changed
                // the object version used by the preceding write.
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("migrating {type_meta}/{kind} {name} via {api_path}")
                });
            }
        }
    }
    bail!("destination kept changing {type_meta}/{kind} {name} during migration")
}

/// Ephemeral containers are valid only through the Pod's dedicated
/// subresource. Keep their specs in the protected export, omit them from
/// ordinary Pod creates/replacements, then reapply them after the Pod itself
/// exists.
fn take_pod_ephemeral_containers(object: &mut Value) -> Result<Option<Value>> {
    if object.get("kind").and_then(Value::as_str) != Some("Pod") {
        return Ok(None);
    }
    let Some(spec) = object.pointer_mut("/spec").and_then(Value::as_object_mut) else {
        return Ok(None);
    };
    let Some(ephemeral_containers) = spec.remove("ephemeralContainers") else {
        return Ok(None);
    };
    let ephemeral_containers = ephemeral_containers
        .as_array()
        .context("Pod spec.ephemeralContainers must be an array")?;
    if ephemeral_containers.is_empty() {
        return Ok(None);
    }
    Ok(Some(Value::Array(ephemeral_containers.clone())))
}

async fn restore_pod_ephemeral_containers(
    api: &Api<DynamicObject>,
    name: &str,
    ephemeral_containers: &Value,
) -> Result<DynamicObject> {
    let patch = serde_json::json!({
        "spec": {"ephemeralContainers": ephemeral_containers}
    });
    api.patch_subresource(
        "ephemeralcontainers",
        name,
        &PatchParams::default(),
        &Patch::Strategic(&patch),
    )
    .await
        .context("patching the Pod ephemeralcontainers subresource")
}

fn crd_schema_matches(existing: &DynamicObject, desired: &DynamicObject) -> bool {
    existing
        .types
        .as_ref()
        .is_some_and(|type_meta| type_meta.kind == "CustomResourceDefinition")
        && desired
            .types
            .as_ref()
            .is_some_and(|type_meta| type_meta.kind == "CustomResourceDefinition")
        && existing.data.get("spec") == desired.data.get("spec")
}

fn can_preserve_existing_crd(existing: &DynamicObject, desired: &DynamicObject) -> bool {
    crd_schema_matches(existing, desired)
        && existing.metadata.labels == desired.metadata.labels
        && existing.metadata.annotations == desired.metadata.annotations
}

fn crd_metadata_merge_patch(existing: &DynamicObject, desired: &DynamicObject) -> Value {
    fn map_patch(
        existing: Option<&BTreeMap<String, String>>,
        desired: Option<&BTreeMap<String, String>>,
    ) -> Value {
        let mut patch = serde_json::Map::new();
        if let Some(existing) = existing {
            for key in existing.keys() {
                if desired.is_none_or(|desired| !desired.contains_key(key)) {
                    patch.insert(key.clone(), Value::Null);
                }
            }
        }
        if let Some(desired) = desired {
            for (key, value) in desired {
                if existing.and_then(|existing| existing.get(key)) != Some(value) {
                    patch.insert(key.clone(), Value::String(value.clone()));
                }
            }
        }
        Value::Object(patch)
    }

    serde_json::json!({
        "metadata": {
            "labels": map_patch(existing.metadata.labels.as_ref(), desired.metadata.labels.as_ref()),
            "annotations": map_patch(
                existing.metadata.annotations.as_ref(),
                desired.metadata.annotations.as_ref(),
            ),
        }
    })
}

fn pod_status_is_terminal_or_exited(pod: &DynamicObject) -> bool {
    let Some(status) = pod.data.get("status").and_then(Value::as_object) else {
        return false;
    };
    if matches!(
        status.get("phase").and_then(Value::as_str),
        Some("Failed" | "Succeeded")
    ) {
        return true;
    }
    status
        .get("containerStatuses")
        .and_then(Value::as_array)
        .is_some_and(|containers| {
            !containers.is_empty()
                && containers.iter().all(|container| {
                    container
                        .pointer("/state/terminated")
                        .is_some_and(Value::is_object)
                })
        })
}

fn custom_resource_gvks(value: &Value) -> BTreeSet<(String, String, String)> {
    if value.get("kind").and_then(Value::as_str) != Some("CustomResourceDefinition") {
        return BTreeSet::new();
    }
    let Some(group) = value.pointer("/spec/group").and_then(Value::as_str) else {
        return BTreeSet::new();
    };
    let Some(kind) = value.pointer("/spec/names/kind").and_then(Value::as_str) else {
        return BTreeSet::new();
    };
    value
        .pointer("/spec/versions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|version| version.get("served").and_then(Value::as_bool) == Some(true))
        .filter_map(|version| version.get("name").and_then(Value::as_str))
        .map(|version| (group.to_string(), version.to_string(), kind.to_string()))
        .collect()
}

async fn wait_for_custom_resource_apis(
    client: &Client,
    expected: &BTreeSet<(String, String, String)>,
    timeout: std::time::Duration,
) -> Result<Vec<(String, String, String)>> {
    let started = std::time::Instant::now();
    let mut last_error = None;
    let mut discovery = loop {
        match discover_apis(client).await {
            Ok(discovery) => break discovery,
            Err(error) => {
                let error = error.context(
                    "discovering destination APIs after applying source CustomResourceDefinitions",
                );
                eprintln!("nodemigrate: destination CRD discovery probe failed: {error:#}");
                last_error = Some(error);
                let remaining = timeout.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Err(last_error.expect("an error was recorded"))
                        .context("destination API discovery did not recover after CRD import");
                }
                tokio::time::sleep(DISCOVERY_RETRY_DELAY.min(remaining)).await;
            }
        }
    };
    let mut missing = missing_custom_resource_apis(&discovery, expected);
    if !missing.is_empty() {
        eprintln!(
            "nodemigrate: waiting for {} of {} source CustomResourceDefinition APIs to appear in destination discovery",
            missing.len(), expected.len()
        );
    }
    while !missing.is_empty() && started.elapsed() < timeout {
        let remaining = timeout.saturating_sub(started.elapsed());
        tokio::time::sleep(std::time::Duration::from_secs(3).min(remaining)).await;
        match discover_apis(client).await {
            Ok(current) => {
                discovery = current;
                missing = missing_custom_resource_apis(&discovery, expected);
                last_error = None;
            }
            Err(error) => {
                let error = error.context(
                    "refreshing destination API discovery for imported CustomResourceDefinitions",
                );
                eprintln!("nodemigrate: destination CRD discovery probe failed: {error:#}");
                last_error = Some(error);
            }
        }
    }
    if missing.is_empty() {
        return Ok(missing);
    }
    if started.elapsed() >= timeout {
        if let Some(error) = last_error {
            return Err(error)
                .context("destination API discovery kept failing while waiting for imported CRDs");
        }
    }
    Ok(missing)
}

async fn destination_crd_readback(client: &Client, discovery: &Discovery) -> Result<String> {
    let (resource, _) = find_resource(
        discovery,
        "CustomResourceDefinition",
        "apiextensions.k8s.io/v1",
    )
    .context("destination discovery has no apiextensions.k8s.io/v1 CustomResourceDefinition")?;
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &resource);
    let list = api
        .list(&ListParams::default())
        .await
        .context("listing destination CustomResourceDefinitions")?;
    let descriptions = list
        .items
        .iter()
        .map(|crd| -> Result<String> {
            let value = serde_json::to_value(crd).context("serializing destination CRD")?;
            let name = crd.metadata.name.as_deref().unwrap_or("<unnamed>");
            let conditions = value
                .pointer("/status/conditions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|condition| {
                    let kind = condition.get("type").and_then(Value::as_str)?;
                    let status = condition.get("status").and_then(Value::as_str)?;
                    let reason = condition
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown reason");
                    Some(format!("{kind}={status} ({reason})"))
                })
                .collect::<Vec<_>>();
            if conditions.is_empty() {
                Ok(format!("{name}[no status conditions]"))
            } else {
                Ok(format!("{name}[{}]", conditions.join(", ")))
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(format!(
        "{} CRDs: {}",
        descriptions.len(),
        descriptions.join("; ")
    ))
}

fn missing_custom_resource_apis(
    discovery: &Discovery,
    expected: &BTreeSet<(String, String, String)>,
) -> Vec<(String, String, String)> {
    expected
        .iter()
        .filter(|(group, version, kind)| {
            find_resource(discovery, kind, &format!("{group}/{version}")).is_none()
        })
        .cloned()
        .collect()
}

fn object_type_label(value: &Value) -> String {
    let api_version = value
        .get("apiVersion")
        .and_then(Value::as_str)
        .unwrap_or("unknown apiVersion");
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("unknown kind");
    format!("{api_version}/{kind}")
}

fn summarize_import_failures(failures: &[(String, String)]) -> String {
    let mut grouped: BTreeMap<&str, (usize, Vec<&str>)> = BTreeMap::new();
    for (object_type, error) in failures {
        let (count, object_types) = grouped.entry(error).or_default();
        *count += 1;
        object_types.push(object_type);
    }
    grouped
        .into_iter()
        .map(|(error, (count, object_types))| {
            let mut types = object_types;
            types.sort_unstable();
            types.dedup();
            format!("{} object type(s) [{}]: {error}", count, types.join(", "))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn is_source_custom_resource(
    object: &Value,
    source_crd_apis: &BTreeSet<(String, String, String)>,
) -> bool {
    let Some(api_version) = object.get("apiVersion").and_then(Value::as_str) else {
        return false;
    };
    let Some((group, version)) = api_version.split_once('/') else {
        return false;
    };
    let Some(kind) = object.get("kind").and_then(Value::as_str) else {
        return false;
    };
    source_crd_apis.contains(&(group.to_string(), version.to_string(), kind.to_string()))
}

fn retryable_import_error(error: &anyhow::Error, retry_not_found: bool) -> bool {
    error.chain().any(|cause| {
        if let Some(kube_error) = cause.downcast_ref::<kube::Error>() {
            return match kube_error {
                kube::Error::Api(status) => {
                    (status.code == 404 && retry_not_found)
                        || matches!(status.code, 408 | 409 | 429 | 500 | 502 | 503 | 504)
                }
                kube::Error::HyperError(_) | kube::Error::Service(_) => true,
                _ => false,
            };
        }
        retry_not_found && cause.to_string().contains("destination does not expose ")
    })
}

fn skip_kind_reason(kind: &str) -> Option<SkipReason> {
    if !SKIP_KINDS.contains(&kind) {
        return None;
    }
    Some(match kind {
        "ComponentStatus" => SkipReason::ReadOnlyComponentStatus,
        "Event" => SkipReason::EphemeralEvents,
        "Node" => SkipReason::NodeReregistration,
        "NodeMetrics" | "PodMetrics" => SkipReason::LiveMetrics,
        "VolumeAttachment" => SkipReason::CsiReattachment,
        _ => return None,
    })
}

fn label_value<'a>(object: &'a Value, key: &str) -> Option<&'a str> {
    object
        .pointer("/metadata/labels")
        .and_then(Value::as_object)
        .and_then(|labels| labels.get(key))
        .and_then(Value::as_str)
}

fn is_default_kubernetes_service_endpoint(object: &Value) -> bool {
    object
        .pointer("/metadata/namespace")
        .and_then(Value::as_str)
        == Some("default")
        && (object.pointer("/metadata/name").and_then(Value::as_str) == Some("kubernetes")
            || label_value(object, "kubernetes.io/service-name") == Some("kubernetes"))
}

fn regenerated_endpoint_skip_reason(object: &Value, kind: &str) -> Option<SkipReason> {
    match kind {
        "Endpoints" => {
            if is_default_kubernetes_service_endpoint(object) {
                Some(SkipReason::ApiServiceRouting)
            } else if label_value(object, "endpoints.kubernetes.io/managed-by")
                == Some("endpoint-controller")
            {
                Some(SkipReason::ControllerManagedEndpoints)
            } else {
                None
            }
        }
        "EndpointSlice" => {
            if is_default_kubernetes_service_endpoint(object) {
                Some(SkipReason::ApiServiceRouting)
            } else if matches!(
                label_value(object, "endpointslice.kubernetes.io/managed-by"),
                Some(
                    "endpointslice-controller.k8s.io"
                        | "endpointslicemirroring-controller.k8s.io"
                        | "nodecontroller"
                )
            ) {
                Some(SkipReason::ControllerManagedEndpoints)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn object_skip_reason(object: &Value) -> Option<SkipReason> {
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(reason) = skip_kind_reason(kind) {
        return Some(reason);
    }
    if let Some(reason) = regenerated_endpoint_skip_reason(object, kind) {
        return Some(reason);
    }
    let api_group = object
        .get("apiVersion")
        .and_then(Value::as_str)
        .and_then(|version| version.split_once('/').map(|(group, _)| group));
    let name = object.pointer("/metadata/name").and_then(Value::as_str);
    if api_group == Some("rbac.authorization.k8s.io")
        && matches!(kind, "Role" | "RoleBinding" | "ClusterRole" | "ClusterRoleBinding")
        && name.is_some_and(|name| name.starts_with("nodebootstrap:"))
    {
        return Some(SkipReason::NodebootstrapRuntimeRbac);
    }
    if api_group == Some("cilium.io") {
        match kind {
            "CiliumEndpoint" => return Some(SkipReason::CiliumEndpointReconciliation),
            "CiliumIdentity" => return Some(SkipReason::CiliumIdentityReconciliation),
            "CiliumNode" => return Some(SkipReason::CiliumNodeReconciliation),
            _ => {}
        }
    }
    // These ConfigMaps contain control-plane trust material. Carrying source
    // certificates into the destination would make aggregated API servers
    // reject the destination front-proxy identity. The destination apiserver
    // and nodebootstrap publish the corresponding local trust bundles.
    let config_map_name = object.pointer("/metadata/name").and_then(Value::as_str);
    let config_map_namespace = object
        .pointer("/metadata/namespace")
        .and_then(Value::as_str);
    if kind == "ConfigMap"
        && (config_map_name == Some("kube-root-ca.crt")
            || (config_map_namespace == Some("kube-system")
                && config_map_name == Some("extension-apiserver-authentication")))
    {
        return Some(SkipReason::DestinationTrustBundle);
    }
    // Kubelets renew these node-heartbeat Leases continuously; the target
    // kubelet must create a fresh Lease for its newly registered Node. Other
    // Leases can carry application or add-on state and are migrated.
    if kind == "Lease"
        && object
            .pointer("/metadata/namespace")
            .and_then(Value::as_str)
            == Some("kube-node-lease")
    {
        return Some(SkipReason::NodeHeartbeatLease);
    }
    if kind == "Pod" {
        let annotations = object
            .pointer("/metadata/annotations")
            .and_then(Value::as_object);
        let is_static_pod_mirror = annotations
            .is_some_and(|annotations| annotations.contains_key("kubernetes.io/config.mirror"));
        let is_controller_owned = object
            .pointer("/metadata/ownerReferences")
            .and_then(Value::as_array)
            .is_some_and(|references| {
                references.iter().any(|reference| {
                    reference
                        .get("controller")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
            });
        if is_static_pod_mirror || is_controller_owned {
            return Some(if is_static_pod_mirror {
                SkipReason::StaticPodMirror
            } else {
                SkipReason::ControllerOwnedPod
            });
        }
    }
    None
}

fn skip_object(object: &Value) -> bool {
    object_skip_reason(object).is_some()
}

fn sanitize(mut object: Value) -> Option<SanitizedObject> {
    if skip_object(&object) {
        return None;
    }
    if object.get("kind").and_then(Value::as_str) == Some("Pod")
        && object
            .pointer("/status/phase")
            .and_then(Value::as_str)
            == Some("Running")
    {
        // Pod status is not portable across clusters. Keep this source-state
        // bit in the protected export so a same-name terminal Pod on return
        // can be recreated and run again; apply_object strips it before API
        // writes.
        object["_nodemigrateSourceWasRunning"] = Value::Bool(true);
    }
    object.as_object_mut()?.remove("status");
    let metadata = object.get_mut("metadata")?.as_object_mut()?;
    let source_uid = metadata
        .remove("uid")
        .and_then(|value| value.as_str().map(str::to_owned));
    for field in [
        "creationTimestamp",
        "deletionTimestamp",
        "deletionGracePeriodSeconds",
        "generation",
        "managedFields",
        "resourceVersion",
        "selfLink",
    ] {
        metadata.remove(field);
    }
    Some(SanitizedObject {
        value: object,
        source_uid,
    })
}

fn write_export_manifest(
    directory: &Path,
    objects: &[ExportedObject],
    node_states: &BTreeMap<String, NodeSchedulingState>,
) -> Result<()> {
    let entries = objects
        .iter()
        .map(|object| {
            let file = object
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .context("export object path has no UTF-8 filename")?;
            Ok(ExportManifestObject {
                file: file.to_owned(),
                source_uid: object.source_uid.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let manifest = ExportManifest {
        format_version: 2,
        objects: entries,
        node_states: node_states.clone(),
    };
    let path = directory.join("manifest.json");
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("creating protected migration manifest {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    serde_json::to_writer_pretty(&file, &manifest)
        .with_context(|| format!("writing protected migration manifest {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing protected migration manifest {}", path.display()))?;
    fs::File::open(directory)
        .with_context(|| format!("opening migration export directory {}", directory.display()))?
        .sync_all()
        .with_context(|| format!("syncing migration export directory {}", directory.display()))?;
    Ok(())
}

fn is_export_object_filename(filename: &str) -> bool {
    let bytes = filename.as_bytes();
    bytes.len() == 13
        && bytes[..8].iter().all(|byte| byte.is_ascii_digit())
        && &filename[8..] == ".json"
}

fn object_rank(object: &Value) -> u8 {
    match object
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "Namespace" => 0,
        "CustomResourceDefinition" => 1,
        // These cluster-scoped references must exist before Pods are
        // admitted. Source API discovery order is not a dependency order.
        "PriorityClass" | "StorageClass" => 2,
        "ServiceAccount" => 3,
        "Secret" => 4,
        // Bind claims before their volumes so import can remap a PV's
        // claimRef UID before the PV write reaches the destination binder.
        "PersistentVolumeClaim" => 5,
        "PersistentVolume" => 6,
        _ => 7,
    }
}

fn prepare_initial_import_object(value: &Value, uid_map: &HashMap<String, String>) -> Value {
    let mut initial = value.clone();
    if initial.get("kind").and_then(Value::as_str) == Some("CSINode") {
        // Kubernetes requires CSINode.spec.drivers even when no CSI drivers
        // are registered. A source that omits spec entirely still means the
        // empty driver set; materialize both levels for strict destinations.
        if !initial.get("spec").is_some_and(Value::is_object) {
            initial["spec"] = serde_json::json!({});
        }
        if let Some(spec) = initial.pointer_mut("/spec").and_then(Value::as_object_mut) {
            if !spec.get("drivers").is_some_and(Value::is_array) {
                spec.insert("drivers".to_string(), Value::Array(Vec::new()));
            }
        }
    }
    if let Some(metadata) = initial
        .pointer_mut("/metadata")
        .and_then(Value::as_object_mut)
    {
        metadata.remove("ownerReferences");
    }
    if initial.get("kind").and_then(Value::as_str) == Some("PersistentVolume") {
        if let Some(claim_ref) = initial
            .pointer_mut("/spec/claimRef")
            .and_then(Value::as_object_mut)
        {
            if let Some(source_uid) = claim_ref.get("uid").and_then(Value::as_str) {
                if let Some(destination_uid) = uid_map.get(source_uid) {
                    claim_ref.insert("uid".to_string(), Value::String(destination_uid.clone()));
                } else {
                    claim_ref.remove("uid");
                }
            }
        }
    }
    initial
}

fn export_directory() -> Result<PathBuf> {
    let base = std::env::var_os("NODEMIGRATE_EXPORT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/nodemigrate/exports"));
    fs::create_dir_all(&base)
        .with_context(|| format!("creating migration export directory {}", base.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700))?;
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("reading system clock")?
        .as_secs();
    for suffix in 0..1000 {
        let dir = base.join(format!("{timestamp}-{}-{suffix}", std::process::id()));
        match fs::create_dir(&dir) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
                }
                return Ok(dir);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating export directory {}", dir.display()))
            }
        }
    }
    bail!("could not allocate a unique migration export directory")
}

#[cfg(test)]
mod tests {
    use super::{
        can_preserve_existing_crd, crd_metadata_merge_patch, crd_schema_matches,
        custom_resource_gvks, is_source_custom_resource, kubeconfig_root_ca,
        namespace_ca_bundle_matches, node_is_ready_replacement, node_scheduling_patch, object_rank,
        node_uid_has_been_replaced, object_skip_reason, object_type_label,
        owner_reference_repair_patch, parse_cilium_kube_proxy_replacement,
        persistent_host_paths, pod_status_is_terminal_or_exited, prepare_initial_import_object,
        preserve_discovered_type_meta, remapped_node_owner_references, restore_cni_path_backups,
        retryable_import_error, same_group_kind, sanitize, service_account_token_secret_patch,
        service_account_token_secret_value, skip_kind_reason, skip_object, snapshot_k3s_cni_paths,
        summarize_import_failures, take_pod_ephemeral_containers, write_export_manifest,
        ApiResource, DynamicObject, Export, ExportedObject, KubeApi, NodeSchedulingState,
        SkipReason,
    };
    use crate::detect::{ClusterConfig, Installation, K3sDatastore, NodeRole, ServiceManager};
    use crate::request::Distribution;
    use std::collections::{BTreeMap, HashMap};
    use std::fs;

    #[test]
    fn parses_cilium_service_proxy_mode() {
        assert!(!parse_cilium_kube_proxy_replacement(None).unwrap());
        assert!(!parse_cilium_kube_proxy_replacement(Some("false")).unwrap());
        assert!(parse_cilium_kube_proxy_replacement(Some("true")).unwrap());
        assert!(parse_cilium_kube_proxy_replacement(Some("strict")).unwrap());
        assert!(parse_cilium_kube_proxy_replacement(Some("unknown")).is_err());
    }

    #[test]
    fn unchanged_crd_schema_is_not_rewritten_during_return_migration() {
        let source: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {
                "name": "ingressroutes.traefik.io",
                "labels": {
                    "app.kubernetes.io/managed-by": "Helm",
                    "obsolete": "remove"
                },
                "annotations": {"meta.helm.sh/release-name": "traefik"}
            },
            "spec": {
                "versions": [{
                    "schema": {"maximum": 9223372036854775000u64}
                }]
            }
        }))
        .unwrap();
        let existing = source.clone();

        assert!(can_preserve_existing_crd(&existing, &source));

        let changed_schema: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {
                "name": "ingressroutes.traefik.io",
                "labels": {"app.kubernetes.io/managed-by": "Helm"},
                "annotations": {"meta.helm.sh/release-name": "traefik"}
            },
            "spec": {
                "versions": [{
                    "schema": {"maximum": 9223372036854776000u64}
                }]
            }
        }))
        .unwrap();
        assert!(!crd_schema_matches(&existing, &changed_schema));
        assert!(!can_preserve_existing_crd(&existing, &changed_schema));

        let genuinely_changed_schema: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {
                "name": "ingressroutes.traefik.io",
                "labels": {
                    "app.kubernetes.io/managed-by": "Helm",
                    "obsolete": "remove"
                },
                "annotations": {"meta.helm.sh/release-name": "traefik"}
            },
            "spec": {
                "versions": [{
                    "schema": {"maximum": 9223372036854780000u64}
                }]
            }
        }))
        .unwrap();
        assert!(!can_preserve_existing_crd(&existing, &genuinely_changed_schema));

        let changed_metadata: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {
                "name": "ingressroutes.traefik.io",
                "labels": {"app.kubernetes.io/managed-by": "other"},
                "annotations": {"meta.helm.sh/release-name": "traefik-moved"}
            },
            "spec": {
                "versions": [{
                    "schema": {"maximum": 9223372036854775000u64}
                }]
            }
        }))
        .unwrap();
        assert!(crd_schema_matches(&existing, &changed_metadata));
        assert!(!can_preserve_existing_crd(&existing, &changed_metadata));
        let metadata_patch = crd_metadata_merge_patch(&existing, &changed_metadata);
        assert_eq!(
            metadata_patch.pointer("/metadata/labels/app.kubernetes.io~1managed-by"),
            Some(&serde_json::json!("other"))
        );
        assert_eq!(
            metadata_patch.pointer("/metadata/labels/obsolete"),
            Some(&serde_json::Value::Null)
        );
        assert_eq!(
            metadata_patch.pointer("/metadata/annotations/meta.helm.sh~1release-name"),
            Some(&serde_json::json!("traefik-moved"))
        );
    }

    #[test]
    fn migration_makes_an_omitted_empty_csinode_driver_list_explicit() {
        let source = serde_json::json!({
            "apiVersion": "storage.k8s.io/v1",
            "kind": "CSINode",
            "metadata": {"name": "worker-2"},
            "spec": {}
        });
        let normalized = prepare_initial_import_object(&source, &HashMap::new());
        assert_eq!(normalized.pointer("/spec/drivers"), Some(&serde_json::json!([])));
        assert_eq!(source.pointer("/spec/drivers"), None);
    }

    #[test]
    fn migration_materializes_an_omitted_csinode_spec_and_driver_list() {
        let source = serde_json::json!({
            "apiVersion": "storage.k8s.io/v1",
            "kind": "CSINode",
            "metadata": {"name": "worker-2"}
        });
        let normalized = prepare_initial_import_object(&source, &HashMap::new());
        assert_eq!(normalized.pointer("/spec/drivers"), Some(&serde_json::json!([])));
        assert!(source.get("spec").is_none());
    }

    #[test]
    fn pod_ephemeral_containers_are_removed_from_regular_write_and_kept_for_subresource() {
        let mut object = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "debugged"},
            "spec": {
                "containers": [{"name": "app", "image": "busybox"}],
                "ephemeralContainers": [{
                    "name": "debugger",
                    "image": "busybox",
                    "command": ["sh", "-c", "sleep 600"]
                }]
            }
        });
        let ephemeral_containers = take_pod_ephemeral_containers(&mut object).unwrap();

        assert_eq!(
            ephemeral_containers,
            Some(serde_json::json!([{
                "name": "debugger",
                "image": "busybox",
                "command": ["sh", "-c", "sleep 600"]
            }]))
        );
        assert_eq!(object.pointer("/spec/ephemeralContainers"), None);
        assert_eq!(
            object.pointer("/spec/containers/0/name"),
            Some(&serde_json::json!("app"))
        );
    }

    #[test]
    fn ephemeral_container_extraction_does_not_change_non_pod_objects() {
        let mut object = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "spec": {"template": {"spec": {"ephemeralContainers": []}}}
        });
        let original = object.clone();

        assert_eq!(take_pod_ephemeral_containers(&mut object).unwrap(), None);
        assert_eq!(object, original);
    }

    #[test]
    fn pod_with_empty_ephemeral_container_list_needs_no_subresource_write() {
        let mut object = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "spec": {"ephemeralContainers": []}
        });

        assert_eq!(take_pod_ephemeral_containers(&mut object).unwrap(), None);
        assert_eq!(object.pointer("/spec/ephemeralContainers"), None);
    }

    #[test]
    fn migration_replaces_a_null_csinode_driver_list_with_an_empty_list() {
        let source = serde_json::json!({
            "apiVersion": "storage.k8s.io/v1",
            "kind": "CSINode",
            "metadata": {"name": "worker-2"},
            "spec": {"drivers": null}
        });
        let normalized = prepare_initial_import_object(&source, &HashMap::new());
        assert_eq!(normalized.pointer("/spec/drivers"), Some(&serde_json::json!([])));
        assert_eq!(
            source.pointer("/spec/drivers"),
            Some(&serde_json::Value::Null)
        );
    }

    #[test]
    fn csinode_owner_reference_patch_preserves_required_driver_list() {
        let current = serde_json::json!({
            "metadata": {"resourceVersion": "12"},
            "spec": {"drivers": [{"name": "hostpath.csi.k8s.io"}]}
        });
        let patch = owner_reference_repair_patch(
            &current,
            vec![serde_json::json!({"kind": "Node", "name": "cp-1", "uid": "new"})],
            true,
        );
        assert_eq!(
            patch.pointer("/spec/drivers"),
            current.pointer("/spec/drivers")
        );
    }

    #[test]
    fn csinode_owner_reference_patch_materializes_missing_driver_list() {
        let current = serde_json::json!({"metadata": {"resourceVersion": "12"}});
        let patch = owner_reference_repair_patch(&current, Vec::new(), true);
        assert_eq!(patch.pointer("/spec/drivers"), Some(&serde_json::json!([])));
    }

    #[test]
    fn source_api_server_name_uses_the_current_kubeconfig_context() {
        let directory = tempfile::tempdir().expect("create kubeconfig directory");
        let kubeconfig = directory.path().join("admin.conf");
        fs::write(
            &kubeconfig,
            "apiVersion: v1\nkind: Config\ncurrent-context: migrate\ncontexts:\n- name: other\n  context:\n    cluster: other-cluster\n    user: admin\n- name: migrate\n  context:\n    cluster: source\n    user: admin\nclusters:\n- name: other-cluster\n  cluster:\n    server: https://wrong.example.test:6443\n- name: source\n  cluster:\n    server: https://cp-1:6443/api\nusers:\n- name: admin\n  user: {}\n",
        )
        .expect("write kubeconfig");
        let api = KubeApi { kubeconfig };
        assert_eq!(api.api_server_name().expect("read endpoint name"), "cp-1");
    }

    #[test]
    fn source_api_server_name_unbrackets_ipv6_authorities() {
        let directory = tempfile::tempdir().expect("create kubeconfig directory");
        let kubeconfig = directory.path().join("admin.conf");
        fs::write(
            &kubeconfig,
            "apiVersion: v1\nkind: Config\ncurrent-context: migrate\ncontexts:\n- name: migrate\n  context:\n    cluster: source\nclusters:\n- name: source\n  cluster:\n    server: https://[2001:db8::1]:6443\n",
        )
        .expect("write kubeconfig");
        let api = KubeApi { kubeconfig };
        assert_eq!(api.api_server_name().expect("read endpoint name"), "2001:db8::1");
    }

    #[test]
    fn dynamic_crd_object_round_trip_preserves_large_schema_bound_numbers() {
        let integer = serde_json::json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {"name": "ingressroutes.traefik.io"},
            "spec": {"versions": [{"schema": {"openAPIV3Schema": {
                "properties": {"spec": {"properties": {"routes": {"items": {
                    "properties": {"priority": {"maximum": 9_223_372_036_854_775_000_i64}}
                }}}}}
                }}}]}
        });
        let floating = serde_json::json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {"name": "ingressroutes.traefik.io"},
            "spec": {"versions": [{"schema": {"openAPIV3Schema": {
                "properties": {"spec": {"properties": {"routes": {"items": {
                    "properties": {"priority": {
                        "maximum": 9_223_372_036_854_775_000_i64 as f64
                    }}
                }}}}}
            }}}]}
        });
        for source in [integer, floating] {
            let dynamic: DynamicObject = serde_json::from_value(source.clone()).unwrap();
            let round_tripped = serde_json::to_value(dynamic).unwrap();
            assert_eq!(
                round_tripped, source,
                "DynamicObject conversion must not move a CRD bound to an adjacent double"
            );
        }
    }

    #[test]
    fn reads_cilium_service_proxy_mode_from_protected_export() {
        let directory = tempfile::tempdir().unwrap();
        let object_path = directory.path().join("000001.json");
        let proxy_path = directory.path().join("000002.json");
        fs::write(
            &object_path,
            br#"{"apiVersion":"v1","kind":"ConfigMap","metadata":{"namespace":"kube-system","name":"cilium-config"},"data":{"kube-proxy-replacement":"strict"}}"#,
        )
        .unwrap();
        fs::write(
            &proxy_path,
            br#"{"apiVersion":"apps/v1","kind":"DaemonSet","metadata":{"namespace":"kube-system","name":"kube-proxy"}}"#,
        )
        .unwrap();
        let export = Export {
            dir: directory.path().to_path_buf(),
            objects: vec![ExportedObject {
                path: object_path,
                source_uid: None,
            },
            ExportedObject {
                path: proxy_path,
                source_uid: None,
            }],
            node_states: BTreeMap::new(),
            host_paths: Vec::new(),
            host_path_backups: Vec::new(),
            cni_path_backups: Vec::new(),
        };

        assert!(export.cilium_kube_proxy_replacement().unwrap());
        assert!(export.kube_proxy_daemonset_present().unwrap());
    }

    #[test]
    fn node_owner_references_wait_for_the_replacement_node_uid() {
        let source_references = [
            serde_json::json!({
                "apiVersion": "v1", "kind": "Node", "name": "worker-a",
                "uid": "old-node-uid", "controller": true
            }),
            serde_json::json!({
                "apiVersion": "v1", "kind": "Secret", "name": "node-credential",
                "uid": "source-secret-uid"
            }),
            serde_json::json!({
                "apiVersion": "v1", "kind": "Node", "name": "worker-b",
                "uid": "other-node-uid"
            }),
        ];
        let destination_references = [serde_json::json!({
            "apiVersion": "v1", "kind": "Secret", "name": "node-credential",
            "uid": "destination-secret-uid"
        })];

        let remapped = remapped_node_owner_references(
            &source_references,
            &destination_references,
            "worker-a",
            "old-node-uid",
            "new-node-uid",
        );

        assert_eq!(remapped.len(), 2);
        assert_eq!(remapped[0], destination_references[0]);
        assert_eq!(remapped[1]["kind"], "Node");
        assert_eq!(remapped[1]["name"], "worker-a");
        assert_eq!(remapped[1]["uid"], "new-node-uid");
        assert!(!remapped.iter().any(|reference| {
            reference.get("name").and_then(serde_json::Value::as_str) == Some("worker-b")
        }));
    }

    #[test]
    fn namespace_ca_bundle_must_match_destination_ca_exactly() {
        let mut configmap = k8s_openapi::api::core::v1::ConfigMap::default();
        configmap.data = Some(BTreeMap::from([(
            "ca.crt".to_string(),
            "-----BEGIN CERTIFICATE-----\ncurrent\n-----END CERTIFICATE-----\n".to_string(),
        )]));
        assert!(namespace_ca_bundle_matches(
            &configmap,
            "-----BEGIN CERTIFICATE-----\ncurrent\n-----END CERTIFICATE-----\n"
        ));
        assert!(!namespace_ca_bundle_matches(
            &configmap,
            "-----BEGIN CERTIFICATE-----\nold\n-----END CERTIFICATE-----\n"
        ));
        configmap.data = None;
        assert!(!namespace_ca_bundle_matches(&configmap, "current"));
    }

    #[test]
    fn cluster_scoped_pod_dependencies_are_imported_before_workloads() {
        let priority_class = serde_json::json!({"kind": "PriorityClass"});
        let storage_class = serde_json::json!({"kind": "StorageClass"});
        let pvc = serde_json::json!({"kind": "PersistentVolumeClaim"});
        let pv = serde_json::json!({"kind": "PersistentVolume"});
        let pod = serde_json::json!({"kind": "Pod"});

        assert!(object_rank(&priority_class) < object_rank(&pod));
        assert!(object_rank(&storage_class) < object_rank(&pod));
        assert!(object_rank(&pvc) < object_rank(&pv));
        assert!(object_rank(&pv) < object_rank(&pod));
    }

    #[test]
    fn initial_pv_import_remaps_claim_uid_before_the_volume_write() {
        let source_uid = "nodestore-pvc-uid";
        let destination_uid = "retained-cluster-pvc-uid";
        let uid_map = HashMap::from([(source_uid.to_string(), destination_uid.to_string())]);
        let source_pv = serde_json::json!({
            "apiVersion": "v1",
            "kind": "PersistentVolume",
            "metadata": {
                "name": "data",
                "ownerReferences": [{"uid": "obsolete-owner"}]
            },
            "spec": {"claimRef": {"name": "data", "uid": source_uid}}
        });

        let initial = prepare_initial_import_object(&source_pv, &uid_map);

        assert_eq!(initial["spec"]["claimRef"]["uid"], destination_uid);
        assert!(initial["metadata"].get("ownerReferences").is_none());
        assert_eq!(source_pv["spec"]["claimRef"]["uid"], source_uid);
    }

    #[test]
    fn initial_pv_import_clears_an_unmapped_claim_uid_for_later_repair() {
        let source_pv = serde_json::json!({
            "apiVersion": "v1",
            "kind": "PersistentVolume",
            "metadata": {"name": "data"},
            "spec": {"claimRef": {"name": "data", "uid": "not-imported"}}
        });

        let initial = prepare_initial_import_object(&source_pv, &HashMap::new());

        assert!(initial.pointer("/spec/claimRef/uid").is_none());
    }

    #[test]
    fn destination_ca_is_read_from_the_selected_kubeconfig_cluster() {
        use base64::Engine;

        let directory = tempfile::tempdir().expect("temporary kubeconfig directory");
        let kubeconfig_path = directory.path().join("admin.conf");
        let expected = "-----BEGIN CERTIFICATE-----\ncluster-ca\n-----END CERTIFICATE-----\n";
        let encoded = base64::engine::general_purpose::STANDARD.encode(expected);
        fs::write(
            &kubeconfig_path,
            format!(
                "apiVersion: v1\nkind: Config\ncurrent-context: target\nclusters:\n- name: target\n  cluster:\n    server: https://127.0.0.1:6443\n    certificate-authority-data: {encoded}\ncontexts:\n- name: target\n  context:\n    cluster: target\n    user: admin\nusers:\n- name: admin\n  user: {{}}\n"
            ),
        )
        .expect("write kubeconfig");

        assert_eq!(kubeconfig_root_ca(&kubeconfig_path).unwrap(), expected);
    }

    #[test]
    fn compatible_api_version_requires_the_same_group_and_kind() {
        let trust_bundle = ApiResource {
            group: "certificates.k8s.io".to_string(),
            version: "v1beta1".to_string(),
            api_version: "certificates.k8s.io/v1beta1".to_string(),
            kind: "ClusterTrustBundle".to_string(),
            plural: "clustertrustbundles".to_string(),
        };

        assert!(same_group_kind(
            &trust_bundle,
            "ClusterTrustBundle",
            "certificates.k8s.io/v1"
        ));
        assert!(!same_group_kind(
            &trust_bundle,
            "CertificateSigningRequest",
            "certificates.k8s.io/v1"
        ));
        assert!(!same_group_kind(
            &trust_bundle,
            "ClusterTrustBundle",
            "example.com/v1"
        ));
    }

    #[test]
    fn import_wait_tracks_only_served_custom_resource_versions() {
        let definition = serde_json::json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "spec": {
                "group": "cilium.io",
                "names": {"kind": "CiliumEndpoint"},
                "versions": [
                    {"name": "v2", "served": true},
                    {"name": "v1alpha1", "served": false}
                ]
            }
        });

        let apis = custom_resource_gvks(&definition);

        assert_eq!(
            apis,
            [(
                "cilium.io".to_string(),
                "v2".to_string(),
                "CiliumEndpoint".to_string()
            )]
            .into_iter()
            .collect()
        );
        assert!(custom_resource_gvks(&serde_json::json!({"kind": "Deployment"})).is_empty());
    }

    #[test]
    fn custom_resource_retry_classification_uses_source_crd_versions() {
        let apis = custom_resource_gvks(&serde_json::json!({
            "kind": "CustomResourceDefinition",
            "spec": {
                "group": "widgets.example.com",
                "names": {"kind": "Widget"},
                "versions": [{"name": "v1", "served": true}]
            }
        }));

        assert!(is_source_custom_resource(
            &serde_json::json!({"apiVersion":"widgets.example.com/v1","kind":"Widget"}),
            &apis
        ));
        assert!(!is_source_custom_resource(
            &serde_json::json!({"apiVersion":"v1","kind":"PersistentVolume"}),
            &apis
        ));
        assert!(!is_source_custom_resource(
            &serde_json::json!({"apiVersion":"widgets.example.com/v2","kind":"Widget"}),
            &apis
        ));
    }

    #[test]
    fn import_failure_summary_groups_missing_destination_apis() {
        let failures = vec![
            (
                "cilium.io/v2/CiliumEndpoint".to_string(),
                "destination does not expose cilium.io/v2/CiliumEndpoint".to_string(),
            ),
            (
                "cilium.io/v2/CiliumEndpoint".to_string(),
                "destination does not expose cilium.io/v2/CiliumEndpoint".to_string(),
            ),
            (
                "snapshot.storage.k8s.io/v1/VolumeSnapshotClass".to_string(),
                "destination does not expose snapshot.storage.k8s.io/v1/VolumeSnapshotClass"
                    .to_string(),
            ),
        ];

        let summary = summarize_import_failures(&failures);

        assert!(summary.contains(
            "2 object type(s) [cilium.io/v2/CiliumEndpoint]: destination does not expose cilium.io/v2/CiliumEndpoint"
        ));
        assert!(summary.contains(
            "1 object type(s) [snapshot.storage.k8s.io/v1/VolumeSnapshotClass]: destination does not expose snapshot.storage.k8s.io/v1/VolumeSnapshotClass"
        ));
        assert_eq!(
            object_type_label(&serde_json::json!({
                "apiVersion": "cilium.io/v2",
                "kind": "CiliumEndpoint"
            })),
            "cilium.io/v2/CiliumEndpoint"
        );
    }

    #[test]
    fn import_retries_transient_api_errors_but_not_permanent_rejections() {
        let unavailable: kube::core::Status = serde_json::from_value(serde_json::json!({
            "kind": "Status",
            "apiVersion": "v1",
            "status": "Failure",
            "message": "admission webhook unavailable",
            "reason": "InternalError",
            "code": 500
        }))
        .unwrap();
        let invalid: kube::core::Status = serde_json::from_value(serde_json::json!({
            "kind": "Status",
            "apiVersion": "v1",
            "status": "Failure",
            "message": "invalid object",
            "reason": "Invalid",
            "code": 422
        }))
        .unwrap();
        let not_found: kube::core::Status = serde_json::from_value(serde_json::json!({
            "kind": "Status",
            "apiVersion": "v1",
            "status": "Failure",
            "message": "the server could not find the requested resource",
            "reason": "NotFound",
            "code": 404
        }))
        .unwrap();

        assert!(retryable_import_error(
            &anyhow::Error::new(kube::Error::Api(Box::new(unavailable,))),
            false
        ));
        assert!(!retryable_import_error(
            &anyhow::Error::new(kube::Error::Api(Box::new(invalid,))),
            false
        ));
        assert!(!retryable_import_error(
            &anyhow::Error::new(kube::Error::Api(Box::new(not_found.clone(),))),
            false
        ));
        assert!(retryable_import_error(
            &anyhow::Error::new(kube::Error::Api(Box::new(not_found,))),
            true
        ));
        assert!(!retryable_import_error(
            &anyhow::anyhow!("destination does not expose apps/v1/Deployment"),
            false
        ));
        assert!(retryable_import_error(
            &anyhow::anyhow!("destination does not expose example.io/v1/Widget"),
            true
        ));
    }

    #[test]
    fn constructs_kubernetes_client_inside_its_runtime_context() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let temp = tempfile::tempdir().unwrap();
        let kubeconfig = temp.path().join("config");
        fs::write(
            &kubeconfig,
            r"apiVersion: v1
kind: Config
clusters:
- name: test
  cluster:
    server: https://127.0.0.1:6443
    insecure-skip-tls-verify: true
users:
- name: test
  user:
    token: test-token
contexts:
- name: test
  context:
    cluster: test
    user: test
current-context: test
",
        )
        .unwrap();

        let (_runtime, _client) = KubeApi { kubeconfig }
            .connected()
            .expect("Kubernetes client construction should have a Tokio runtime context");
    }

    #[test]
    fn restores_k3s_cni_directories_after_data_dir_removal() {
        let host = tempfile::tempdir().unwrap();
        let exports = tempfile::tempdir().unwrap();
        let data_dir = host.path().join("var/lib/rancher/k3s");
        let conf_dir = data_dir.join("agent/etc/cni/net.d");
        let bin_dir = data_dir.join("data/current/bin");
        fs::create_dir_all(&conf_dir).unwrap();
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(conf_dir.join("05-cilium.conflist"), "cilium-config").unwrap();
        fs::write(bin_dir.join("cilium-cni"), b"cni-binary").unwrap();
        let installation = Installation {
            distribution: Distribution::K3s,
            role: NodeRole::ControlPlane,
            runtime_endpoint: None,
            service_manager: Some(ServiceManager::Systemd),
            service_name: "k3s".to_string(),
            service_file: None,
            binary: None,
            config_files: Vec::new(),
            cluster: Some(ClusterConfig {
                data_dir: data_dir.clone(),
                kubeconfig: None,
                service_cidr: None,
                cluster_cidr: None,
                cluster_domain: None,
                cluster_dns: None,
                node_name: None,
                cni: Some("cilium".to_string()),
                cni_conf_dir: Some(conf_dir.clone()),
                cni_bin_dir: Some(bin_dir.clone()),
                flannel_backend: None,
                datastore: Some(K3sDatastore::Kine),
            }),
        };
        let backups = snapshot_k3s_cni_paths(exports.path(), &installation).unwrap();

        fs::remove_dir_all(&data_dir).unwrap();
        restore_cni_path_backups(&backups).unwrap();

        assert_eq!(
            fs::read_to_string(conf_dir.join("05-cilium.conflist")).unwrap(),
            "cilium-config"
        );
        let binary = fs::read(bin_dir.join("cilium-cni")).unwrap();
        assert_eq!(binary.as_slice(), b"cni-binary");
        assert!(conf_dir.is_dir());
        assert!(bin_dir.is_dir());
    }

    #[test]
    fn migration_export_keeps_reserved_user_annotation_unchanged() {
        let sanitized = sanitize(serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "settings",
                "namespace": "apps",
                "uid": "source-uid",
                "resourceVersion": "17",
                "annotations": {
                    "nodemigrate.io/source-uid": "user-value"
                }
            },
            "data": {"setting": "preserved"},
            "status": {"ignored": true}
        }))
        .expect("ConfigMap should be exported");

        assert_eq!(sanitized.source_uid.as_deref(), Some("source-uid"));
        assert_eq!(
            sanitized.value["metadata"]["annotations"]["nodemigrate.io/source-uid"],
            "user-value"
        );
        assert!(sanitized.value["metadata"].get("uid").is_none());
        assert!(sanitized.value.get("status").is_none());
    }

    #[test]
    fn migration_export_marks_running_standalone_pods_for_terminal_restart() {
        let sanitized = sanitize(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "standalone", "namespace": "apps"},
            "spec": {"restartPolicy": "Never", "containers": [{"name": "app", "image": "busybox"}]},
            "status": {"phase": "Running"}
        }))
        .expect("running standalone Pod should be exported");
        assert_eq!(
            sanitized.value["_nodemigrateSourceWasRunning"],
            serde_json::Value::Bool(true)
        );
        assert!(sanitized.value.get("status").is_none());

        let failed: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "standalone", "namespace": "apps"},
            "status": {"phase": "Failed"}
        }))
        .unwrap();
        let exited: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "standalone", "namespace": "apps"},
            "status": {
                "phase": "Running",
                "containerStatuses": [{"name": "app", "state": {"terminated": {"exitCode": 137}}}]
            }
        }))
        .unwrap();
        let running: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "standalone", "namespace": "apps"},
            "status": {
                "phase": "Running",
                "containerStatuses": [{"name": "app", "ready": true, "state": {"running": {}}}]
            }
        }))
        .unwrap();
        assert!(pod_status_is_terminal_or_exited(&failed));
        assert!(pod_status_is_terminal_or_exited(&exited));
        assert!(!pod_status_is_terminal_or_exited(&running));
    }

    #[test]
    fn migration_export_preserves_pv_reclaim_policy_and_pvc_binding() {
        let pv = sanitize(serde_json::json!({
            "apiVersion": "v1",
            "kind": "PersistentVolume",
            "metadata": {"name": "migration-data", "uid": "source-pv-uid"},
            "spec": {
                "persistentVolumeReclaimPolicy": "Retain",
                "claimRef": {
                    "namespace": "migration-apps",
                    "name": "migration-data",
                    "uid": "source-pvc-uid"
                },
                "hostPath": {"path": "/var/lib/migration-data", "type": "DirectoryOrCreate"}
            },
            "status": {"phase": "Bound"}
        }))
        .expect("PersistentVolume should be exported");
        assert_eq!(pv.source_uid.as_deref(), Some("source-pv-uid"));
        assert_eq!(pv.value["spec"]["persistentVolumeReclaimPolicy"], "Retain");
        assert_eq!(pv.value["spec"]["claimRef"]["name"], "migration-data");
        assert_eq!(pv.value["spec"]["claimRef"]["uid"], "source-pvc-uid");
        assert_eq!(
            pv.value["spec"]["hostPath"]["path"],
            "/var/lib/migration-data"
        );
        assert!(pv.value.get("status").is_none());

        let pvc = sanitize(serde_json::json!({
            "apiVersion": "v1",
            "kind": "PersistentVolumeClaim",
            "metadata": {"name": "migration-data", "namespace": "migration-apps"},
            "spec": {
                "volumeName": "migration-data",
                "storageClassName": "",
                "accessModes": ["ReadWriteOnce"],
                "resources": {"requests": {"storage": "1Gi"}}
            },
            "status": {"phase": "Bound"}
        }))
        .expect("PersistentVolumeClaim should be exported");
        assert_eq!(pvc.value["spec"]["volumeName"], "migration-data");
        assert_eq!(pvc.value["spec"]["storageClassName"], "");
        assert!(pvc.value.get("status").is_none());
    }

    #[test]
    fn migration_export_restores_type_metadata_from_discovery() {
        let resource = ApiResource::from_gvk(&kube::core::GroupVersionKind::gvk(
            "apps",
            "v1",
            "Deployment",
        ));
        let mut object = serde_json::json!({"metadata": {"name": "web"}});

        preserve_discovered_type_meta(&mut object, &resource).unwrap();

        assert_eq!(object["apiVersion"], "apps/v1");
        assert_eq!(object["kind"], "Deployment");
    }

    #[test]
    fn migration_export_skips_ephemeral_metrics_api_objects() {
        assert_eq!(
            object_skip_reason(&serde_json::json!({
                "apiVersion": "metrics.k8s.io/v1beta1",
                "kind": "NodeMetrics",
                "metadata": {"name": "node-a"}
            })),
            Some(SkipReason::LiveMetrics)
        );
        assert_eq!(
            object_skip_reason(&serde_json::json!({
                "apiVersion": "metrics.k8s.io/v1beta1",
                "kind": "PodMetrics",
                "metadata": {"name": "pod-a", "namespace": "apps"}
            })),
            Some(SkipReason::LiveMetrics)
        );
    }

    #[test]
    fn migration_export_preserves_service_account_token_secret() {
        let token = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "type": "kubernetes.io/service-account-token",
            "metadata": {
                "name": "builder-token",
                "namespace": "apps",
                "annotations": {"kubernetes.io/service-account.name": "builder"}
            },
            "data": {"token": "c291cmNlLXRva2Vu", "custom": "cHJlc2VydmVk"}
        });
        let sanitized = sanitize(token).expect("ServiceAccount token Secret should be exported");
        assert_eq!(sanitized.value["data"]["token"], "c291cmNlLXRva2Vu");
        assert_eq!(sanitized.value["data"]["custom"], "cHJlc2VydmVk");
        assert_eq!(
            sanitized.value["metadata"]["annotations"]["kubernetes.io/service-account.name"],
            "builder"
        );
        assert!(
            object_rank(&serde_json::json!({"kind": "ServiceAccount"}))
                < object_rank(&serde_json::json!({"kind": "Secret"}))
        );
    }

    #[test]
    fn migrated_service_account_token_secret_uses_destination_identity_and_trust() {
        use base64::Engine;

        let secret: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {"name": "builder-token", "resourceVersion": "17"}
        }))
        .unwrap();
        let destination_ca = "-----BEGIN CERTIFICATE-----\ntarget-ca\n-----END CERTIFICATE-----\n";

        let patch = service_account_token_secret_patch(
            &secret,
            "apps",
            "destination-account-uid",
            "target-signed-jwt",
            destination_ca,
        )
        .unwrap();

        let decode = |key: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(patch["data"][key].as_str().unwrap())
                .unwrap()
        };
        assert_eq!(patch["metadata"]["resourceVersion"], "17");
        assert_eq!(
            patch["metadata"]["annotations"]["kubernetes.io/service-account.uid"],
            "destination-account-uid"
        );
        assert_eq!(decode("token"), b"target-signed-jwt");
        assert_eq!(decode("namespace"), b"apps");
        assert_eq!(decode("ca.crt"), destination_ca.as_bytes());
    }

    #[test]
    fn missing_migrated_service_account_token_secret_is_recreated_for_destination() {
        use base64::Engine;

        let source = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "type": "kubernetes.io/service-account-token",
            "metadata": {
                "name": "migration-legacy-token",
                "namespace": "migration-apps",
                "uid": "source-secret-uid",
                "resourceVersion": "41",
                "annotations": {
                    "kubernetes.io/service-account.name": "migration-token-user",
                    "kubernetes.io/service-account.uid": "source-account-uid"
                }
            },
            "data": {
                "token": "c291cmNlLXRva2Vu",
                "namespace": "b2xkLW5hbWVzcGFjZQ==",
                "ca.crt": "c291cmNlLWNh",
                "fixture": "bGVnYWN5LXNlY3JldC1kYXRhLXByZXNlcnZlZA=="
            }
        });
        let destination_ca = "-----BEGIN CERTIFICATE-----\ntarget-ca\n-----END CERTIFICATE-----\n";
        let secret = service_account_token_secret_value(
            &source,
            "migration-apps",
            "destination-account-uid",
            "destination-signed-jwt",
            destination_ca,
        )
        .unwrap();
        let decode = |key: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(secret["data"][key].as_str().unwrap())
                .unwrap()
        };

        assert_eq!(secret["metadata"]["name"], "migration-legacy-token");
        assert_eq!(secret["metadata"]["namespace"], "migration-apps");
        assert!(secret["metadata"].get("uid").is_none());
        assert!(secret["metadata"].get("resourceVersion").is_none());
        assert_eq!(
            secret["metadata"]["annotations"]["kubernetes.io/service-account.name"],
            "migration-token-user"
        );
        assert_eq!(
            secret["metadata"]["annotations"]["kubernetes.io/service-account.uid"],
            "destination-account-uid"
        );
        assert_eq!(decode("token"), b"destination-signed-jwt");
        assert_eq!(decode("namespace"), b"migration-apps");
        assert_eq!(decode("ca.crt"), destination_ca.as_bytes());
        assert_eq!(decode("fixture"), b"legacy-secret-data-preserved");
    }

    #[test]
    fn migration_export_regenerates_destination_control_plane_trust_configmaps() {
        let object = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "kube-root-ca.crt", "namespace": "kube-system"},
            "data": {"ca.crt": "source-cluster-ca"}
        });
        assert_eq!(
            object_skip_reason(&object),
            Some(SkipReason::DestinationTrustBundle)
        );
        assert!(sanitize(object).is_none());

        let extension_auth = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "extension-apiserver-authentication",
                "namespace": "kube-system"
            },
            "data": {"requestheader-client-ca-file": "source-front-proxy-ca"}
        });
        assert_eq!(
            object_skip_reason(&extension_auth),
            Some(SkipReason::DestinationTrustBundle)
        );
        assert!(sanitize(extension_auth).is_none());

        assert!(!skip_object(&serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "application-settings", "namespace": "apps"},
            "data": {"setting": "preserved"}
        })));
    }

    #[test]
    fn migration_export_regenerates_only_nodebootstrap_owned_rbac() {
        for kind in ["Role", "RoleBinding", "ClusterRole", "ClusterRoleBinding"] {
            let object = serde_json::json!({
                "apiVersion": "rbac.authorization.k8s.io/v1",
                "kind": kind,
                "metadata": {
                    "name": "nodebootstrap:controller-sa-node-controller",
                    "namespace": "kube-system"
                },
                "rules": [{"apiGroups": [""], "resources": ["pods"], "verbs": ["get"]}]
            });
            assert_eq!(
                object_skip_reason(&object),
                Some(SkipReason::NodebootstrapRuntimeRbac),
                "nodebootstrap-owned {kind} must be regenerated by the destination"
            );
            assert!(sanitize(object).is_none());
        }

        for object in [
            serde_json::json!({
                "apiVersion": "rbac.authorization.k8s.io/v1",
                "kind": "ClusterRole",
                "metadata": {"name": "migration-observer"},
                "rules": [{"apiGroups": [""], "resources": ["pods"], "verbs": ["get"]}]
            }),
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": "nodebootstrap:application-data", "namespace": "apps"},
                "data": {"preserve": "this"}
            }),
        ] {
            assert!(
                sanitize(object).is_some(),
                "user RBAC and non-RBAC objects with the reserved-looking name remain migratable"
            );
        }
    }

    #[test]
    fn migration_export_preserves_user_endpoints_and_leases() {
        for object in [
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "Endpoints",
                "metadata": {"name": "external-db", "namespace": "migration-apps"},
                "subsets": [{"addresses": [{"ip": "192.0.2.20"}], "ports": [{"port": 5432}]}]
            }),
            serde_json::json!({
                "apiVersion": "discovery.k8s.io/v1",
                "kind": "EndpointSlice",
                "metadata": {
                    "name": "external-db-v4",
                    "namespace": "migration-apps",
                    "labels": {
                        "kubernetes.io/service-name": "external-db",
                        "endpointslice.kubernetes.io/managed-by": "migration-operator"
                    }
                },
                "addressType": "IPv4",
                "endpoints": [{"addresses": ["192.0.2.20"]}],
                "ports": [{"port": 5432}]
            }),
            serde_json::json!({
                "apiVersion": "coordination.k8s.io/v1",
                "kind": "Lease",
                "metadata": {"name": "migration-lock", "namespace": "migration-apps"},
                "spec": {"holderIdentity": "migration-controller"}
            }),
        ] {
            assert!(
                sanitize(object).is_some(),
                "user-managed endpoint or application Lease was omitted"
            );
        }
    }

    #[test]
    fn migration_export_rebuilds_cilium_runtime_state_but_preserves_policies() {
        for (object, expected_reason) in [
            (
                serde_json::json!({
                    "apiVersion": "cilium.io/v2",
                    "kind": "CiliumEndpoint",
                    "metadata": {"name": "web-abc", "namespace": "apps"},
                    "status": {"id": 1234, "identity": {"id": 1234}}
                }),
                SkipReason::CiliumEndpointReconciliation,
            ),
            (
                serde_json::json!({
                    "apiVersion": "cilium.io/v2",
                    "kind": "CiliumIdentity",
                    "metadata": {"name": "12345"}
                }),
                SkipReason::CiliumIdentityReconciliation,
            ),
            (
                serde_json::json!({
                    "apiVersion": "cilium.io/v2",
                    "kind": "CiliumNode",
                    "metadata": {"name": "worker-1"},
                    "spec": {"ipam": {"podCIDRs": ["10.42.0.0/24"]}}
                }),
                SkipReason::CiliumNodeReconciliation,
            ),
        ] {
            assert_eq!(object_skip_reason(&object), Some(expected_reason));
            assert!(sanitize(object).is_none());
        }

        let policy = serde_json::json!({
            "apiVersion": "cilium.io/v2",
            "kind": "CiliumNetworkPolicy",
            "metadata": {"name": "allow-web", "namespace": "apps"},
            "spec": {"endpointSelector": {"matchLabels": {"app": "web"}}}
        });
        assert_eq!(object_skip_reason(&policy), None);
        assert!(sanitize(policy).is_some());
    }

    #[test]
    fn migration_export_skips_only_kubernetes_regenerated_endpoints_and_node_leases() {
        let cases = [
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "Endpoints",
                "metadata": {
                    "name": "web",
                    "namespace": "apps",
                    "labels": {"endpoints.kubernetes.io/managed-by": "endpoint-controller"}
                }
            }),
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "Endpoints",
                "metadata": {"name": "kubernetes", "namespace": "default"}
            }),
            serde_json::json!({
                "apiVersion": "discovery.k8s.io/v1",
                "kind": "EndpointSlice",
                "metadata": {
                    "name": "web-abc",
                    "namespace": "apps",
                    "labels": {"endpointslice.kubernetes.io/managed-by": "endpointslice-controller.k8s.io"}
                }
            }),
            serde_json::json!({
                "apiVersion": "discovery.k8s.io/v1",
                "kind": "EndpointSlice",
                "metadata": {
                    "name": "kubernetes",
                    "namespace": "default",
                    "labels": {"kubernetes.io/service-name": "kubernetes"}
                }
            }),
            serde_json::json!({
                "apiVersion": "discovery.k8s.io/v1",
                "kind": "EndpointSlice",
                "metadata": {
                    "name": "web-mirror-abc",
                    "namespace": "apps",
                    "labels": {"endpointslice.kubernetes.io/managed-by": "endpointslicemirroring-controller.k8s.io"}
                }
            }),
            serde_json::json!({
                "apiVersion": "discovery.k8s.io/v1",
                "kind": "EndpointSlice",
                "metadata": {
                    "name": "web-nodecontroller",
                    "namespace": "apps",
                    "labels": {
                        "kubernetes.io/service-name": "web",
                        "endpointslice.kubernetes.io/managed-by": "nodecontroller"
                    }
                }
            }),
            serde_json::json!({
                "apiVersion": "coordination.k8s.io/v1",
                "kind": "Lease",
                "metadata": {"name": "node-a", "namespace": "kube-node-lease"}
            }),
        ];
        for (index, object) in cases.into_iter().enumerate() {
            let expected_reason = if index == 6 {
                SkipReason::NodeHeartbeatLease
            } else if index == 1 || index == 3 {
                SkipReason::ApiServiceRouting
            } else {
                SkipReason::ControllerManagedEndpoints
            };
            assert_eq!(object_skip_reason(&object), Some(expected_reason));
            assert!(
                sanitize(object).is_none(),
                "Kubernetes-regenerated endpoint or node Lease was exported"
            );
        }
    }

    #[test]
    fn migration_export_skip_kinds_have_explicit_lifecycle_reasons() {
        for (kind, expected) in [
            ("ComponentStatus", SkipReason::ReadOnlyComponentStatus),
            ("Event", SkipReason::EphemeralEvents),
            ("Node", SkipReason::NodeReregistration),
            ("NodeMetrics", SkipReason::LiveMetrics),
            ("PodMetrics", SkipReason::LiveMetrics),
            ("VolumeAttachment", SkipReason::CsiReattachment),
        ] {
            assert_eq!(skip_kind_reason(kind), Some(expected));
            assert!(!expected.description().is_empty());
        }
        assert_eq!(skip_kind_reason("Deployment"), None);
    }

    #[test]
    fn migration_export_preserves_standalone_pods_and_rollout_history() {
        for object in [
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {"name": "standalone", "namespace": "apps"},
                "spec": {"containers": [{"name": "app", "image": "busybox"}]}
            }),
            serde_json::json!({
                "apiVersion": "apps/v1",
                "kind": "ReplicaSet",
                "metadata": {"name": "web-old", "namespace": "apps"},
                "spec": {"replicas": 0}
            }),
            serde_json::json!({
                "apiVersion": "apps/v1",
                "kind": "ControllerRevision",
                "metadata": {"name": "db-old", "namespace": "apps"},
                "revision": 1,
                "data": {"spec": {"replicas": 1}}
            }),
        ] {
            assert!(
                sanitize(object).is_some(),
                "durable workload object was omitted from migration export"
            );
        }
    }

    #[test]
    fn migration_export_skips_controller_regenerated_and_static_mirror_pods() {
        let controller_owned = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "deployment-pod",
                "namespace": "apps",
                "ownerReferences": [{
                    "apiVersion": "apps/v1",
                    "kind": "ReplicaSet",
                    "name": "web",
                    "uid": "source-uid",
                    "controller": true
                }]
            }
        });
        assert_eq!(
            object_skip_reason(&controller_owned),
            Some(SkipReason::ControllerOwnedPod)
        );
        assert!(sanitize(controller_owned).is_none());

        let mirror = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "kube-apiserver-node-a",
                "namespace": "kube-system",
                "annotations": {"kubernetes.io/config.mirror": "mirror-uid"}
            }
        });
        assert_eq!(
            object_skip_reason(&mirror),
            Some(SkipReason::StaticPodMirror)
        );
        assert!(sanitize(mirror).is_none());
    }

    #[test]
    fn recovery_manifest_keeps_uid_mapping_outside_api_objects() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let object_path = directory.path().join("00000000.json");
        std::fs::write(
            &object_path,
            serde_json::to_vec(&serde_json::json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": "settings", "namespace": "apps"},
                "data": {"marker": "preserved"}
            }))
            .unwrap(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&object_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let objects = [ExportedObject {
            path: object_path,
            source_uid: Some("source-uid".to_string()),
        }];

        let node_states = BTreeMap::from([
            (
                "node-a".to_string(),
                NodeSchedulingState {
                    uid: Some("node-uid".to_string()),
                    labels: HashMap::from([
                        ("zone".to_string(), "west".to_string()),
                        (
                            "node-role.kubernetes.io/control-plane".to_string(),
                            String::new(),
                        ),
                    ]),
                    annotations: HashMap::new(),
                    taints: Vec::new(),
                    unschedulable: Some(true),
                },
            ),
            (
                "node-b".to_string(),
                NodeSchedulingState {
                    uid: Some("worker-uid".to_string()),
                    labels: HashMap::from([(
                        "kubernetes.io/hostname".to_string(),
                        "node-b".to_string(),
                    )]),
                    annotations: HashMap::new(),
                    taints: Vec::new(),
                    unschedulable: Some(false),
                },
            ),
        ]);
        write_export_manifest(directory.path(), &objects, &node_states).unwrap();

        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.path().join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["formatVersion"], 2);
        assert_eq!(manifest["nodeStates"]["node-a"]["labels"]["zone"], "west");
        assert_eq!(manifest["objects"][0]["file"], "00000000.json");
        assert_eq!(manifest["objects"][0]["sourceUid"], "source-uid");
        let export = Export::load(directory.path()).unwrap();
        assert_eq!(export.objects.len(), 1);
        assert_eq!(export.objects[0].source_uid.as_deref(), Some("source-uid"));
        assert_eq!(export.node_state_count(), 2);
        assert_eq!(
            export.node_state("node-a").unwrap().uid.as_deref(),
            Some("node-uid")
        );
        assert_eq!(
            export.node_state("node-a").unwrap().unschedulable,
            Some(true)
        );
        assert_eq!(
            export.node_state("node-b").unwrap().uid.as_deref(),
            Some("worker-uid")
        );
        assert_eq!(export.control_plane_node_names(), ["node-a"]);
    }

    #[test]
    fn node_private_export_copy_does_not_mutate_the_shared_source_export() {
        let root = tempfile::tempdir().unwrap();
        let source_dir = root.path().join("source");
        let first_node_dir = root.path().join("first-node");
        let second_node_dir = root.path().join("second-node");
        fs::create_dir(&source_dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let source_object = source_dir.join("00000000.json");
        let source_data = br#"{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"migration-state","namespace":"default"},"data":{"marker":"original"}}"#;
        fs::write(&source_object, source_data).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&source_object, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let source_objects = [ExportedObject {
            path: source_object.clone(),
            source_uid: Some("source-object-uid".to_string()),
        }];
        let source_nodes = BTreeMap::from([(
            "node-a".to_string(),
            NodeSchedulingState {
                uid: Some("source-node-uid".to_string()),
                labels: HashMap::from([(
                    "node-role.kubernetes.io/control-plane".to_string(),
                    String::new(),
                )]),
                annotations: HashMap::from([(
                    "operator.example/zone".to_string(),
                    "west".to_string(),
                )]),
                taints: Vec::new(),
                unschedulable: Some(false),
            },
        )]);
        write_export_manifest(&source_dir, &source_objects, &source_nodes).unwrap();
        let source = Export::load(&source_dir).unwrap();

        let first = source.copy_to_directory(first_node_dir).unwrap();
        let second = source.copy_to_directory(second_node_dir).unwrap();
        assert_ne!(first.dir, second.dir);
        assert_ne!(first.objects[0].path, second.objects[0].path);
        assert_eq!(
            first.objects[0].source_uid.as_deref(),
            Some("source-object-uid")
        );
        assert_eq!(
            second.objects[0].source_uid.as_deref(),
            Some("source-object-uid")
        );
        for export in [&first, &second] {
            assert_eq!(export.node_state_count(), 1);
            assert_eq!(
                export.node_state("node-a").unwrap().uid.as_deref(),
                Some("source-node-uid")
            );
            assert_eq!(
                export.node_state("node-a").unwrap().annotations["operator.example/zone"],
                "west"
            );
            assert_eq!(export.control_plane_node_names(), ["node-a"]);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(first.dir.metadata().unwrap().permissions().mode() & 0o777, 0o700);
            assert_eq!(
                first.objects[0].path.metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        fs::write(&first.objects[0].path, b"node-specific change").unwrap();
        assert_eq!(fs::read(&source_object).unwrap(), source_data);
        assert_eq!(fs::read(&second.objects[0].path).unwrap(), source_data);
        assert_ne!(fs::read(&first.objects[0].path).unwrap(), source_data);
    }

    #[test]
    fn offline_node_exports_keep_host_path_backups_separate() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let east_path = directory.path().join("east-volume");
        let west_path = directory.path().join("west-volume");
        fs::write(&east_path, "east-before").unwrap();
        fs::write(&west_path, "west-before").unwrap();
        let east_object = directory.path().join("east-pv.json");
        let west_object = directory.path().join("west-pv.json");
        for (object_path, volume_path, zone) in [
            (&east_object, &east_path, "east"),
            (&west_object, &west_path, "west"),
        ] {
            fs::write(
                object_path,
                serde_json::to_vec(&serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "PersistentVolume",
                    "metadata": {"name": format!("{zone}-pv")},
                    "spec": {
                        "hostPath": {"path": volume_path},
                        "nodeAffinity": {"required": {"nodeSelectorTerms": [{
                            "matchExpressions": [{"key": "topology.kubernetes.io/zone", "operator": "In", "values": [zone]}]
                        }]}}
                    }
                }))
                .unwrap(),
            )
            .unwrap();
        }
        let objects = vec![
            ExportedObject {
                path: east_object,
                source_uid: None,
            },
            ExportedObject {
                path: west_object,
                source_uid: None,
            },
        ];
        let node_states = BTreeMap::from([
            (
                "node-east".to_string(),
                NodeSchedulingState {
                    labels: HashMap::from([(
                        "topology.kubernetes.io/zone".to_string(),
                        "east".to_string(),
                    )]),
                    ..Default::default()
                },
            ),
            (
                "node-west".to_string(),
                NodeSchedulingState {
                    labels: HashMap::from([(
                        "topology.kubernetes.io/zone".to_string(),
                        "west".to_string(),
                    )]),
                    ..Default::default()
                },
            ),
        ]);
        let mut east_export = Export {
            dir: directory.path().to_path_buf(),
            objects: objects.clone(),
            node_states: node_states.clone(),
            host_paths: Vec::new(),
            host_path_backups: Vec::new(),
            cni_path_backups: Vec::new(),
        };
        let mut west_export = Export {
            dir: directory.path().to_path_buf(),
            objects,
            node_states,
            host_paths: Vec::new(),
            host_path_backups: Vec::new(),
            cni_path_backups: Vec::new(),
        };

        east_export
            .snapshot_host_paths_for_node("node-east")
            .unwrap();
        west_export
            .snapshot_host_paths_for_node("node-west")
            .unwrap();

        assert_eq!(east_export.host_paths, vec![east_path.clone()]);
        assert_eq!(west_export.host_paths, vec![west_path.clone()]);
        assert!(directory.path().join("host-paths-node-east").is_dir());
        assert!(directory.path().join("host-paths-node-west").is_dir());
        fs::write(&east_path, "east-after").unwrap();
        fs::write(&west_path, "west-after").unwrap();
        east_export.restore_host_paths().unwrap();
        west_export.restore_host_paths().unwrap();
        assert_eq!(fs::read_to_string(east_path).unwrap(), "east-before");
        assert_eq!(fs::read_to_string(west_path).unwrap(), "west-before");
    }

    #[test]
    fn recovery_manifest_rejects_paths_outside_export_directory() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let manifest_path = directory.path().join("manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_vec(&serde_json::json!({
                "formatVersion": 1,
                "objects": [{"file": "../outside.json", "sourceUid": "source-uid"}]
            }))
            .unwrap(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&manifest_path, std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }

        assert!(Export::load(directory.path()).is_err());
    }

    #[test]
    fn replacement_node_state_keeps_labels_annotations_taints_and_unschedulable() {
        let node: kube::api::DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": {
                "name": "worker-a",
                "uid": "source-node-uid",
                "labels": {
                    "kubernetes.io/hostname": "worker-a",
                    "storage.example/node": "local"
                },
                "annotations": {
                    "storage.example/volume-group": "fast",
                    "nodemigrate.io/source-uid": "operator-value"
                }
            },
            "spec": {
                "taints": [{"key": "dedicated", "value": "gpu", "effect": "NoSchedule"}],
                "unschedulable": true
            }
        }))
        .unwrap();

        let state = NodeSchedulingState::from_node(&node);
        assert_eq!(state.uid.as_deref(), Some("source-node-uid"));
        assert_eq!(state.labels["storage.example/node"], "local");
        assert_eq!(state.annotations["storage.example/volume-group"], "fast");
        assert_eq!(
            state.annotations["nodemigrate.io/source-uid"],
            "operator-value"
        );
        assert_eq!(state.taints.len(), 1);
        assert_eq!(state.unschedulable, Some(true));
        let patch = node_scheduling_patch(&state);
        assert_eq!(
            patch["metadata"]["annotations"]["nodemigrate.io/source-uid"],
            "operator-value"
        );
        assert_eq!(patch["spec"]["unschedulable"], true);
    }

    #[test]
    fn replacement_node_must_have_a_new_uid_and_be_ready() {
        let old_ready: DynamicObject = serde_json::from_value(serde_json::json!({
            "metadata": {"uid": "old-node-uid"},
            "status": {"conditions": [{"type": "Ready", "status": "True"}]}
        }))
        .unwrap();
        let new_not_ready: DynamicObject = serde_json::from_value(serde_json::json!({
            "metadata": {"uid": "new-node-uid"},
            "status": {"conditions": [{"type": "Ready", "status": "False"}]}
        }))
        .unwrap();
        let new_ready: DynamicObject = serde_json::from_value(serde_json::json!({
            "metadata": {"uid": "new-node-uid"},
            "status": {"conditions": [{"type": "Ready", "status": "True"}]}
        }))
        .unwrap();

        assert!(!node_is_ready_replacement(&old_ready, "old-node-uid"));
        assert!(!node_uid_has_been_replaced(Some(&old_ready), "old-node-uid"));
        assert!(node_uid_has_been_replaced(None, "old-node-uid"));
        assert!(node_uid_has_been_replaced(Some(&new_not_ready), "old-node-uid"));
        assert!(!node_is_ready_replacement(&new_not_ready, "old-node-uid"));
        assert!(node_is_ready_replacement(&new_ready, "old-node-uid"));
    }

    #[test]
    fn host_path_backup_includes_safe_hostpath_and_local_volumes_once() {
        let objects = vec![
            serde_json::json!({
                "kind": "PersistentVolume",
                "spec": {"hostPath": {"path": "/srv/data"}}
            }),
            serde_json::json!({
                "kind": "PersistentVolume",
                "spec": {"local": {"path": "/srv/data/nested"}}
            }),
            serde_json::json!({
                "kind": "PersistentVolume",
                "spec": {"hostPath": {"path": "/"}}
            }),
            serde_json::json!({
                "kind": "PersistentVolume",
                "spec": {"hostPath": {"path": "/srv/../etc"}}
            }),
            serde_json::json!({"kind": "Pod", "spec": {"hostPath": {"path": "/tmp"}}}),
        ];

        assert_eq!(
            persistent_host_paths(&objects, Some(&HashMap::new())),
            [std::path::PathBuf::from("/srv/data")]
        );
    }

    #[test]
    fn host_path_backup_respects_required_pv_node_affinity() {
        let objects = vec![
            serde_json::json!({
                "kind": "PersistentVolume",
                "spec": {
                    "local": {"path": "/srv/control-plane-data"},
                    "nodeAffinity": {"required": {"nodeSelectorTerms": [{
                        "matchExpressions": [{
                            "key": "kubernetes.io/hostname",
                            "operator": "In",
                            "values": ["cp-1"]
                        }]
                    }]}}
                }
            }),
            serde_json::json!({
                "kind": "PersistentVolume",
                "spec": {
                    "local": {"path": "/srv/worker-data"},
                    "nodeAffinity": {"required": {"nodeSelectorTerms": [{
                        "matchFields": [{
                            "key": "metadata.name",
                            "operator": "In",
                            "values": ["worker-1"]
                        }]
                    }]}}
                }
            }),
        ];
        let labels = HashMap::from([("kubernetes.io/hostname".to_string(), "cp-1".to_string())]);

        assert_eq!(
            persistent_host_paths(&objects, Some(&labels)),
            [std::path::PathBuf::from("/srv/control-plane-data")]
        );
    }

    #[test]
    fn host_path_backup_keeps_unknown_affinity_candidates() {
        let objects = vec![serde_json::json!({
            "kind": "PersistentVolume",
            "spec": {
                "local": {"path": "/srv/node-data"},
                "nodeAffinity": {"required": {"nodeSelectorTerms": [{
                    "matchExpressions": [{
                        "key": "storage.example/zone",
                        "operator": "In",
                        "values": ["zone-a"]
                    }]
                }]}}
            }
        })];

        assert_eq!(
            persistent_host_paths(&objects, None),
            [std::path::PathBuf::from("/srv/node-data")]
        );
    }

    #[test]
    fn node_affinity_ors_terms_and_ands_requirements() {
        let objects = vec![
            serde_json::json!({
                "kind": "PersistentVolume",
                "spec": {
                    "local": {"path": "/srv/matching-term"},
                    "nodeAffinity": {"required": {"nodeSelectorTerms": [
                        {"matchExpressions": [
                            {"key": "topology.kubernetes.io/zone", "operator": "In", "values": ["zone-a"]},
                            {"key": "disk", "operator": "In", "values": ["ssd"]}
                        ]},
                        {"matchFields": [
                            {"key": "metadata.name", "operator": "In", "values": ["cp-1"]}
                        ]}
                    ]}}
                }
            }),
            serde_json::json!({
                "kind": "PersistentVolume",
                "spec": {
                    "local": {"path": "/srv/non-matching-term"},
                    "nodeAffinity": {"required": {"nodeSelectorTerms": [{
                        "matchExpressions": [
                            {"key": "topology.kubernetes.io/zone", "operator": "In", "values": ["zone-a"]},
                            {"key": "disk", "operator": "In", "values": ["ssd"]}
                        ]
                    }]}}
                }
            }),
        ];
        let labels = HashMap::from([
            (
                "topology.kubernetes.io/zone".to_string(),
                "zone-a".to_string(),
            ),
            ("disk".to_string(), "hdd".to_string()),
            ("metadata.name".to_string(), "cp-1".to_string()),
        ]);

        assert_eq!(
            persistent_host_paths(&objects, Some(&labels)),
            [std::path::PathBuf::from("/srv/matching-term")]
        );
    }
}
