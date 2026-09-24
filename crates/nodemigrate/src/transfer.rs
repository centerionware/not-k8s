//! Kubernetes API object transfer. Datastore files are distribution-specific;
//! reading and applying resources through the API avoids copying Kine/etcd
//! storage into nodestore.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, ensure, Context, Result};
use kube::{
    api::{Api, DynamicObject, ListParams, Patch, PatchParams},
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
const SYSTEM_NAMESPACES: &[&str] = &["kube-system", "kube-public", "kube-node-lease"];

#[derive(Debug, Clone)]
pub struct KubeApi {
    kubeconfig: PathBuf,
}

impl KubeApi {
    pub fn source(installation: &Installation) -> Result<Self> {
        let kubeconfig = std::env::var_os("KUBECONFIG")
            .map(PathBuf::from)
            .or_else(|| {
                installation
                    .cluster
                    .as_ref()
                    .and_then(|cluster| cluster.kubeconfig.clone())
            })
            .unwrap_or_else(|| match installation.distribution {
                crate::request::Distribution::K3s => PathBuf::from("/etc/rancher/k3s/k3s.yaml"),
                _ => PathBuf::from("/etc/kubernetes/admin.conf"),
            });
        ensure!(
            kubeconfig.is_file(),
            "source kubeconfig {} does not exist",
            kubeconfig.display()
        );
        Ok(Self { kubeconfig })
    }

    pub fn destination() -> Result<Self> {
        let directory = std::env::var_os("NODEBOOTSTRAP_KUBECONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/etc/nodebootstrap"));
        let kubeconfig = directory.join("admin.kubeconfig");
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

    pub fn single_node(&self) -> Result<()> {
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
            ensure!(
                nodes.items.len() == 1,
                "migration requires a single-node source cluster; found {} nodes",
                nodes.items.len()
            );
            Ok(())
        })
    }

    pub fn validate_no_volumes(&self) -> Result<()> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let discovery = Discovery::new(client.clone())
                .run()
                .await
                .context("discovering source Kubernetes APIs")?;
            for kind in ["PersistentVolume", "PersistentVolumeClaim"] {
                let Some((resource, capabilities)) = find_resource(&discovery, kind, "v1") else {
                    continue;
                };
                ensure!(
                    capabilities.supports_operation(verbs::LIST),
                    "Kubernetes API cannot list {kind}"
                );
                let api: Api<DynamicObject> = Api::all_with(client.clone(), &resource);
                let objects = api
                    .list(&ListParams::default())
                    .await
                    .with_context(|| format!("checking source {kind} objects"))?;
                ensure!(
                    objects.items.is_empty(),
                    "source cluster has {} {kind} objects; volume data transfer is not implemented",
                    objects.items.len()
                );
            }
            Ok(())
        })
    }

    pub fn export(&self, installation: &Installation) -> Result<Export> {
        self.ready()?;
        self.single_node()?;
        self.validate_no_volumes()?;
        let (runtime, client) = self.connected()?;
        let objects = runtime.block_on(async {
            let discovery = Discovery::new(client.clone())
                .run()
                .await
                .context("discovering source Kubernetes APIs")?;
            let mut objects = Vec::new();
            for group in discovery.groups() {
                for (resource, capabilities) in group.recommended_resources() {
                    if !capabilities.supports_operation(verbs::LIST) || skip_kind(&resource.kind) {
                        continue;
                    }
                    let api: Api<DynamicObject> = Api::all_with(client.clone(), &resource);
                    let list = api.list(&ListParams::default()).await.with_context(|| {
                        format!("listing {} objects from the source API", resource.kind)
                    })?;
                    for object in list.items {
                        let value = serde_json::to_value(object)
                            .context("serializing Kubernetes object")?;
                        if !skip_object(&value) {
                            objects.push(value);
                        }
                    }
                }
            }
            Ok::<_, anyhow::Error>(objects)
        })?;

        if let Some(cluster) = &installation.cluster {
            if cluster.datastore == K3sDatastore::Etcd {
                tracing::warn!(
                    "K3s uses embedded/external etcd; only Kubernetes API objects will be migrated"
                );
            }
        }
        ensure!(
            !objects.iter().any(is_persistent_volume_object),
            "source cluster has PersistentVolume or PersistentVolumeClaim objects; volume data transfer is not implemented, so no service was changed"
        );
        ensure!(
            !objects.is_empty(),
            "source API returned no migratable objects"
        );
        let mut objects: Vec<Value> = objects.into_iter().filter_map(sanitize).collect();
        ensure!(
            !objects.iter().any(has_webhook_conversion),
            "source has a CRD conversion webhook whose service is not transferred; refusing migration"
        );
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
        Ok(Export { dir, paths })
    }

    pub fn import(&self, export: &Export) -> Result<()> {
        let (runtime, client) = self.connected()?;
        runtime.block_on(async {
            let mut pending = export.paths.clone();
            let mut last_error = String::new();
            for attempt in 0..5 {
                let discovery = Discovery::new(client.clone()).run().await.context("discovering destination Kubernetes APIs")?;
                let mut retry = Vec::new();
                for path in pending {
                    let value: Value = serde_json::from_slice(
                        &fs::read(&path).with_context(|| format!("reading {}", path.display()))?
                    ).context("decoding protected migration object")?;
                    let result = apply_object(&client, &discovery, &value).await;
                    if let Err(error) = result {
                        last_error = error.to_string();
                        retry.push(path);
                    }
                }
                pending = retry;
                if pending.is_empty() { return Ok(()); }
                if attempt < 4 { tokio::time::sleep(std::time::Duration::from_secs(3)).await; }
            }
            bail!("{} Kubernetes objects could not be restored; export retained at {}. Last error: {}", pending.len(), export.dir.display(), last_error)
        })
    }
}

#[derive(Debug)]
pub struct Export {
    pub dir: PathBuf,
    paths: Vec<PathBuf>,
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

async fn apply_object(client: &Client, discovery: &Discovery, value: &Value) -> Result<()> {
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
    api.patch(
        name,
        &PatchParams::apply("nodemigrate"),
        &Patch::Apply(&object),
    )
    .await
    .with_context(|| format!("applying {type_meta}/{kind} {name}"))?;
    Ok(())
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
    let namespace = object
        .pointer("/metadata/namespace")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if SYSTEM_NAMESPACES.contains(&namespace) {
        return true;
    }
    if object.get("kind").and_then(Value::as_str) == Some("Namespace")
        && object
            .pointer("/metadata/name")
            .and_then(Value::as_str)
            .is_some_and(|name| SYSTEM_NAMESPACES.contains(&name))
    {
        return true;
    }
    object.get("kind").and_then(Value::as_str) == Some("Secret")
        && object.pointer("/type").and_then(Value::as_str)
            == Some("kubernetes.io/service-account-token")
}

fn has_webhook_conversion(object: &Value) -> bool {
    object.get("kind").and_then(Value::as_str) == Some("CustomResourceDefinition")
        && object
            .pointer("/spec/conversion/strategy")
            .and_then(Value::as_str)
            == Some("Webhook")
}

fn is_persistent_volume_object(object: &Value) -> bool {
    matches!(
        object.get("kind").and_then(Value::as_str),
        Some("PersistentVolume" | "PersistentVolumeClaim")
    )
}

fn sanitize(mut object: Value) -> Option<Value> {
    if skip_object(&object) {
        return None;
    }
    object.as_object_mut()?.remove("status");
    let metadata = object.get_mut("metadata")?.as_object_mut()?;
    for field in [
        "creationTimestamp",
        "deletionTimestamp",
        "deletionGracePeriodSeconds",
        "generation",
        "managedFields",
        "resourceVersion",
        "selfLink",
        "uid",
    ] {
        metadata.remove(field);
    }
    // Old UIDs do not exist in the new cluster. Retaining owner references
    // would make the garbage collector delete imported resources.
    metadata.remove("ownerReferences");
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
