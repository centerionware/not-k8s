//! Shared fixtures for the CRD integration tests.

use nodeapiserver::config::Config;
use nodeapiserver::storage::client::StorageClient;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Mirrors `tests/encryption_roundtrip.rs`'s own `find_nodestore_binary`
/// exactly — see that file's doc comment for why building on demand
/// (rather than only checking for an already-built binary) matters.
pub fn find_nodestore_binary() -> Option<PathBuf> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .parent()?
        .to_path_buf();
    let candidates = [
        "bin/nodestore",
        "target/release/nodestore",
        "target/debug/nodestore",
    ];
    for candidate in candidates {
        let path = repo_root.join(candidate);
        if path.is_file() {
            return Some(path);
        }
    }

    if !repo_root.join("crates/nodestore").is_dir() {
        return None;
    }
    eprintln!(
        "no nodestore binary found at any of {candidates:?} -- building one now (cargo build -p nodestore)"
    );
    let status = std::process::Command::new("cargo")
        .args(["build", "-p", "nodestore"])
        .current_dir(&repo_root)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let built = repo_root.join("target/debug/nodestore");
    built.is_file().then_some(built)
}

/// Spawns a real, throwaway `nodestore` on `port` and returns a connected
/// [`StorageClient`] once it is reachable. Each test uses a distinct port so
/// Cargo's default parallel test execution cannot collide the processes.
pub async fn spawn_nodestore(
    nodestore_bin: &Path,
    port: u16,
) -> (tokio::process::Child, tempfile::TempDir, StorageClient) {
    let data_dir = tempfile::tempdir().expect("creating a scratch nodestore data dir");
    let listen = format!("127.0.0.1:{port}");

    let mut child = tokio::process::Command::new(nodestore_bin)
        .env("NODESTORE_LISTEN", &listen)
        .env("NODESTORE_DATA_DIR", data_dir.path())
        .env("RUST_LOG", "warn")
        .kill_on_drop(true)
        .spawn()
        .expect("spawning the real nodestore binary");

    let pki_dir = data_dir.path().join("pki/client");
    let cert = pki_dir.join("client.crt");
    let key = pki_dir.join("client.key");
    let ca = pki_dir.join("ca.crt");

    let mut cfg = Config::default();
    cfg.nodestore_endpoint = format!("https://{listen}");
    cfg.nodestore_cert_file = Some(cert.clone());
    cfg.nodestore_key_file = Some(key.clone());
    cfg.nodestore_ca_file = Some(ca.clone());

    let mut storage = None;
    for _ in 0..100 {
        if let Some(status) = child
            .try_wait()
            .expect("checking whether nodestore is still running")
        {
            panic!("nodestore exited during startup with {status:?}");
        }
        if cert.is_file() && key.is_file() && ca.is_file() {
            if let Ok(client) = StorageClient::connect(&cfg).await {
                storage = Some(client);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let storage = storage.expect("nodestore never became reachable within 20s");
    (child, data_dir, storage)
}
