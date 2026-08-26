//! A real upstream kube-apiserver backed by a throwaway nodestore.

use super::context::E2eContext;
use super::datastore::DatastoreProcess;
use super::skip_test;
use anyhow::{Context, Result};
use futures::StreamExt;
use k8s_openapi::api::core::v1::{ConfigMap, Namespace};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams, WatchEvent, WatchParams};
use kube::{Client, Config as KubeConfig};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const KUBE_APISERVER_VERSION: &str = "v1.33.0";
const API_TOKEN: &str = "nodebootstrap-e2e-token";

fn architecture() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        "arm" => Ok("arm"),
        other => Err(skip_test(format!("kube-apiserver has no test asset for {other}"))),
    }
}

fn kube_apiserver_binary() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("NOTK8S_KUBE_APISERVER_BINARY") {
        if Path::new(&path).is_file() {
            return Ok(path.into());
        }
        return Err(skip_test(format!("NOTK8S_KUBE_APISERVER_BINARY is not a file: {path}")));
    }
    let arch = architecture()?;
    let cache_root = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("nodebootstrap-cache"))
        .join("notk8s");
    fs::create_dir_all(&cache_root)?;
    let path = cache_root.join(format!("kube-apiserver-{KUBE_APISERVER_VERSION}-{arch}"));
    if path.is_file() {
        return Ok(path);
    }
    let url = format!(
        "https://dl.k8s.io/release/{KUBE_APISERVER_VERSION}/bin/linux/{arch}/kube-apiserver"
    );
    let response = ureq::get(&url).call().map_err(|error| {
        skip_test(format!("could not fetch kube-apiserver {KUBE_APISERVER_VERSION}: {error}"))
    })?;
    let partial = path.with_extension("partial");
    let mut output = File::create(&partial)?;
    let mut reader = response.into_reader();
    std::io::copy(&mut reader, &mut output)?;
    output.flush()?;
    drop(output);
    fs::rename(&partial, &path)?;
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions)?;
    Ok(path)
}

struct ApiServerHarness {
    store: DatastoreProcess,
    api: Option<Child>,
    root: PathBuf,
    port: u16,
}

impl ApiServerHarness {
    async fn start() -> Result<Self> {
        let binary = kube_apiserver_binary()?;
        let store = DatastoreProcess::start()?;
        let root = std::env::temp_dir().join(format!(
            "nodebootstrap-datastore-apiserver-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&root)?;
        let pki = crate::pki::generate(&crate::pki::ClusterPkiSpec {
            service_ip: "127.0.0.1".parse()?,
            extra_sans: Vec::new(),
        })
        .context("generating throwaway apiserver PKI")?;
        fs::write(root.join("sa.key"), pki.sa_signing.key_pem)?;
        fs::write(root.join("sa.pub"), pki.sa_signing.cert_pem)?;
        fs::write(root.join("serving-ca.crt"), pki.ca.cert_pem)?;
        fs::write(root.join("serving.crt"), pki.apiserver_serving.cert_pem)?;
        fs::write(root.join("serving.key"), pki.apiserver_serving.key_pem)?;
        fs::write(root.join("tokens.csv"), format!("{API_TOKEN},e2e-admin,e2e-admin,system:masters\n"))?;
        let port = std::env::var("NODESTORE_APISERVER_TEST_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(17_443);
        let mut harness = Self {
            store,
            api: None,
            root,
            port,
        };
        harness.start_api(binary)?;
        harness.wait_ready().await?;
        Ok(harness)
    }

    fn client(&self) -> Result<Client> {
        let mut config = KubeConfig::new(format!("https://127.0.0.1:{}", self.port).parse()?);
        let ca = pem::parse(fs::read(self.root.join("serving-ca.crt"))?)?;
        config.root_cert = Some(vec![ca.into_contents()]);
        config.auth_info.token = Some(API_TOKEN.to_owned().into());
        config.default_namespace = "default".to_owned();
        Client::try_from(config).context("building kube-rs client for throwaway apiserver")
    }

    fn start_api(&mut self, binary: PathBuf) -> Result<()> {
        let log = File::create(self.root.join("apiserver.log"))?;
        let child = Command::new(binary)
            .arg(format!("--etcd-servers=https://{}", self.store.address()))
            .arg(format!("--etcd-cafile={}", self.store.client_dir().join("ca.crt").display()))
            .arg(format!("--etcd-certfile={}", self.store.client_dir().join("client.crt").display()))
            .arg(format!("--etcd-keyfile={}", self.store.client_dir().join("client.key").display()))
            .arg(format!("--secure-port={}", self.port))
            .arg(format!("--cert-dir={}", self.root.join("certs").display()))
            .arg(format!("--tls-cert-file={}", self.root.join("serving.crt").display()))
            .arg(format!("--tls-private-key-file={}", self.root.join("serving.key").display()))
            .arg(format!("--token-auth-file={}", self.root.join("tokens.csv").display()))
            .arg(format!("--service-account-key-file={}", self.root.join("sa.pub").display()))
            .arg(format!("--service-account-signing-key-file={}", self.root.join("sa.key").display()))
            .args([
                "--service-account-issuer=https://kubernetes.default.svc",
                "--service-cluster-ip-range=10.144.0.0/16",
                "--authorization-mode=AlwaysAllow",
                "--allow-privileged=true",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .context("starting standalone kube-apiserver")?;
        self.api = Some(child);
        Ok(())
    }

    async fn wait_ready(&mut self) -> Result<()> {
        let client = self.client()?;
        let namespaces: Api<Namespace> = Api::all(client);
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Ok(list) = namespaces.list(&ListParams::default()).await {
                let names: std::collections::HashSet<_> = list
                    .items
                    .into_iter()
                    .filter_map(|namespace| namespace.metadata.name)
                    .collect();
                if names.contains("default") && names.contains("kube-system") {
                    return Ok(());
                }
            }
            if self
                .api
                .as_mut()
                .map(|api| api.try_wait().ok().flatten().is_some())
                .unwrap_or(true)
            {
                anyhow::bail!("standalone kube-apiserver exited; log: {}", self.root.join("apiserver.log").display());
            }
            if Instant::now() >= deadline {
                anyhow::bail!("standalone kube-apiserver did not become ready; log: {}", self.root.join("apiserver.log").display());
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn stop_api(&mut self) {
        if let Some(mut api) = self.api.take() {
            let _ = api.kill();
            let _ = api.wait();
        }
    }

    async fn restart_api(&mut self) -> Result<()> {
        self.stop_api();
        self.store.restart()?;
        let binary = kube_apiserver_binary()?;
        self.start_api(binary)?;
        self.wait_ready().await
    }
}

impl Drop for ApiServerHarness {
    fn drop(&mut self) {
        self.stop_api();
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub(super) async fn real_apiserver_starts_and_serves_against_nodestore(
    _context: &E2eContext,
) -> Result<()> {
    let harness = ApiServerHarness::start().await?;
    let client = harness.client()?;
    let namespaces: Api<Namespace> = Api::all(client);
    let names = namespaces
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .filter_map(|namespace| namespace.metadata.name)
        .collect::<Vec<_>>();
    anyhow::ensure!(names.iter().any(|name| name == "default"));
    anyhow::ensure!(names.iter().any(|name| name == "kube-system"));
    let range: Value = serde_json::from_str(&harness.store.rpc(
        "etcdserverpb.KV/Range",
        r#"{"key":"L3JlZ2lzdHJ5Lw==","rangeEnd":"L3JlZ2lzdHJ5MA=="}"#,
    )?)?;
    anyhow::ensure!(
        range.get("count").and_then(Value::as_str).is_some() || range.get("kvs").is_some(),
        "nodestore did not return the apiserver registry range"
    );
    Ok(())
}

pub(super) async fn apiserver_crud_round_trips_through_nodestore(
    _context: &E2eContext,
) -> Result<()> {
    let harness = ApiServerHarness::start().await?;
    let client = harness.client()?;
    let namespaces: Api<Namespace> = Api::all(client.clone());
    namespaces
        .create(
            &PostParams::default(),
            &Namespace {
                metadata: ObjectMeta {
                    name: Some("nodestore-crud".to_owned()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await?;
    let configmaps: Api<ConfigMap> = Api::namespaced(client, "nodestore-crud");
    let mut data = BTreeMap::new();
    data.insert("k".to_owned(), "v1".to_owned());
    let created = configmaps
        .create(
            &PostParams::default(),
            &ConfigMap {
                metadata: ObjectMeta {
                    name: Some("probe".to_owned()),
                    ..Default::default()
                },
                data: Some(data),
                ..Default::default()
            },
        )
        .await?;
    anyhow::ensure!(created.data.as_ref().and_then(|data| data.get("k")) == Some(&"v1".to_owned()));
    let rv1 = created.metadata.resource_version.clone();
    let updated = configmaps
        .patch(
            "probe",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({"data": {"k": "v2"}})),
        )
        .await?;
    anyhow::ensure!(updated.data.as_ref().and_then(|data| data.get("k")) == Some(&"v2".to_owned()));
    let rv2 = updated.metadata.resource_version;
    anyhow::ensure!(rv1 != rv2, "CRUD update did not advance resourceVersion");
    configmaps.delete("probe", &DeleteParams::default()).await?;
    Ok(())
}

pub(super) async fn apiserver_watch_delivers_through_nodestore(
    _context: &E2eContext,
) -> Result<()> {
    let harness = ApiServerHarness::start().await?;
    let client = harness.client()?;
    let namespaces: Api<Namespace> = Api::all(client.clone());
    namespaces
        .create(
            &PostParams::default(),
            &Namespace {
                metadata: ObjectMeta {
                    name: Some("nodestore-watch".to_owned()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await?;
    let configmaps: Api<ConfigMap> = Api::namespaced(client, "nodestore-watch");
    let watch = configmaps.watch(&WatchParams::default(), "0").await?;
    futures::pin_mut!(watch);
    let mut data = BTreeMap::new();
    data.insert("k".to_owned(), "v".to_owned());
    configmaps
        .create(
            &PostParams::default(),
            &ConfigMap {
                metadata: ObjectMeta {
                    name: Some("watched".to_owned()),
                    ..Default::default()
                },
                data: Some(data),
                ..Default::default()
            },
        )
        .await?;
    configmaps.delete("watched", &DeleteParams::default()).await?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let event = tokio::time::timeout(Duration::from_secs(1), watch.next()).await;
        if let Ok(Some(event)) = event {
            if let WatchEvent::Deleted(configmap) = event? {
                anyhow::ensure!(configmap.metadata.name.as_deref() == Some("watched"), "DELETE watch event lost object identity");
                return Ok(());
            }
        }
    }
    anyhow::bail!("apiserver watch did not deliver the deletion")
}

pub(super) async fn apiserver_state_survives_a_datastore_restart(
    _context: &E2eContext,
) -> Result<()> {
    let mut harness = ApiServerHarness::start().await?;
    let client = harness.client()?;
    let namespaces: Api<Namespace> = Api::all(client.clone());
    namespaces
        .create(
            &PostParams::default(),
            &Namespace {
                metadata: ObjectMeta {
                    name: Some("nodestore-durable".to_owned()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await?;
    let configmaps: Api<ConfigMap> = Api::namespaced(client, "nodestore-durable");
    let mut data = BTreeMap::new();
    data.insert("k".to_owned(), "v".to_owned());
    configmaps
        .create(
            &PostParams::default(),
            &ConfigMap {
                metadata: ObjectMeta {
                    name: Some("kept".to_owned()),
                    ..Default::default()
                },
                data: Some(data),
                ..Default::default()
            },
        )
        .await?;
    harness.restart_api().await?;
    let kept = configmaps.get("kept").await?;
    anyhow::ensure!(kept.data.as_ref().and_then(|data| data.get("k")) == Some(&"v".to_owned()));
    Ok(())
}
