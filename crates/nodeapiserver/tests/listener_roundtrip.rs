//! A small throwaway-rig test for the real nodeapiserver process.
//!
//! The other nodeapiserver integration tests exercise the REST layer directly
//! against a real nodestore. This test crosses the process and TLS boundary as
//! well: it starts both binaries, waits for live `/readyz`, and drives the
//! listener with a real HTTP client through discovery and generic CRUD. This
//! is intentionally cheap enough to run with the normal nodeapiserver test
//! target; the full cluster e2e remains the final nodeapiserver-to-main gate.

use rcgen::generate_simple_self_signed;
use reqwest::Client;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, Command};

fn find_binary(name: &str) -> Option<PathBuf> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .parent()?
        .to_path_buf();
    let candidates = [
        repo_root.join(format!("target/debug/{name}")),
        repo_root.join(format!("target/release/{name}")),
        repo_root.join(format!("bin/{name}")),
    ];
    candidates.into_iter().find(|path| path.is_file())
}

async fn spawn_nodestore(nodestore: &Path, port: u16) -> (Child, tempfile::TempDir, Vec<PathBuf>) {
    let data_dir = tempfile::tempdir().expect("creating a scratch nodestore data dir");
    let listen = format!("127.0.0.1:{port}");
    let mut child = Command::new(nodestore)
        .env("NODESTORE_LISTEN", &listen)
        .env("NODESTORE_DATA_DIR", data_dir.path())
        .env("RUST_LOG", "warn")
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the real nodestore binary");

    let pki_dir = data_dir.path().join("pki/client");
    let files = vec![
        pki_dir.join("client.crt"),
        pki_dir.join("client.key"),
        pki_dir.join("ca.crt"),
    ];
    for _ in 0..100 {
        if let Some(status) = child.try_wait().expect("checking nodestore startup") {
            panic!("nodestore exited during startup with {status:?}");
        }
        if files.iter().all(|path| path.is_file()) {
            return (child, data_dir, files);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("nodestore did not create client TLS material within 10s");
}

async fn wait_ready(client: &Client, endpoint: &str) {
    for _ in 0..100 {
        if let Ok(response) = client.get(format!("{endpoint}/readyz")).send().await {
            if response.status().is_success() {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("nodeapiserver did not become ready within 10s");
}

#[tokio::test]
async fn listener_serves_a_real_discovery_and_crud_round_trip() {
    let Some(nodestore) = find_binary("nodestore") else {
        eprintln!("SKIPPED: nodestore binary is not available for the throwaway rig");
        return;
    };
    let Some(nodeapiserver) = std::env::var_os("CARGO_BIN_EXE_nodeapiserver")
        .map(PathBuf::from)
        .or_else(|| find_binary("nodeapiserver"))
    else {
        eprintln!("SKIPPED: nodeapiserver binary is not available for the throwaway rig");
        return;
    };

    let nodestore_port = 23901 + (std::process::id() % 100) as u16;
    let listener_port = 24901 + (std::process::id() % 100) as u16;
    let (mut nodestore_child, _data_dir, nodestore_tls) =
        spawn_nodestore(&nodestore, nodestore_port).await;

    let tls_dir = tempfile::tempdir().expect("creating listener TLS directory");
    let certificate = generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generating listener certificate");
    let cert_file = tls_dir.path().join("server.crt");
    let key_file = tls_dir.path().join("server.key");
    std::fs::write(&cert_file, certificate.cert.pem()).expect("writing listener certificate");
    std::fs::write(&key_file, certificate.key_pair.serialize_pem()).expect("writing listener key");

    let endpoint = format!("https://127.0.0.1:{listener_port}");
    let mut nodeapiserver_child = Command::new(nodeapiserver)
        .env(
            "NODEAPISERVER_BIND_ADDR",
            format!("127.0.0.1:{listener_port}"),
        )
        .env("NODEAPISERVER_TLS_CERT_FILE", &cert_file)
        .env("NODEAPISERVER_TLS_KEY_FILE", &key_file)
        .env(
            "NODEAPISERVER_NODESTORE_ENDPOINT",
            format!("https://127.0.0.1:{nodestore_port}"),
        )
        .env("NODEAPISERVER_NODESTORE_CERT_FILE", &nodestore_tls[0])
        .env("NODEAPISERVER_NODESTORE_KEY_FILE", &nodestore_tls[1])
        .env("NODEAPISERVER_NODESTORE_CA_FILE", &nodestore_tls[2])
        .env("RUST_LOG", "warn")
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the real nodeapiserver binary");

    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("building the throwaway-rig HTTP client");
    wait_ready(&client, &endpoint).await;

    let version = client
        .get(format!("{endpoint}/version"))
        .send()
        .await
        .expect("requesting /version")
        .error_for_status()
        .expect("/version should succeed")
        .json::<serde_json::Value>()
        .await
        .expect("decoding /version");
    assert_eq!(version["major"], "1");
    assert!(version["gitVersion"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));

    let discovery = client
        .get(format!("{endpoint}/api/v1"))
        .send()
        .await
        .expect("requesting core discovery")
        .error_for_status()
        .expect("core discovery should succeed")
        .json::<serde_json::Value>()
        .await
        .expect("decoding core discovery");
    assert_eq!(discovery["kind"], "APIResourceList");
    assert!(discovery["resources"]
        .as_array()
        .is_some_and(|resources| resources
            .iter()
            .any(|resource| resource["name"] == "namespaces")));

    let name = format!("listener-rig-{}", std::process::id());
    let created = client
        .post(format!("{endpoint}/api/v1/namespaces"))
        .header("content-type", "application/json")
        .json(&json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {"name": name},
        }))
        .send()
        .await
        .expect("creating a Namespace through the listener");
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);

    let fetched = client
        .get(format!("{endpoint}/api/v1/namespaces/{name}"))
        .send()
        .await
        .expect("getting the Namespace through the listener")
        .error_for_status()
        .expect("Namespace GET should succeed")
        .json::<serde_json::Value>()
        .await
        .expect("decoding Namespace GET");
    assert_eq!(fetched["metadata"]["name"], name);

    let deleted = client
        .delete(format!("{endpoint}/api/v1/namespaces/{name}"))
        .send()
        .await
        .expect("deleting the Namespace through the listener");
    assert_eq!(deleted.status(), reqwest::StatusCode::OK);

    let _ = nodeapiserver_child.kill().await;
    let _ = nodeapiserver_child.wait().await;
    let _ = nodestore_child.kill().await;
    let _ = nodestore_child.wait().await;
}
