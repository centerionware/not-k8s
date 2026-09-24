//! Kubernetes API object transfer. Datastore files are distribution-specific;
//! reading and applying resources through the API avoids copying Kine/etcd
//! storage into nodestore.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, ensure, Context, Result};
use kube::{
    api::{Api, DeleteParams, DynamicObject, ListParams, Patch, PatchParams},
    config::Kubeconfig,
    discovery::{verbs, ApiResource, Discovery},
    Client,
};
use serde_json::Value;

use crate::detect::{Installation, K3sDatastore};

const SKIP_KINDS: &[&str] = &[
    "ComponentStatus",
    "ControllerRevision",
    "Endpoints",
    "EndpointSlice",
    "Event",
    "Lease",
    "Node",
    "Pod",
    "ReplicaSet",
    "VolumeAttachment",
];
const SOURCE_UID_ANNOTATION: &str = "nodemigrate.io/source-uid";

#[derive(Debug, Clone)]
pub struct KubeApi {
    kubeconfig: PathBuf,
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
        let client = Client::try_from(kubeconfig).context("building Kubernetes API client")?;
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

    pub fn delete_node(&self, name: &str) -> Result<()> {
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
            api.delete(name, &DeleteParams::default())
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
        Ok(HostPathSnapshot { directory, backups })
    }

    pub fn export(&self, installation: &Installation) -> Result<Export> {
        self.ready()?;
        let node_name = installation_node_name(installation);
        let (runtime, client) = self.connected()?;
        let (objects, labels) = runtime.block_on(async {
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
                            let value = serde_json::to_value(object)
                                .context("serializing Kubernetes object")?;
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
            Ok::<_, anyhow::Error>((objects, labels))
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
        let mut objects: Vec<Value> = objects.into_iter().filter_map(sanitize).collect();
        objects.sort_by_key(object_rank);

        let dir = export_directory()?;
        let mut paths = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            let path = dir.join(format!("{index:08}.json"));
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .with_context(|| format!("creating migration object {}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
            }
            serde_json::to_writer(file, object).context("writing protected Kubernetes object")?;
            paths.push(path);
        }
        Ok(Export {
            dir,
            paths,
            host_paths,
            host_path_backups: Vec::new(),
        })
    }

    pub fn import(&self, export: &Export) -> Result<()> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let mut pending = export.paths.clone();
            let mut last_error = String::new();
            let mut uid_map = HashMap::new();
            for attempt in 0..5 {
                let discovery = Discovery::new(client.clone()).run().await.context("discovering destination Kubernetes APIs")?;
                let mut retry = Vec::new();
                for path in pending {
                    let value: Value = serde_json::from_slice(
                        &fs::read(&path).with_context(|| format!("reading {}", path.display()))?
                    ).context("decoding protected migration object")?;
                    let mut initial = value.clone();
                    if let Some(metadata) = initial.pointer_mut("/metadata").and_then(Value::as_object_mut) {
                        metadata.remove("ownerReferences");
                    }
                    if let Some(claim_ref) = initial.pointer_mut("/spec/claimRef").and_then(Value::as_object_mut) {
                        claim_ref.remove("uid");
                    }
                    match apply_object(&client, &discovery, &initial).await {
                        Ok(applied) => {
                            if let (Some(source_uid), Some(destination_uid)) = (
                                value.pointer("/metadata/annotations/nodemigrate.io~1source-uid").and_then(Value::as_str),
                                applied.metadata.uid.as_deref(),
                            ) {
                                uid_map.insert(source_uid.to_owned(), destination_uid.to_owned());
                            }
                        }
                        Err(error) => {
                            last_error = error.to_string();
                            retry.push(path);
                        }
                    }
                }
                pending = retry;
                if pending.is_empty() { break; }
                if attempt < 4 { tokio::time::sleep(std::time::Duration::from_secs(3)).await; }
            }
            if !pending.is_empty() {
                bail!("{} Kubernetes objects could not be restored; export retained at {}. Last error: {}", pending.len(), export.dir.display(), last_error)
            }
            let discovery = Discovery::new(client.clone()).run().await.context("discovering destination APIs for reference repair")?;
            for path in &export.paths {
                let mut value: Value = serde_json::from_slice(&fs::read(path)
                    .with_context(|| format!("reading {}", path.display()))?)
                    .context("decoding protected migration object")?;
                let mut changed = false;
                {
                    let metadata = value.pointer_mut("/metadata").and_then(Value::as_object_mut)
                        .context("migration object is missing metadata")?;
                    if let Some(annotations) = metadata.get_mut("annotations").and_then(Value::as_object_mut) {
                        changed |= annotations.remove(SOURCE_UID_ANNOTATION).is_some();
                        if annotations.is_empty() { metadata.remove("annotations"); }
                    }
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
                        .with_context(|| format!("repairing references in {}", path.display()))?;
                }
            }
            Ok(())
        })
    }
}

#[derive(Debug)]
pub struct Export {
    pub dir: PathBuf,
    paths: Vec<PathBuf>,
    host_paths: Vec<PathBuf>,
    host_path_backups: Vec<HostPathBackup>,
}

#[derive(Debug)]
pub struct HostPathSnapshot {
    directory: PathBuf,
    backups: Vec<HostPathBackup>,
}

#[derive(Debug)]
struct HostPathBackup {
    source: PathBuf,
    backup: PathBuf,
    directory: bool,
}

impl Export {
    pub fn snapshot_host_paths(&mut self) -> Result<()> {
        self.host_path_backups = backup_host_paths(&self.dir, &self.host_paths)?;
        Ok(())
    }

    pub fn restore_host_paths(&self) -> Result<()> {
        restore_host_path_backups(&self.host_path_backups)
    }
}

impl HostPathSnapshot {
    pub fn recovery_directory(&self) -> &Path {
        &self.directory
    }

    pub fn restore(&self) -> Result<()> {
        restore_host_path_backups(&self.backups)
    }
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
    let backup_root = directory.join("host-paths");
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

fn sanitize(mut object: Value) -> Option<Value> {
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
    if let Some(source_uid) = source_uid {
        metadata
            .entry("annotations")
            .or_insert_with(|| serde_json::json!({}));
        metadata
            .get_mut("annotations")?
            .as_object_mut()?
            .insert(SOURCE_UID_ANNOTATION.to_string(), Value::String(source_uid));
    }
    Some(object)
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
    use super::persistent_host_paths;
    use std::collections::HashMap;

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
