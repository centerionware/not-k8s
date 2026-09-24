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
use kube::{
    api::{Api, DeleteParams, DynamicObject, ListParams, Patch, PatchParams, Preconditions},
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

const SKIP_KINDS: &[&str] = &[
    "ComponentStatus",
    "ControllerRevision",
    "Endpoints",
    "EndpointSlice",
    "Event",
    "Lease",
    "Node",
    "NodeMetrics",
    "Pod",
    "PodMetrics",
    "ReplicaSet",
    "VolumeAttachment",
];
#[derive(Debug, Clone)]
pub struct KubeApi {
    kubeconfig: PathBuf,
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

impl KubeApi {
    pub fn source(installation: &Installation) -> Result<Self> {
        let kubeconfig = std::env::var_os("NODEMIGRATE_SOURCE_KUBECONFIG")
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
            });
        ensure!(
            kubeconfig.is_file(),
            "source kubeconfig {} does not exist",
            kubeconfig.display()
        );
        Ok(Self { kubeconfig })
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
            let discovery = Discovery::new(client.clone())
                .run()
                .await
                .context("discovering Kubernetes APIs")?;
            let (resource, capabilities) = find_resource(&discovery, "Namespace", "v1")
                .context("Kubernetes API does not expose Namespace")?;
            ensure!(
                capabilities.supports_operation(verbs::LIST),
                "Kubernetes API cannot list namespaces"
            );
            let api: Api<DynamicObject> = Api::all_with(client, &resource);
            api.list(&ListParams::default())
                .await
                .context("checking Kubernetes API readiness")?;
            Ok(())
        })
    }

    pub fn node_count(&self) -> Result<usize> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let discovery = Discovery::new(client.clone())
                .run()
                .await
                .context("discovering Kubernetes APIs")?;
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
            let discovery = Discovery::new(client.clone())
                .run()
                .await
                .context("discovering Kubernetes APIs")?;
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
            Ok(node
                .data
                .pointer("/status/conditions")
                .and_then(Value::as_array)
                .is_some_and(|conditions| {
                    conditions.iter().any(|condition| {
                        condition.get("type").and_then(Value::as_str) == Some("Ready")
                            && condition.get("status").and_then(Value::as_str) == Some("True")
                    })
                }))
        })
    }

    pub fn node_exists(&self, name: &str) -> Result<bool> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let discovery = Discovery::new(client.clone())
                .run()
                .await
                .context("discovering Kubernetes APIs")?;
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
            let discovery = Discovery::new(client.clone())
                .run()
                .await
                .context("discovering Kubernetes APIs")?;
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
            let discovery = Discovery::new(client.clone())
                .run()
                .await
                .context("discovering Kubernetes APIs")?;
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

    pub fn delete_node(&self, name: &str, expected_uid: &str) -> Result<()> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let discovery = Discovery::new(client.clone())
                .run()
                .await
                .context("discovering Kubernetes APIs")?;
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
            Ok(())
        })
    }

    /// Back up hostPath/local PV payloads present on this node without
    /// exporting or re-applying cluster-wide API objects from a worker.
    pub fn node_labels(&self, node_name: &str) -> Result<HashMap<String, String>> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let discovery = Discovery::new(client.clone())
                .run()
                .await
                .context("discovering Kubernetes APIs")?;
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
            let discovery = Discovery::new(client.clone())
                .run()
                .await
                .context("discovering Kubernetes APIs")?;
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
            let discovery = Discovery::new(client.clone())
                .run()
                .await
                .context("discovering source Kubernetes APIs")?;
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
            for group in discovery.groups() {
                for (resource, capabilities) in group.recommended_resources() {
                    if !capabilities.supports_operation(verbs::LIST) || skip_kind(&resource.kind) {
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
                            if !skip_object(&value) {
                                objects.push(value);
                            }
                        }
                        continue_token = page.metadata.continue_.filter(|token| !token.is_empty());
                        if continue_token.is_none() {
                            break;
                        }
                    }
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
        runtime.block_on(async {
            let mut pending = export.objects.clone();
            let mut last_error = String::new();
            let mut uid_map = HashMap::new();
            let mut source_crd_apis = BTreeSet::new();
            for attempt in 0..5 {
                let discovery = Discovery::new(client.clone()).run().await.context("discovering destination Kubernetes APIs")?;
                let mut retry = Vec::new();
                let mut failures = Vec::new();
                for object in pending {
                    let value: Value = serde_json::from_slice(
                        &fs::read(&object.path)
                            .with_context(|| format!("reading {}", object.path.display()))?
                    ).context("decoding protected migration object")?;
                    let mut initial = value.clone();
                    if let Some(metadata) = initial.pointer_mut("/metadata").and_then(Value::as_object_mut) {
                        metadata.remove("ownerReferences");
                    }
                    if let Some(claim_ref) = initial.pointer_mut("/spec/claimRef").and_then(Value::as_object_mut) {
                        claim_ref.remove("uid");
                    }
                    if attempt == 0 {
                        source_crd_apis.extend(custom_resource_gvks(&initial));
                    }
                    match apply_object(&client, &discovery, &initial).await {
                        Ok(applied) => {
                            if let (Some(source_uid), Some(destination_uid)) = (
                                object.source_uid,
                                applied.metadata.uid,
                            ) {
                                uid_map.insert(source_uid, destination_uid);
                            }
                        }
                        Err(error) => {
                            failures.push((object_type_label(&initial), error.to_string()));
                            retry.push(object);
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
                if attempt == 0 && !source_crd_apis.is_empty() && !crd_apply_failed {
                    let missing = wait_for_custom_resource_apis(
                        &client,
                        &source_crd_apis,
                        std::time::Duration::from_secs(60),
                    )
                    .await?;
                    if !missing.is_empty() {
                        let missing = missing
                            .iter()
                            .map(|(group, version, kind)| format!("{group}/{version}/{kind}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        bail!(
                            "{} Kubernetes objects could not be restored; export retained at {}. Destination did not expose these source CRD APIs after 60 seconds: {missing}. Last-attempt failures: {}",
                            pending.len(), export.dir.display(), last_error
                        );
                    }
                }
                if pending.is_empty() { break; }
                if attempt < 4 { tokio::time::sleep(std::time::Duration::from_secs(3)).await; }
            }
            if !pending.is_empty() {
                bail!("{} Kubernetes objects could not be restored; export retained at {}. Last-attempt failures: {}", pending.len(), export.dir.display(), last_error)
            }
            let discovery = Discovery::new(client.clone()).run().await.context("discovering destination APIs for reference repair")?;
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
                    apply_object(&client, &discovery, &value).await
                        .with_context(|| format!("repairing references in {}", object.path.display()))?;
                }
            }
            Ok(())
        })
    }
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
    discovery
        .groups()
        .flat_map(|group| group.recommended_resources())
        .find(|(resource, _)| resource.kind == kind && resource.api_version == api_version)
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
        .with_context(|| format!("destination does not expose {type_meta}/{kind}"))?;
    ensure!(
        capabilities.supports_operation(verbs::PATCH),
        "destination does not allow applying {kind}"
    );
    let object: DynamicObject =
        serde_json::from_value(value.clone()).context("decoding migration object")?;
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
    let applied = api
        .patch(
            name,
            &PatchParams::apply("nodemigrate").force(),
            &Patch::Apply(&object),
        )
        .await
        .with_context(|| format!("applying {type_meta}/{kind} {name}"))?;
    Ok(applied)
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
    let mut discovery = Discovery::new(client.clone())
        .run()
        .await
        .context("discovering destination APIs after applying source CustomResourceDefinitions")?;
    let mut missing = missing_custom_resource_apis(&discovery, expected);
    if !missing.is_empty() {
        eprintln!(
            "nodemigrate: waiting for {} of {} source CustomResourceDefinition APIs to appear in destination discovery",
            missing.len(), expected.len()
        );
    }
    while !missing.is_empty() && started.elapsed() < timeout {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        discovery = Discovery::new(client.clone()).run().await.context(
            "refreshing destination API discovery for imported CustomResourceDefinitions",
        )?;
        missing = missing_custom_resource_apis(&discovery, expected);
    }
    Ok(missing)
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

fn skip_kind(kind: &str) -> bool {
    SKIP_KINDS.contains(&kind)
}

fn skip_object(object: &Value) -> bool {
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if skip_kind(kind) {
        return true;
    }
    object.get("kind").and_then(Value::as_str) == Some("Secret")
        && object.pointer("/type").and_then(Value::as_str)
            == Some("kubernetes.io/service-account-token")
}

fn sanitize(mut object: Value) -> Option<SanitizedObject> {
    if skip_object(&object) {
        return None;
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
        "StorageClass" => 2,
        _ => 3,
    }
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
        custom_resource_gvks, node_scheduling_patch, object_type_label, persistent_host_paths,
        preserve_discovered_type_meta, restore_cni_path_backups, sanitize, skip_object,
        snapshot_k3s_cni_paths, summarize_import_failures, write_export_manifest, ApiResource,
        Export, ExportedObject, KubeApi, NodeSchedulingState,
    };
    use crate::detect::{ClusterConfig, Installation, K3sDatastore, NodeRole, ServiceManager};
    use crate::request::Distribution;
    use std::collections::{BTreeMap, HashMap};
    use std::fs;

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
        assert!(skip_object(&serde_json::json!({
            "apiVersion": "metrics.k8s.io/v1beta1",
            "kind": "NodeMetrics",
            "metadata": {"name": "node-a"}
        })));
        assert!(skip_object(&serde_json::json!({
            "apiVersion": "metrics.k8s.io/v1beta1",
            "kind": "PodMetrics",
            "metadata": {"name": "pod-a", "namespace": "apps"}
        })));
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

        let node_states = BTreeMap::from([(
            "node-a".to_string(),
            NodeSchedulingState {
                uid: Some("node-uid".to_string()),
                labels: HashMap::from([("zone".to_string(), "west".to_string())]),
                annotations: HashMap::new(),
                taints: Vec::new(),
                unschedulable: Some(true),
            },
        )]);
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
        assert_eq!(export.node_state_count(), 1);
        assert_eq!(
            export.node_state("node-a").unwrap().uid.as_deref(),
            Some("node-uid")
        );
        assert_eq!(
            export.node_state("node-a").unwrap().unschedulable,
            Some(true)
        );
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
