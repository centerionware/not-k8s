use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const RPC_PROTO_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../nodestore/proto");

fn b64(value: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(value)
}

fn nodestore_binary() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("NOTK8S_NODESTORE_E2E_BINARY") {
        if Path::new(&path).is_file() {
            return Ok(path.into());
        }
    }
    let cfg = crate::config::Config::from_env()?;
    let path = cfg.toolchain_dir().join("bin/nodestore");
    if path.is_file() {
        Ok(path)
    } else {
        Err(skip_test(format!(
            "nodestore binary is not installed at {}; provide NOTK8S_NODESTORE_E2E_BINARY",
            path.display()
        )))
    }
}

fn grpcurl_available() -> bool {
    Command::new("grpcurl")
        .arg("-version")
        .output()
        .is_ok_and(|output| output.status.success())
}

pub(super) struct DatastoreProcess {
    child: Child,
    dir: PathBuf,
    address: String,
    peer_address: String,
    log_path: PathBuf,
}

impl DatastoreProcess {
    pub(super) fn start() -> Result<Self> {
        if !grpcurl_available() {
            return Err(skip_test("datastore wire tests require grpcurl"));
        }
        let binary = nodestore_binary()?;
        let dir = std::env::temp_dir().join(format!(
            "nodebootstrap-datastore-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir)?;
        let port = std::env::var("NODESTORE_E2E_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(23_790);
        let address = format!("127.0.0.1:{port}");
        let peer_address = format!("127.0.0.1:{}", port + 100);
        let log_path = dir.join("nodestore.log");
        let child = Command::new(&binary)
            .arg("nodestore")
            .env("NODESTORE_LISTEN", &address)
            .env("NODESTORE_DATA_DIR", dir.join("data"))
            .stdout(Stdio::null())
            .stderr(Stdio::from(File::create(&log_path)?))
            .spawn()
            .with_context(|| format!("starting {}", binary.display()))?;
        let process = Self {
            child,
            dir,
            address,
            peer_address,
            log_path,
        };
        process.wait_ready()?;
        Ok(process)
    }

    pub(super) fn client_dir(&self) -> PathBuf {
        self.dir.join("data/pki/client")
    }

    pub(super) fn data_dir(&self) -> PathBuf {
        self.dir.join("data")
    }

    pub(super) fn address(&self) -> &str {
        &self.address
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn cluster_child(&self, initial_cluster: &str) -> Result<Child> {
        let binary = nodestore_binary()?;
        Ok(Command::new(binary)
            .arg("nodestore")
            .env("NODESTORE_MEMBER_ID", "1")
            .env("NODESTORE_INITIAL_CLUSTER", initial_cluster)
            .env("NODESTORE_LISTEN", &self.address)
            .env("NODESTORE_PEER_LISTEN", &self.peer_address)
            .env(
                "NODESTORE_ADVERTISE_CLIENT_URL",
                format!("https://{}", self.address),
            )
            .env(
                "NODESTORE_ADVERTISE_PEER_URL",
                format!("https://{}", self.peer_address),
            )
            .env("NODESTORE_DATA_DIR", self.dir.join("data"))
            .stdout(Stdio::null())
            .stderr(Stdio::from(File::create(&self.log_path)?))
            .spawn()
            .context("starting nodestore with cluster configuration")?)
    }

    fn restart_as_one_member_cluster(&mut self) -> Result<()> {
        self.stop();
        let spec = format!("1=https://{}", self.peer_address);
        self.child = self.cluster_child(&spec)?;
        self.wait_ready()
    }

    fn start_with_unsafe_multi_member_configuration(&mut self) -> Result<String> {
        self.stop();
        let peer_port = self
            .peer_address
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
            .context("peer address had no numeric port")?;
        let spec = format!(
            "1=https://{},2=https://127.0.0.1:{},3=https://127.0.0.1:{}",
            self.peer_address,
            peer_port + 1,
            peer_port + 2
        );
        let mut child = self.cluster_child(&spec)?;
        let status = child.wait()?;
        anyhow::ensure!(
            !status.success(),
            "unsafe multi-member conversion unexpectedly started"
        );
        Ok(fs::read_to_string(&self.log_path).unwrap_or_default())
    }

    pub(super) fn rpc(&self, method: &str, request: &str) -> Result<String> {
        let pki = self.client_dir();
        let ca = pki.join("ca.crt");
        let cert = pki.join("client.crt");
        let key = pki.join("client.key");
        let output = Command::new("grpcurl")
            .args([
                "-cacert",
                ca.to_str().context("CA path is not UTF-8")?,
                "-cert",
                cert.to_str().context("client cert path is not UTF-8")?,
                "-key",
                key.to_str().context("client key path is not UTF-8")?,
                "-max-time",
                "10",
                "-import-path",
                RPC_PROTO_DIR,
                "-proto",
                "rpc.proto",
                "-d",
                request,
                &self.address,
                method,
            ])
            .output()
            .with_context(|| format!("calling {method} through grpcurl"))?;
        anyhow::ensure!(
            output.status.success(),
            "grpcurl {method} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn wait_ready(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if self
                .rpc("etcdserverpb.Maintenance/Status", "{}")
                .is_ok()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!("nodestore never answered on {}", self.address);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    pub(super) fn restart(&mut self) -> Result<()> {
        self.stop();
        let binary = nodestore_binary()?;
        self.child = Command::new(binary)
            .arg("nodestore")
            .env("NODESTORE_LISTEN", &self.address)
            .env("NODESTORE_DATA_DIR", self.dir.join("data"))
            .stdout(Stdio::null())
            .stderr(Stdio::from(File::create(&self.log_path)?))
            .spawn()
            .context("restarting nodestore")?;
        self.wait_ready()
    }
}

impl Drop for DatastoreProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn get_value(store: &DatastoreProcess, key: &str) -> Result<String> {
    let output = store.rpc(
        "etcdserverpb.KV/Range",
        &json!({"key": b64(key)}).to_string(),
    )?;
    let document: Value = serde_json::from_str(&output)?;
    let Some(encoded) = document
        .pointer("/kvs/0/value")
        .and_then(Value::as_str)
    else {
        return Ok(String::new());
    };
    Ok(String::from_utf8(
        base64::engine::general_purpose::STANDARD.decode(encoded)?,
    )?)
}

fn revision(store: &DatastoreProcess) -> Result<String> {
    let value: Value = serde_json::from_str(
        &store.rpc("etcdserverpb.Maintenance/Status", "{}")?,
    )?;
    value
        .pointer("/header/revision")
        .map(Value::to_string)
        .context("nodestore Status had no revision")
}

async fn start_store() -> Result<DatastoreProcess> {
    DatastoreProcess::start()
}

pub(super) async fn datastore_serves_the_etcd_status_rpc(
    _context: &E2eContext,
) -> Result<()> {
    let store = start_store().await?;
    let output = store.rpc("etcdserverpb.Maintenance/Status", "{}")?;
    anyhow::ensure!(output.contains("\"version\""), "Status had no version field");
    anyhow::ensure!(output.contains("3."), "Status did not report an etcd-compatible version");
    Ok(())
}

pub(super) async fn datastore_round_trips_a_key_over_grpc(
    _context: &E2eContext,
) -> Result<()> {
    let store = start_store().await?;
    store.rpc(
        "etcdserverpb.KV/Put",
        &json!({"key": b64("/registry/test/a"), "value": b64("hello")}).to_string(),
    )?;
    anyhow::ensure!(get_value(&store, "/registry/test/a")? == "hello", "initial value did not round trip");
    store.rpc(
        "etcdserverpb.KV/Put",
        &json!({"key": b64("/registry/test/a"), "value": b64("goodbye")}).to_string(),
    )?;
    anyhow::ensure!(get_value(&store, "/registry/test/a")? == "goodbye", "overwrite did not round trip");
    store.rpc(
        "etcdserverpb.KV/DeleteRange",
        &json!({"key": b64("/registry/test/a")}).to_string(),
    )?;
    anyhow::ensure!(get_value(&store, "/registry/test/a")?.is_empty(), "deleted key still existed");
    Ok(())
}

pub(super) async fn datastore_lists_a_prefix_in_key_order(
    _context: &E2eContext,
) -> Result<()> {
    let store = start_store().await?;
    for key in ["c", "a", "b"] {
        store.rpc(
            "etcdserverpb.KV/Put",
            &json!({"key": b64(&format!("/registry/pods/{key}")), "value": b64(key)}).to_string(),
        )?;
    }
    store.rpc(
        "etcdserverpb.KV/Put",
        &json!({"key": b64("/registry/nodes/n"), "value": b64("n")}).to_string(),
    )?;
    let output: Value = serde_json::from_str(&store.rpc(
        "etcdserverpb.KV/Range",
        &json!({"key": b64("/registry/pods/"), "rangeEnd": b64("/registry/pods0")}).to_string(),
    )?)?;
    let keys = output
        .get("kvs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|kv| kv.get("key").and_then(Value::as_str).map(str::to_string))
        .map(|key| Ok(String::from_utf8(base64::engine::general_purpose::STANDARD.decode(key)?)?))
        .collect::<Result<Vec<_>>>()?;
    anyhow::ensure!(
        keys == ["/registry/pods/a", "/registry/pods/b", "/registry/pods/c"],
        "prefix order was {keys:?}"
    );
    Ok(())
}

pub(super) async fn datastore_enforces_compare_and_swap(
    _context: &E2eContext,
) -> Result<()> {
    let store = start_store().await?;
    let key = b64("/registry/cas");
    store.rpc(
        "etcdserverpb.KV/Put",
        &json!({"key": key, "value": b64("v1")}).to_string(),
    )?;
    let current: Value = serde_json::from_str(&store.rpc(
        "etcdserverpb.KV/Range",
        &json!({"key": b64("/registry/cas")}).to_string(),
    )?)?;
    let stale = current
        .pointer("/kvs/0/modRevision")
        .context("CAS key had no modRevision")?
        .to_string();
    store.rpc(
        "etcdserverpb.KV/Put",
        &json!({"key": b64("/registry/cas"), "value": b64("v2")}).to_string(),
    )?;
    let txn = json!({
        "compare": [{"key": b64("/registry/cas"), "result": "EQUAL", "target": "MOD", "modRevision": stale}],
        "success": [{"requestPut": {"key": b64("/registry/cas"), "value": b64("v3")}}],
        "failure": [{"requestRange": {"key": b64("/registry/cas")}}]
    });
    let output = store.rpc("etcdserverpb.KV/Txn", &txn.to_string())?;
    anyhow::ensure!(!output.contains("\"succeeded\": true"), "stale CAS unexpectedly succeeded");
    anyhow::ensure!(get_value(&store, "/registry/cas")? == "v2", "stale CAS changed the value");
    Ok(())
}

pub(super) async fn datastore_creates_a_key_only_if_absent(
    _context: &E2eContext,
) -> Result<()> {
    let store = start_store().await?;
    let txn = json!({
        "compare": [{"key": b64("/registry/new"), "result": "EQUAL", "target": "MOD", "modRevision": "0"}],
        "success": [{"requestPut": {"key": b64("/registry/new"), "value": b64("first")}}],
        "failure": []
    });
    anyhow::ensure!(
        store.rpc("etcdserverpb.KV/Txn", &txn.to_string())?.contains("\"succeeded\": true"),
        "create-if-absent transaction did not succeed"
    );
    anyhow::ensure!(get_value(&store, "/registry/new")? == "first", "created value was wrong");
    anyhow::ensure!(
        !store.rpc("etcdserverpb.KV/Txn", &txn.to_string())?.contains("\"succeeded\": true"),
        "create-if-absent transaction overwrote an existing key"
    );
    Ok(())
}

fn spawn_watch(store: &DatastoreProcess, request: &str, path: &Path) -> Result<Child> {
    let pki = store.client_dir();
    let output = File::create(path)?;
    Command::new("grpcurl")
        .args([
            "-cacert", pki.join("ca.crt").to_str().context("CA path is not UTF-8")?,
            "-cert", pki.join("client.crt").to_str().context("client cert path is not UTF-8")?,
            "-key", pki.join("client.key").to_str().context("client key path is not UTF-8")?,
            "-max-time", "15", "-import-path", RPC_PROTO_DIR, "-proto", "rpc.proto",
            "-d", request, &store.address, "etcdserverpb.Watch/Watch",
        ])
        .stdout(Stdio::from(output))
        .stderr(Stdio::null())
        .spawn()
        .context("starting nodestore watch stream")
}

pub(super) async fn datastore_streams_watch_events_as_they_happen(
    _context: &E2eContext,
) -> Result<()> {
    let store = start_store().await?;
    let path = store.dir.join("watch.json");
    let request = json!({"createRequest": {"key": b64("/registry/watched/"), "rangeEnd": b64("/registry/watched0")}}).to_string();
    let mut watch = spawn_watch(&store, &request, &path)?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    store.rpc(
        "etcdserverpb.KV/Put",
        &json!({"key": b64("/registry/watched/x"), "value": b64("seen")}).to_string(),
    )?;
    store.rpc(
        "etcdserverpb.KV/DeleteRange",
        &json!({"key": b64("/registry/watched/x")}).to_string(),
    )?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if fs::read_to_string(&path).unwrap_or_default().contains("\"DELETE\"") {
            let _ = watch.kill();
            let body = fs::read_to_string(&path)?;
            anyhow::ensure!(body.contains(&b64("/registry/watched/x")), "watch omitted the watched key");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let _ = watch.kill();
    anyhow::bail!("watch stream did not deliver DELETE: {}", fs::read_to_string(&path).unwrap_or_default());
}

pub(super) async fn datastore_replays_missed_events_to_a_late_watcher(
    _context: &E2eContext,
) -> Result<()> {
    let store = start_store().await?;
    store.rpc(
        "etcdserverpb.KV/Put",
        &json!({"key": b64("/registry/replay/a"), "value": b64("one")}).to_string(),
    )?;
    store.rpc(
        "etcdserverpb.KV/Put",
        &json!({"key": b64("/registry/replay/b"), "value": b64("two")}).to_string(),
    )?;
    let path = store.dir.join("replay.json");
    let request = json!({"createRequest": {"key": b64("/registry/replay/"), "rangeEnd": b64("/registry/replay0"), "startRevision": "1"}}).to_string();
    let mut watch = spawn_watch(&store, &request, &path)?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let _ = watch.kill();
    let body = fs::read_to_string(&path)?;
    anyhow::ensure!(body.contains(&b64("/registry/replay/a")) && body.contains(&b64("/registry/replay/b")), "late watch did not replay both events");
    Ok(())
}

pub(super) async fn datastore_refuses_a_read_below_the_compaction_point(
    _context: &E2eContext,
) -> Result<()> {
    let store = start_store().await?;
    for value in ["v1", "v2"] {
        store.rpc(
            "etcdserverpb.KV/Put",
            &json!({"key": b64("/registry/compact"), "value": b64(value)}).to_string(),
        )?;
    }
    let current = revision(&store)?;
    store.rpc(
        "etcdserverpb.KV/Compact",
        &json!({"revision": current}).to_string(),
    )?;
    let output = store.rpc(
        "etcdserverpb.KV/Range",
        &json!({"key": b64("/registry/compact"), "revision": "1"}).to_string(),
    );
    anyhow::ensure!(output.is_err() && output.unwrap_err().to_string().contains("compacted"), "read below compaction did not fail as compacted");
    anyhow::ensure!(get_value(&store, "/registry/compact")? == "v2", "compaction changed the live value");
    Ok(())
}

pub(super) async fn datastore_expires_a_lease_and_its_keys(
    _context: &E2eContext,
) -> Result<()> {
    let store = start_store().await?;
    let grant: Value = serde_json::from_str(&store.rpc(
        "etcdserverpb.Lease/LeaseGrant",
        r#"{"TTL":"1"}"#,
    )?)?;
    let lease_value = grant.get("ID").context("LeaseGrant returned no ID")?;
    let lease = lease_value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| lease_value.to_string());
    store.rpc(
        "etcdserverpb.KV/Put",
        &json!({"key": b64("/registry/leased"), "value": b64("temp"), "lease": lease}).to_string(),
    )?;
    anyhow::ensure!(get_value(&store, "/registry/leased")? == "temp", "leased key was not written");
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if get_value(&store, "/registry/leased")?.is_empty() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    anyhow::bail!("leased key did not expire");
}

pub(super) async fn datastore_survives_a_restart_with_its_data(
    _context: &E2eContext,
) -> Result<()> {
    let mut store = start_store().await?;
    store.rpc(
        "etcdserverpb.KV/Put",
        &json!({"key": b64("/registry/durable"), "value": b64("persisted")}).to_string(),
    )?;
    let before = revision(&store)?;
    store.restart()?;
    anyhow::ensure!(get_value(&store, "/registry/durable")? == "persisted", "data did not survive restart");
    anyhow::ensure!(revision(&store)? == before, "revision changed across restart");
    Ok(())
}

pub(super) async fn datastore_upgrades_a_populated_single_member_into_a_one_member_cluster(
    _context: &E2eContext,
) -> Result<()> {
    let mut store = start_store().await?;
    store.rpc(
        "etcdserverpb.KV/Put",
        &json!({"key": b64("/registry/before-upgrade"), "value": b64("original")}).to_string(),
    )?;
    anyhow::ensure!(
        !store.data_dir().join("raft.db").exists(),
        "single-member store unexpectedly had a raft log before conversion"
    );
    store.restart_as_one_member_cluster()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if get_value(&store, "/registry/before-upgrade")
            .is_ok_and(|value| value == "original")
        {
            store.rpc(
                "etcdserverpb.KV/Put",
                &json!({"key": b64("/registry/after-upgrade"), "value": b64("committed-by-raft")}).to_string(),
            )?;
            anyhow::ensure!(
                get_value(&store, "/registry/after-upgrade")? == "committed-by-raft",
                "converted one-member cluster did not commit a new write"
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!(
        "converted one-member cluster did not recover its existing data; log: {}",
        fs::read_to_string(&store.log_path).unwrap_or_default()
    )
}

pub(super) async fn datastore_refuses_direct_upgrade_to_a_multi_member_cluster(
    _context: &E2eContext,
) -> Result<()> {
    let mut store = start_store().await?;
    store.rpc(
        "etcdserverpb.KV/Put",
        &json!({"key": b64("/registry/precious"), "value": b64("data")}).to_string(),
    )?;
    let log = store.start_with_unsafe_multi_member_configuration()?;
    anyhow::ensure!(
        log.contains("MemberAdd"),
        "unsafe multi-member conversion did not explain the safe MemberAdd path: {log}"
    );
    anyhow::ensure!(
        store.data_dir().join("state.db").exists(),
        "unsafe conversion removed the existing state database"
    );
    anyhow::ensure!(
        store
            .rpc(
                "etcdserverpb.KV/Put",
                &json!({"key": b64("/registry/should-not-work"), "value": b64("x")}).to_string(),
            )
            .is_err(),
        "unsafe multi-member conversion left the client API serving"
    );
    Ok(())
}

pub(super) async fn datastore_shutdown_leaves_no_listener_behind(
    _context: &E2eContext,
) -> Result<()> {
    let mut store = start_store().await?;
    store.rpc(
        "etcdserverpb.KV/Put",
        &json!({"key": b64("/registry/alive"), "value": b64("yes")}).to_string(),
    )?;
    store.stop();
    anyhow::ensure!(
        store.child.try_wait()?.is_some(),
        "nodestore child remained running after shutdown"
    );
    anyhow::ensure!(
        store
            .rpc("etcdserverpb.Maintenance/Status", "{}")
            .is_err(),
        "nodestore still answered after its process was stopped"
    );
    Ok(())
}

pub(super) async fn datastore_refuses_a_cluster_it_cannot_be_part_of(
    _context: &E2eContext,
) -> Result<()> {
    let binary = nodestore_binary()?;
    let dir = std::env::temp_dir().join(format!("nodebootstrap-invalid-cluster-{}", std::process::id()));
    fs::create_dir_all(&dir)?;
    let output = Command::new(binary)
        .arg("nodestore")
        .env("NODESTORE_LISTEN", "127.0.0.1:23791")
        .env("NODESTORE_DATA_DIR", dir.join("data"))
        .env("NODESTORE_MEMBER_ID", "9")
        .env("NODESTORE_INITIAL_CLUSTER", "1=https://10.0.0.1:2380,2=https://10.0.0.2:2380")
        .output()?;
    let _ = fs::remove_dir_all(&dir);
    anyhow::ensure!(!output.status.success(), "invalid cluster membership unexpectedly started");
    anyhow::ensure!(String::from_utf8_lossy(&output.stderr).contains("does not appear"), "invalid membership error did not explain the missing member");
    Ok(())
}

pub(super) async fn datastore_refuses_a_malformed_cluster_spec(
    _context: &E2eContext,
) -> Result<()> {
    let binary = nodestore_binary()?;
    let dir = std::env::temp_dir().join(format!("nodebootstrap-malformed-cluster-{}", std::process::id()));
    fs::create_dir_all(&dir)?;
    let output = Command::new(binary)
        .arg("nodestore")
        .env("NODESTORE_LISTEN", "127.0.0.1:23792")
        .env("NODESTORE_DATA_DIR", dir.join("data"))
        .env("NODESTORE_INITIAL_CLUSTER", "1=10.0.0.1:2380")
        .output()?;
    let _ = fs::remove_dir_all(&dir);
    anyhow::ensure!(!output.status.success(), "malformed cluster spec unexpectedly started");
    anyhow::ensure!(String::from_utf8_lossy(&output.stderr).contains("must include a scheme"), "malformed cluster error did not explain the missing scheme");
    Ok(())
}
