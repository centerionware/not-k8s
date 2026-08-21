//! A real, live encryption-at-rest round trip against a real running
//! `nodestore` — the one piece of Group C's encryption wiring nothing
//! else in this crate could verify: unit tests can (and do) prove the
//! transform primitives and config parsing are individually correct,
//! but only a genuine `create` -> raw-bytes-are-ciphertext -> `get`
//! round trip against a real datastore proves the *wiring itself*
//! actually protects data at rest, the same "verified against real
//! infrastructure, not assumed" standard the rest of this project holds
//! itself to (`CLAUDE.md`'s own stated practice).
//!
//! Skips itself (prints why, doesn't fail) when no `nodestore` binary is
//! available to spawn — same posture
//! `deploy/lib/test/cases/datastore.sh`'s own `_nodestore_binary`/
//! `skip_test` takes at the bash-e2e layer, for the same reason: this is
//! valuable exactly when a `nodestore` binary happens to be built
//! alongside this crate (e.g. `quick-check.yml -f
//! components=nodestore,nodeapiserver`), and must never break a run
//! where it isn't (most `cargo test -p nodeapiserver` invocations,
//! including this crate's own default CI dispatch).

use nodeapiserver::config::Config;
use nodeapiserver::server::rest;
use nodeapiserver::storage::client::StorageClient;
use nodeapiserver::storage::pb::etcdserverpb::RangeRequest;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

/// Mirrors `deploy/lib/test/cases/datastore.sh`'s own `_nodestore_binary`
/// candidate list, from a Rust integration test's own vantage point
/// (`CARGO_MANIFEST_DIR` is `crates/nodeapiserver`, two levels below the
/// workspace root that `target/` and `bin/` both hang off of). If none of
/// those already exist, builds one on demand (`cargo build -p nodestore`)
/// rather than giving up — real, found behavior: `cargo test -p
/// nodestore` alone does not reliably leave a plain `target/debug/
/// nodestore` executable the way `cargo test -p nodeapiserver` does for
/// its own bin (confirmed directly against a real CI run, not assumed),
/// so a caller that only ran `cargo test -p nodestore,nodeapiserver`
/// genuinely has no binary sitting around yet even though the crate
/// compiled. This keeps the test self-sufficient regardless of exactly
/// which cargo invocation ran before it.
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

/// A fixed, non-secret 32-byte test key — this is a throwaway scratch
/// datastore torn down at the end of the test, not real key material for
/// anything real upstream or this build's own docs would call a
/// production secret.
const TEST_AES_KEY_B64: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

fn encryption_config_yaml() -> String {
    format!(
        r#"
apiVersion: apiserver.config.k8s.io/v1
kind: EncryptionConfiguration
resources:
  - resources:
      - secrets
    providers:
      - aesgcm:
          keys:
            - name: test-key
              secret: {TEST_AES_KEY_B64}
      - identity: {{}}
"#
    )
}

#[tokio::test]
async fn secrets_are_genuinely_encrypted_at_rest_and_decrypt_back_correctly() {
    let Some(nodestore_bin) = find_nodestore_binary() else {
        eprintln!("SKIPPED: no nodestore binary available and building one on demand failed (no crates/nodestore on this ref, or `cargo build -p nodestore` itself failed -- see stderr above)");
        return;
    };

    let data_dir = tempfile::tempdir().expect("creating a scratch nodestore data dir");
    // A high, unusual port -- avoids colliding with a real nodestore
    // instance that might already be running on this same host (e.g.
    // this repo's own dev deployment on the standard 2379).
    let port = 23799;
    let listen = format!("127.0.0.1:{port}");

    let mut child = tokio::process::Command::new(&nodestore_bin)
        .env("NODESTORE_LISTEN", &listen)
        .env("NODESTORE_DATA_DIR", data_dir.path())
        .env("RUST_LOG", "warn")
        .kill_on_drop(true)
        .spawn()
        .expect("spawning the real nodestore binary");

    // The self-generated single-member client PKI -- exactly where
    // `deploy/lib/test/cases/datastore.sh`'s own `_nodestore_tls_flags`
    // points, and for the same reason (no plaintext mode, no explicit
    // cert config given, so nodestore generates its own).
    let pki_dir = data_dir.path().join("pki/client");
    let cert = pki_dir.join("client.crt");
    let key = pki_dir.join("client.key");
    let ca = pki_dir.join("ca.crt");

    let mut cfg = Config::default();
    cfg.nodestore_endpoint = format!("https://{listen}");
    cfg.nodestore_cert_file = Some(cert.clone());
    cfg.nodestore_key_file = Some(key.clone());
    cfg.nodestore_ca_file = Some(ca.clone());

    let encryption_config_path = data_dir.path().join("encryption-config.yaml");
    std::fs::write(&encryption_config_path, encryption_config_yaml()).expect("writing the scratch EncryptionConfiguration");

    // Wait for nodestore to be up and its self-generated PKI to exist,
    // rather than a fixed sleep -- a fixed sleep is either flaky or slow
    // on a loaded CI runner, same reasoning the bash harness's own
    // `_nodestore_start` already uses.
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
    let mut storage = storage.expect("nodestore never became reachable within 20s");

    let encryption_yaml = std::fs::read_to_string(&encryption_config_path).unwrap();
    let encryption_config = nodeapiserver::storage::encryption_config::parse(&encryption_yaml).expect("parsing the scratch EncryptionConfiguration");
    let mut storage_with_encryption = storage.clone().with_encryption(Some(encryption_config));

    // A Secret is exactly the resource a real cluster's own
    // EncryptionConfiguration almost always names first -- this is the
    // realistic case, not an arbitrary pick.
    let body = json!({
        "metadata": {"name": "test-secret", "namespace": "default"},
        "data": {"password": "c2VjcmV0"},
    });
    let created = match rest::create(&mut storage_with_encryption, "", "v1", "secrets", Some("default"), &body).await.expect("rest::create must not itself error") {
        rest::CreateOutcome::Created(object) => object,
        other => panic!("expected Created, got {other:?}"),
    };
    assert_eq!(created["metadata"]["name"], "test-secret");

    // The real proof: read the raw bytes nodestore is actually holding
    // and confirm they're genuinely ciphertext, not the plaintext
    // protobuf envelope `decode_stored_object` itself would recognize.
    let key_bytes = nodeapiserver::storage::keys::object_key("", "secrets", Some("default"), "test-secret").into_bytes();
    let raw = storage.range(RangeRequest { key: key_bytes, ..Default::default() }).await.expect("raw Range must succeed").kvs;
    assert_eq!(raw.len(), 1, "the object must exist at its real etcd key");
    let raw_value = &raw[0].value;
    assert!(
        raw_value.starts_with(nodeapiserver::storage::encryption::AES_GCM_PREFIX_V1.as_bytes()),
        "stored bytes must start with the real AES-GCM envelope prefix, got {:?}",
        String::from_utf8_lossy(&raw_value[..raw_value.len().min(40)])
    );
    // Mutually exclusive with the assertion above by construction (the
    // two prefixes diverge at their 4th byte: `:` vs `\0`), spelled out
    // anyway as its own real check — this is the one line that would
    // catch "wiring silently no-oped and wrote the plaintext envelope
    // instead," the exact failure mode this whole test exists to rule
    // out.
    assert!(!raw_value.starts_with(&nodeapiserver::codec::protobuf::MAGIC), "stored bytes must NOT be the plaintext k8s\\0 envelope -- encryption did not actually apply");

    // And the read path decrypts it back correctly.
    let read_back = match rest::get(&mut storage_with_encryption, None, "", "v1", "secrets", Some("default"), "test-secret").await.expect("rest::get must not error") {
        rest::GetOutcome::Found(object) => object,
        other => panic!("expected Found, got {other:?}"),
    };
    assert_eq!(read_back["data"]["password"], "c2VjcmV0");
    assert_eq!(read_back["metadata"]["name"], "test-secret");

    let _ = child.kill().await;
    let _ = child.wait().await;
}
