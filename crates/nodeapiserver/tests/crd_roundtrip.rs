//! A real, live CRD round trip against a real running `nodestore` —
//! proves Group K's dynamic resource registry actually routes a
//! CRD-defined resource through `server::rest`'s generic verbs, the same
//! "verified against real infrastructure, not assumed" standard
//! `tests/encryption_roundtrip.rs` already holds itself to (and this file
//! mirrors that one's own nodestore-spawn machinery — see its doc
//! comment for why a nodestore binary is built on demand rather than
//! this test failing outright when one isn't already sitting around).
//!
//! Exercises the full lifecycle a real `kubectl apply -f
//! <a CRD>.yaml && kubectl apply -f <a custom resource>.yaml` sequence
//! would: `CREATE` a `CustomResourceDefinition` (itself a real, compiled
//! built-in type — Group A's codegen already covers
//! `apiextensions.k8s.io/v1`) and confirm its own `status.conditions`
//! come back `Established`/`NamesAccepted` (`apiextensions::conditions`,
//! this build's synchronous stand-in for real upstream's async
//! establishing controller); then `CREATE`/`GET`/`LIST`/`DELETE` a
//! custom-resource instance of that CRD's own Kind, resolved purely
//! through the dynamic registry (`apiextensions::registry`) since it has
//! no compiled proto schema at all, confirming schema-driven defaulting
//! (`apiextensions::schema_defaults`) actually filled in a field the
//! client never submitted.

use nodeapiserver::config::Config;
use nodeapiserver::server::rest;
use nodeapiserver::storage::client::StorageClient;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

/// Mirrors `tests/encryption_roundtrip.rs`'s own `find_nodestore_binary`
/// exactly — see that file's doc comment for why building on demand
/// (rather than only checking for an already-built binary) matters.
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

fn a_crd() -> serde_json::Value {
    json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": "widgets.example.com"},
        "spec": {
            "group": "example.com",
            "scope": "Namespaced",
            "names": {"plural": "widgets", "singular": "widget", "kind": "Widget", "listKind": "WidgetList"},
            "versions": [{
                "name": "v1",
                "served": true,
                "storage": true,
                "schema": {
                    "openAPIV3Schema": {
                        "type": "object",
                        "properties": {
                            "spec": {
                                "type": "object",
                                "properties": {
                                    "size": {"type": "string", "default": "small"},
                                    "color": {"type": "string"},
                                },
                            },
                        },
                    },
                },
            }],
        },
    })
}

#[tokio::test]
async fn a_crd_defined_resource_routes_through_the_generic_rest_verbs() {
    let Some(nodestore_bin) = find_nodestore_binary() else {
        eprintln!("SKIPPED: no nodestore binary available and building one on demand failed (no crates/nodestore on this ref, or `cargo build -p nodestore` itself failed -- see stderr above)");
        return;
    };

    let data_dir = tempfile::tempdir().expect("creating a scratch nodestore data dir");
    // A different high port than `tests/encryption_roundtrip.rs` uses, so
    // the two integration test binaries can run concurrently (`cargo
    // test` runs separate test binaries in parallel by default) without
    // colliding on the same listen address.
    let port = 23800;
    let listen = format!("127.0.0.1:{port}");

    let mut child = tokio::process::Command::new(&nodestore_bin)
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
    let mut storage = storage.expect("nodestore never became reachable within 20s");

    // 1. Create the CRD itself -- a real, compiled built-in type, and
    // confirm its server-computed status marks it usable immediately
    // (this build's synchronous stand-in for real upstream's async
    // establishing controller -- see `apiextensions::conditions`'s own
    // doc comment).
    let created_crd = match rest::create(&mut storage, "apiextensions.k8s.io", "v1", "customresourcedefinitions", None, &a_crd()).await.expect("rest::create(CRD) must not itself error") {
        rest::CreateOutcome::Created(object) => object,
        other => panic!("expected Created, got {other:?}"),
    };
    let conditions = created_crd["status"]["conditions"].as_array().cloned().unwrap_or_default();
    let established = conditions.iter().find(|c| c["type"] == "Established").expect("an Established condition must be present");
    assert_eq!(established["status"], "True", "a CRD with no naming rivals must come back Established immediately: {created_crd}");
    let names_accepted = conditions.iter().find(|c| c["type"] == "NamesAccepted").expect("a NamesAccepted condition must be present");
    assert_eq!(names_accepted["status"], "True");
    assert_eq!(created_crd["status"]["storedVersions"], json!(["v1"]));

    // 2. Create a custom-resource instance of the CRD's own Kind --
    // resolved purely dynamically (`example.com/v1/widgets` has no
    // compiled proto schema anywhere in this crate), and confirm
    // schema-driven defaulting actually filled in `spec.size` (the CRD's
    // schema names a default, the client never submitted one) while
    // leaving the client's own `spec.color` alone.
    let widget = json!({
        "apiVersion": "example.com/v1",
        "kind": "Widget",
        "metadata": {"name": "my-widget", "namespace": "default"},
        "spec": {"color": "red"},
    });
    let created_widget = match rest::create(&mut storage, "example.com", "v1", "widgets", Some("default"), &widget).await.expect("rest::create(Widget) must not itself error") {
        rest::CreateOutcome::Created(object) => object,
        other => panic!("expected Created, got {other:?}"),
    };
    assert_eq!(created_widget["spec"]["color"], "red", "the client's own submitted field must survive untouched");
    assert_eq!(created_widget["spec"]["size"], "small", "the CRD schema's own default must have been applied server-side");

    // 3. GET and LIST both find it, decoded correctly (the generic
    // decode path's own JSON fallback for a Kind with no compiled proto
    // schema -- `server::rest::decode_stored_object`'s own doc comment).
    let read_back = match rest::get(&mut storage, None, "example.com", "v1", "widgets", Some("default"), "my-widget").await.expect("rest::get must not error") {
        rest::GetOutcome::Found(object) => object,
        other => panic!("expected Found, got {other:?}"),
    };
    assert_eq!(read_back["spec"]["size"], "small");

    let listed = match rest::list(&mut storage, None, "example.com", "v1", "widgets", Some("default"), "", "", 0, "").await.expect("rest::list must not error") {
        rest::ListOutcome::Found(list) => list,
        other => panic!("expected Found, got {other:?}"),
    };
    assert_eq!(listed["kind"], "WidgetList");
    let items = listed["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 1, "the widget just created must be the only item listed: {listed}");

    // 4. A resource this CRD does NOT define is still a genuine
    // UnknownResource -- the dynamic registry doesn't turn into "anything
    // goes."
    let unknown = rest::get(&mut storage, None, "example.com", "v1", "gizmos", Some("default"), "whatever").await.expect("rest::get must not error");
    assert_eq!(unknown, rest::GetOutcome::UnknownResource);

    // 5. DELETE removes it, returning the object as it was.
    let deleted = match rest::delete(&mut storage, "example.com", "v1", "widgets", Some("default"), "my-widget").await.expect("rest::delete must not error") {
        rest::DeleteOutcome::Deleted(object) => object,
        other => panic!("expected Deleted, got {other:?}"),
    };
    assert_eq!(deleted["metadata"]["name"], "my-widget");
    let gone = rest::get(&mut storage, None, "example.com", "v1", "widgets", Some("default"), "my-widget").await.expect("rest::get must not error");
    assert_eq!(gone, rest::GetOutcome::ObjectNotFound);

    let _ = child.kill().await;
    let _ = child.wait().await;
}
