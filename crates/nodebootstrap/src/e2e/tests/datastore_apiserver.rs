//! A real upstream kube-apiserver backed by a throwaway nodestore.

use super::context::E2eContext;
use super::datastore::DatastoreProcess;
use super::skip_test;
use anyhow::{Context, Result};
use serde_json::Value;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const KUBE_APISERVER_VERSION: &str = "v1.33.0";
const API_TOKEN: &str = "nodebootstrap-e2e-token";

fn command_available(command: &str) -> bool {
    Command::new(command)
        .arg("--help")
        .output()
        .is_ok_and(|output| output.status.success() || output.status.code() == Some(1))
}

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

trait PermissionsExt {
    fn set_mode(&mut self, mode: u32);
}

impl PermissionsExt for std::fs::Permissions {
    fn set_mode(&mut self, mode: u32) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as UnixPermissionsExt;
            *self = UnixPermissionsExt::from_mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
    }
}

fn run_success(command: &mut Command, description: &str) -> Result<Output> {
    let output = command.output().with_context(|| description.to_string())?;
    anyhow::ensure!(
        output.status.success(),
        "{description} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

struct ApiServerHarness {
    store: DatastoreProcess,
    api: Option<Child>,
    root: PathBuf,
    kubeconfig: PathBuf,
    port: u16,
}

impl ApiServerHarness {
    fn start() -> Result<Self> {
        if !command_available("kubectl") {
            return Err(skip_test("datastore apiserver tests require kubectl"));
        }
        if !command_available("openssl") {
            return Err(skip_test("datastore apiserver tests require openssl"));
        }
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
        run_success(
            Command::new("openssl")
                .args(["genrsa", "-out"])
                .arg(root.join("sa.key"))
                .arg("2048"),
            "creating the apiserver service-account key",
        )?;
        run_success(
            Command::new("openssl")
                .args(["rsa", "-in"])
                .arg(root.join("sa.key"))
                .args(["-pubout", "-out"])
                .arg(root.join("sa.pub")),
            "creating the apiserver service-account public key",
        )?;
        fs::write(root.join("tokens.csv"), format!("{API_TOKEN},e2e-admin,e2e-admin,system:masters\n"))?;
        let port = std::env::var("NODESTORE_APISERVER_TEST_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(17_443);
        let kubeconfig = root.join("kubeconfig");
        fs::write(
            &kubeconfig,
            format!(
                "apiVersion: v1\nkind: Config\nclusters:\n- cluster:\n    server: https://127.0.0.1:{port}\n    insecure-skip-tls-verify: true\n  name: nodestore-e2e\ncontexts:\n- context: {{cluster: nodestore-e2e, user: e2e-admin}}\n  name: nodestore-e2e\ncurrent-context: nodestore-e2e\nusers:\n- name: e2e-admin\n  user:\n    token: {API_TOKEN}\n"
            ),
        )?;
        let mut harness = Self {
            store,
            api: None,
            root,
            kubeconfig,
            port,
        };
        harness.start_api(binary)?;
        harness.wait_ready()?;
        Ok(harness)
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

    fn kubectl(&self, args: &[&str]) -> Result<Output> {
        Command::new("kubectl")
            .arg("--kubeconfig")
            .arg(&self.kubeconfig)
            .args(args)
            .output()
            .context("running kubectl against throwaway apiserver")
    }

    fn kubectl_success(&self, args: &[&str]) -> Result<String> {
        let output = self.kubectl(args)?;
        anyhow::ensure!(
            output.status.success(),
            "kubectl {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn wait_ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if self.kubectl(&["get", "--raw", "/readyz"]).is_ok_and(|output| output.status.success()) {
                return Ok(());
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

    fn restart_api(&mut self) -> Result<()> {
        self.stop_api();
        self.store.restart()?;
        let binary = kube_apiserver_binary()?;
        self.start_api(binary)?;
        self.wait_ready()
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
    let harness = ApiServerHarness::start()?;
    let namespaces = harness.kubectl_success(&["get", "namespaces", "-o", "name"])?;
    anyhow::ensure!(namespaces.contains("namespace/default"));
    anyhow::ensure!(namespaces.contains("namespace/kube-system"));
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
    let harness = ApiServerHarness::start()?;
    harness.kubectl_success(&["create", "namespace", "nodestore-crud"])?;
    harness.kubectl_success(&[
        "-n",
        "nodestore-crud",
        "create",
        "configmap",
        "probe",
        "--from-literal=k=v1",
    ])?;
    anyhow::ensure!(
        harness.kubectl_success(&["-n", "nodestore-crud", "get", "configmap", "probe", "-o", "jsonpath={.data.k}"])?
            == "v1"
    );
    let rv1 = harness.kubectl_success(&[
        "-n", "nodestore-crud", "get", "configmap", "probe", "-o", "jsonpath={.metadata.resourceVersion}",
    ])?;
    harness.kubectl_success(&[
        "-n", "nodestore-crud", "create", "configmap", "probe", "--from-literal=k=v2", "--dry-run=client", "-o", "yaml",
    ])?;
    harness.kubectl_success(&[
        "-n", "nodestore-crud", "patch", "configmap", "probe", "--type=merge", "-p", r#"{"data":{"k":"v2"}}"#,
    ])?;
    let rv2 = harness.kubectl_success(&[
        "-n", "nodestore-crud", "get", "configmap", "probe", "-o", "jsonpath={.metadata.resourceVersion}",
    ])?;
    anyhow::ensure!(rv1 != rv2, "CRUD update did not advance resourceVersion");
    harness.kubectl_success(&["-n", "nodestore-crud", "delete", "configmap", "probe"])?;
    Ok(())
}

pub(super) async fn apiserver_watch_delivers_through_nodestore(
    _context: &E2eContext,
) -> Result<()> {
    let harness = ApiServerHarness::start()?;
    harness.kubectl_success(&["create", "namespace", "nodestore-watch"])?;
    let path = harness.root.join("watch.json");
    let output = File::create(&path)?;
    let mut watch = Command::new("kubectl")
        .arg("--kubeconfig")
        .arg(&harness.kubeconfig)
        .args(["-n", "nodestore-watch", "get", "configmaps", "--watch", "--output-watch-events", "-o", "json"])
        .stdout(Stdio::from(output))
        .stderr(Stdio::null())
        .spawn()
        .context("starting apiserver watch")?;
    std::thread::sleep(Duration::from_secs(2));
    harness.kubectl_success(&["-n", "nodestore-watch", "create", "configmap", "watched", "--from-literal=k=v"])?;
    harness.kubectl_success(&["-n", "nodestore-watch", "delete", "configmap", "watched"])?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let body = fs::read_to_string(&path).unwrap_or_default();
        if body.contains("DELETED") {
            let _ = watch.kill();
            let _ = watch.wait();
            anyhow::ensure!(body.contains("watched"), "DELETE watch event lost object identity");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = watch.kill();
    let _ = watch.wait();
    anyhow::bail!("apiserver watch did not deliver the deletion: {}", fs::read_to_string(path).unwrap_or_default())
}

pub(super) async fn apiserver_state_survives_a_datastore_restart(
    _context: &E2eContext,
) -> Result<()> {
    let mut harness = ApiServerHarness::start()?;
    harness.kubectl_success(&["create", "namespace", "nodestore-durable"])?;
    harness.kubectl_success(&[
        "-n", "nodestore-durable", "create", "configmap", "kept", "--from-literal=k=v",
    ])?;
    harness.restart_api()?;
    anyhow::ensure!(
        harness.kubectl_success(&[
            "-n", "nodestore-durable", "get", "configmap", "kept", "-o", "jsonpath={.data.k}",
        ])? == "v"
    );
    Ok(())
}
