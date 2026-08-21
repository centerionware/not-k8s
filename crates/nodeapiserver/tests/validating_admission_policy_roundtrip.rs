//! A real, live round trip proving `admissionregistration.k8s.io/v1`'s
//! `ValidatingAdmissionPolicy` actually works through the generic REST
//! machinery, against a real `nodestore` — same "verified against real
//! infrastructure, not assumed" standard `tests/apiservice_roundtrip.rs`
//! already holds itself to, and the same real question that test's own
//! doc comment answered for `APIService`: `admissionregistration.k8s.io`
//! is a real `k8s.io/api` staging package (unlike `kube-aggregator`,
//! which needed its own special vendoring fix), so both its OpenAPI
//! schema *and* its proto were already vendored by the ordinary
//! `vendor/refresh.sh` glob — this test is what actually confirms that
//! translates into a real working resource, rather than assuming it
//! does from the vendoring alone (`admission::match_conditions`'s own
//! doc comment already named this as a real, unverified-live
//! assumption; this test closes that gap).

use nodeapiserver::config::Config;
use nodeapiserver::server::rest;
use nodeapiserver::storage::client::StorageClient;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

/// Mirrors `tests/apiservice_roundtrip.rs`'s own `find_nodestore_binary`/
/// `spawn_nodestore` exactly — see that file's doc comments for why each
/// test file owns its own copy of this setup rather than sharing one.
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
async fn validating_admission_policy_is_a_real_working_resource_through_the_generic_rest_verbs() {
    let Some(nodestore_bin) = find_nodestore_binary() else {
        eprintln!("SKIPPED: no nodestore binary available and building one on demand failed");
        return;
    };
    let (mut child, _data_dir, mut storage) = spawn_nodestore(&nodestore_bin, 23812).await;

    // A real-shaped ValidatingAdmissionPolicy -- the same kind of
    // "deployments must declare a real replica ceiling" example real
    // upstream's own documentation uses, not a synthetic minimal
    // fixture.
    let policy = json!({
        "apiVersion": "admissionregistration.k8s.io/v1",
        "kind": "ValidatingAdmissionPolicy",
        "metadata": {"name": "demo-replica-limit"},
        "spec": {
            "failurePolicy": "Fail",
            "matchConstraints": {
                "resourceRules": [{
                    "apiGroups": ["apps"],
                    "apiVersions": ["v1"],
                    "operations": ["CREATE", "UPDATE"],
                    "resources": ["deployments"],
                }],
            },
            "validations": [{
                "expression": "object.spec.replicas <= 5",
                "message": "replicas must not exceed 5",
            }],
        },
    });

    let created = match rest::create(&mut storage, "admissionregistration.k8s.io", "v1", "validatingadmissionpolicies", None, &policy).await.expect("rest::create must not itself error") {
        rest::CreateOutcome::Created(object) => object,
        other => panic!("expected Created, got {other:?}"),
    };
    assert_eq!(created["spec"]["failurePolicy"], "Fail");
    assert_eq!(created["spec"]["validations"][0]["expression"], "object.spec.replicas <= 5");

    let read_back = match rest::get(&mut storage, None, "admissionregistration.k8s.io", "v1", "validatingadmissionpolicies", None, "demo-replica-limit").await.expect("rest::get must not error") {
        rest::GetOutcome::Found(object) => object,
        other => panic!("expected Found, got {other:?}"),
    };
    assert_eq!(read_back["spec"]["matchConstraints"]["resourceRules"][0]["resources"][0], "deployments");

    let listed = match rest::list(&mut storage, None, "admissionregistration.k8s.io", "v1", "validatingadmissionpolicies", None, "", "", 0, "").await.expect("rest::list must not error") {
        rest::ListOutcome::Found(list) => list,
        other => panic!("expected Found, got {other:?}"),
    };
    assert_eq!(listed["kind"], "ValidatingAdmissionPolicyList");
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);

    // A real update (cluster-scoped, so namespace: None -- same
    // convention every other resource in this crate uses).
    let mut replacement = read_back.clone();
    replacement["spec"]["failurePolicy"] = json!("Ignore");
    let updated = match rest::update(&mut storage, "admissionregistration.k8s.io", "v1", "validatingadmissionpolicies", None, "demo-replica-limit", &replacement).await.expect("rest::update must not itself error") {
        rest::UpdateOutcome::Updated(object) => object,
        other => panic!("expected Updated, got {other:?}"),
    };
    assert_eq!(updated["spec"]["failurePolicy"], "Ignore");

    let deleted = match rest::delete(&mut storage, "admissionregistration.k8s.io", "v1", "validatingadmissionpolicies", None, "demo-replica-limit").await.expect("rest::delete must not error") {
        rest::DeleteOutcome::Deleted(object) => object,
        other => panic!("expected Deleted, got {other:?}"),
    };
    assert_eq!(deleted["metadata"]["name"], "demo-replica-limit");
    let gone = rest::get(&mut storage, None, "admissionregistration.k8s.io", "v1", "validatingadmissionpolicies", None, "demo-replica-limit").await.expect("rest::get must not error");
    assert_eq!(gone, rest::GetOutcome::ObjectNotFound);

    let _ = child.kill().await;
    let _ = child.wait().await;
}
