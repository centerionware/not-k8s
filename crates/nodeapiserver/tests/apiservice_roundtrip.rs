//! A real, live round trip proving `apiregistration.k8s.io/v1`'s
//! `APIService` — Group L's own backing resource — actually works
//! through the generic REST machinery, against a real `nodestore`. Same
//! "verified against real infrastructure, not assumed" standard
//! `tests/encryption_roundtrip.rs`/`tests/crd_roundtrip.rs` already hold
//! themselves to.
//!
//! This is the exact same class of gap Group K's own CRD work found
//! live more than once this session: `resolve_kind` (driven by
//! `vendor/openapi-spec/v3`, no group allowlist) already knew about
//! `APIService`, but `schema_for_gvk` (driven by `vendor/protos`, which
//! `vendor/refresh.sh` only fetched from `k8s.io/api*` — a glob that
//! misses `k8s.io/kube-aggregator`, `APIService`'s real staging repo)
//! had no compiled schema for it at all, so every real verb silently
//! answered `UnknownResource` for a type that looked known. Fixed by
//! vendoring `k8s.io/kube-aggregator/pkg/apis/apiregistration/{v1,v1beta1}/
//! generated.proto` directly (not a full `refresh.sh` re-run, which
//! would have re-fetched the entire already-vendored tree against
//! whatever `release-1.34` currently points to — real, unrelated drift
//! this fix has no reason to risk) and widening `refresh.sh`'s own glob
//! so a future full re-vendor keeps this covered.

use nodeapiserver::config::Config;
use nodeapiserver::server::rest;
use nodeapiserver::storage::client::StorageClient;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

/// Mirrors `tests/crd_roundtrip.rs`'s own `find_nodestore_binary` /
/// `spawn_nodestore` exactly — see that file's doc comments for why
/// building on demand (rather than only checking for an already-built
/// binary) matters, and why each test file owns its own copy of this
/// setup rather than sharing one across files.
fn find_nodestore_binary() -> Option<PathBuf> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent()?.parent()?.to_path_buf();
    let candidates = ["bin/nodestore", "target/release/nodestore", "target/debug/nodestore"];
    for candidate in candidates {
        let path = repo_root.join(candidate);
        if path.is_file() {
            return Some(path);
        }
    }
    if !repo_root.join("crates/nodestore").is_dir() {
        return None;
    }
    eprintln!("no nodestore binary found at any of {candidates:?} -- building one now (cargo build -p nodestore)");
    let status = std::process::Command::new("cargo").args(["build", "-p", "nodestore"]).current_dir(&repo_root).status().ok()?;
    if !status.success() {
        return None;
    }
    let built = repo_root.join("target/debug/nodestore");
    built.is_file().then_some(built)
}

async fn spawn_nodestore(nodestore_bin: &std::path::Path, port: u16) -> (tokio::process::Child, tempfile::TempDir, StorageClient) {
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
        if let Some(status) = child.try_wait().expect("checking whether nodestore is still running") {
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

#[tokio::test]
async fn api_service_is_a_real_working_resource_through_the_generic_rest_verbs() {
    let Some(nodestore_bin) = find_nodestore_binary() else {
        eprintln!("SKIPPED: no nodestore binary available and building one on demand failed");
        return;
    };
    let (mut child, _data_dir, mut storage) = spawn_nodestore(&nodestore_bin, 23807).await;

    // A real-shaped APIService -- the actual, common real-world case
    // (metrics-server) fronting a group-version with a backing Service,
    // not a synthetic minimal fixture.
    let api_service = json!({
        "apiVersion": "apiregistration.k8s.io/v1",
        "kind": "APIService",
        "metadata": {"name": "v1beta1.metrics.k8s.io"},
        "spec": {
            "group": "metrics.k8s.io",
            "version": "v1beta1",
            "service": {"namespace": "kube-system", "name": "metrics-server", "port": 443},
            "groupPriorityMinimum": 100,
            "versionPriority": 100,
            "caBundle": "",
        },
    });

    let created = match rest::create(&mut storage, "apiregistration.k8s.io", "v1", "apiservices", None, &api_service).await.expect("rest::create must not itself error") {
        rest::CreateOutcome::Created(object) => object,
        other => panic!("expected Created, got {other:?}"),
    };
    assert_eq!(created["spec"]["group"], "metrics.k8s.io");
    assert_eq!(created["spec"]["service"]["name"], "metrics-server");

    let read_back = match rest::get(&mut storage, None, "apiregistration.k8s.io", "v1", "apiservices", None, "v1beta1.metrics.k8s.io").await.expect("rest::get must not error") {
        rest::GetOutcome::Found(object) => object,
        other => panic!("expected Found, got {other:?}"),
    };
    assert_eq!(read_back["spec"]["groupPriorityMinimum"], 100);

    let listed = match rest::list(&mut storage, None, "apiregistration.k8s.io", "v1", "apiservices", None, "", "", 0, "").await.expect("rest::list must not error") {
        rest::ListOutcome::Found(list) => list,
        other => panic!("expected Found, got {other:?}"),
    };
    assert_eq!(listed["kind"], "APIServiceList");
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);

    // A real update (cluster-scoped, so namespace: None -- same
    // convention every other resource in this crate uses).
    let mut replacement = read_back.clone();
    replacement["spec"]["versionPriority"] = json!(200);
    let updated = match rest::update(&mut storage, "apiregistration.k8s.io", "v1", "apiservices", None, "v1beta1.metrics.k8s.io", &replacement).await.expect("rest::update must not itself error") {
        rest::UpdateOutcome::Updated(object) => object,
        other => panic!("expected Updated, got {other:?}"),
    };
    assert_eq!(updated["spec"]["versionPriority"], 200);

    let deleted = match rest::delete(&mut storage, "apiregistration.k8s.io", "v1", "apiservices", None, "v1beta1.metrics.k8s.io").await.expect("rest::delete must not error") {
        rest::DeleteOutcome::Deleted(object) => object,
        other => panic!("expected Deleted, got {other:?}"),
    };
    assert_eq!(deleted["metadata"]["name"], "v1beta1.metrics.k8s.io");
    let gone = rest::get(&mut storage, None, "apiregistration.k8s.io", "v1", "apiservices", None, "v1beta1.metrics.k8s.io").await.expect("rest::get must not error");
    assert_eq!(gone, rest::GetOutcome::ObjectNotFound);

    let _ = child.kill().await;
    let _ = child.wait().await;
}
