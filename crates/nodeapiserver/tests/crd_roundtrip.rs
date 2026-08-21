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

/// Spawns a real, throwaway `nodestore` on `port` and returns a connected
/// [`StorageClient`] once it's reachable — the shared setup both test
/// functions in this file use, each on its own port so `cargo test`'s
/// default parallel-test-execution doesn't collide the two.
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

    // A different high port than `tests/encryption_roundtrip.rs` uses, so
    // the two integration test binaries can run concurrently (`cargo
    // test` runs separate test binaries in parallel by default) without
    // colliding on the same listen address.
    let (mut child, _data_dir, mut storage) = spawn_nodestore(&nodestore_bin, 23800).await;

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

/// The `UPDATE`/`PATCH` half of Group K: `rest::update` and
/// `rest::patch_prepare`/`patch_persist` both now resolve a CRD-defined
/// resource the same way `create`/`get`/`list`/`delete` already did.
/// `JSON Patch`/`Merge Patch` need no schema and work identically to a
/// built-in; `strategic-merge-patch` is a real, named gap (`apply_patch`'s
/// own doc comment) — confirmed here as a clean `Invalid`, not a panic
/// or a silently-wrong merge.
#[tokio::test]
async fn update_and_patch_work_against_a_crd_defined_resource() {
    let Some(nodestore_bin) = find_nodestore_binary() else {
        eprintln!("SKIPPED: no nodestore binary available and building one on demand failed");
        return;
    };
    let (mut child, _data_dir, mut storage) = spawn_nodestore(&nodestore_bin, 23802).await;

    rest::create(&mut storage, "apiextensions.k8s.io", "v1", "customresourcedefinitions", None, &a_crd()).await.expect("rest::create(CRD) must not itself error");
    let widget = json!({
        "apiVersion": "example.com/v1",
        "kind": "Widget",
        "metadata": {"name": "editable-widget", "namespace": "default"},
        "spec": {"color": "red"},
    });
    let created = match rest::create(&mut storage, "example.com", "v1", "widgets", Some("default"), &widget).await.expect("rest::create must not itself error") {
        rest::CreateOutcome::Created(object) => object,
        other => panic!("expected Created, got {other:?}"),
    };

    // 1. A plain PUT (rest::update) -- real optimistic concurrency
    // (the submitted resourceVersion must match), schema-driven
    // defaulting still applies since the client's own replacement body
    // doesn't set `spec.size` either.
    let mut replacement = created.clone();
    replacement["spec"]["color"] = json!("blue");
    replacement["spec"].as_object_mut().unwrap().remove("size");
    let updated = match rest::update(&mut storage, "example.com", "v1", "widgets", Some("default"), "editable-widget", &replacement).await.expect("rest::update must not itself error") {
        rest::UpdateOutcome::Updated(object) => object,
        other => panic!("expected Updated, got {other:?}"),
    };
    assert_eq!(updated["spec"]["color"], "blue");
    assert_eq!(updated["spec"]["size"], "small", "the CRD schema's own default must still apply on UPDATE");

    // 2. A JSON Patch (RFC 6902) -- needs no schema at all.
    let json_patch = json!([{"op": "replace", "path": "/spec/color", "value": "green"}]);
    let (candidate, context) = match rest::patch_prepare(&mut storage, "example.com", "v1", "widgets", Some("default"), "editable-widget", rest::PatchKind::Json, &json_patch)
        .await
        .expect("rest::patch_prepare must not itself error")
    {
        rest::PatchPrepareOutcome::Ready(candidate, context) => (candidate, context),
        other => panic!("expected Ready, got {other:?}"),
    };
    let patched = match rest::patch_persist(&mut storage, "example.com", "v1", "widgets", Some("default"), "editable-widget", context, candidate).await.expect("rest::patch_persist must not itself error") {
        rest::UpdateOutcome::Updated(object) => object,
        other => panic!("expected Updated, got {other:?}"),
    };
    assert_eq!(patched["spec"]["color"], "green");

    // 3. A Merge Patch (RFC 7386) -- also needs no schema.
    let merge_patch = json!({"spec": {"color": "yellow"}});
    let (candidate, context) = match rest::patch_prepare(&mut storage, "example.com", "v1", "widgets", Some("default"), "editable-widget", rest::PatchKind::Merge, &merge_patch)
        .await
        .expect("rest::patch_prepare must not itself error")
    {
        rest::PatchPrepareOutcome::Ready(candidate, context) => (candidate, context),
        other => panic!("expected Ready, got {other:?}"),
    };
    let patched = match rest::patch_persist(&mut storage, "example.com", "v1", "widgets", Some("default"), "editable-widget", context, candidate).await.expect("rest::patch_persist must not itself error") {
        rest::UpdateOutcome::Updated(object) => object,
        other => panic!("expected Updated, got {other:?}"),
    };
    assert_eq!(patched["spec"]["color"], "yellow");

    // 4. strategic-merge-patch is a real, named gap for a CRD -- a clean
    // Invalid, not a panic and not a silently-wrong merge.
    let strategic_patch = json!({"spec": {"color": "purple"}});
    match rest::patch_prepare(&mut storage, "example.com", "v1", "widgets", Some("default"), "editable-widget", rest::PatchKind::StrategicMerge, &strategic_patch)
        .await
        .expect("rest::patch_prepare must not itself error")
    {
        rest::PatchPrepareOutcome::Invalid(msgs) => {
            assert!(msgs.iter().any(|m| m.contains("strategic-merge-patch")), "expected a clear strategic-merge-patch error, got {msgs:?}");
        }
        other => panic!("expected Invalid, got {other:?}"),
    }

    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// The `WATCH` half of Group K: `server::listener`'s own dispatch, on a
/// cache miss for a resource its static table doesn't know at all,
/// resolves it dynamically (`rest::resolve_dynamic_kind`) and lazily
/// spawns a cache for it right then (`CacheRegistry::spawn`, callable at
/// any time — see its own doc comment). This test exercises exactly that
/// sequence directly against real infrastructure (not the full HTTP/TLS
/// listener, which would need real client-cert PKI just to reach this
/// same code path) and confirms the resulting cache is a genuine live
/// watch, not just a snapshot: an object created *after* the cache
/// already exists shows up on the live channel, not only in a replay.
#[tokio::test]
async fn watching_a_crd_defined_resource_lazily_spawns_a_cache_and_streams_real_events() {
    let Some(nodestore_bin) = find_nodestore_binary() else {
        eprintln!("SKIPPED: no nodestore binary available and building one on demand failed");
        return;
    };
    let (mut child, _data_dir, mut storage) = spawn_nodestore(&nodestore_bin, 23801).await;

    rest::create(&mut storage, "apiextensions.k8s.io", "v1", "customresourcedefinitions", None, &a_crd()).await.expect("rest::create(CRD) must not itself error");

    let cache_registry = nodeapiserver::cacher::CacheRegistry::new();
    assert!(cache_registry.get("example.com", "v1", "widgets").is_none(), "nothing should be registered before the first watch ever asks for it");

    let kind = rest::resolve_dynamic_kind(&mut storage, "example.com", "v1", "widgets")
        .await
        .expect("resolve_dynamic_kind must not itself error")
        .expect("the CRD's own resource must resolve dynamically now that the CRD exists");
    assert_eq!(kind, "Widget");

    let cache = cache_registry.spawn(storage.clone(), "example.com", "v1", "widgets");
    // has_synced() is the real signal a reflector's first LIST completed
    // -- not a fixed sleep, same reasoning every other live test in this
    // crate already follows.
    let mut synced = false;
    for _ in 0..100 {
        if cache.has_synced() {
            synced = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(synced, "the lazily-spawned reflector must finish its first LIST within 5s");

    let (replay, mut rx) = cache.watch_from(0).expect("watch_from(0) must succeed against a freshly synced cache");
    assert!(replay.is_empty(), "no widgets exist yet, so the replay must be empty");

    // Create one for real -- through the exact same generic rest::create
    // the live HTTP handler itself calls -- and confirm the reflector's
    // own live watch stream (not the replay) picks it up.
    let widget = json!({
        "apiVersion": "example.com/v1",
        "kind": "Widget",
        "metadata": {"name": "streamed-widget", "namespace": "default"},
        "spec": {},
    });
    rest::create(&mut storage, "example.com", "v1", "widgets", Some("default"), &widget).await.expect("rest::create(Widget) must not itself error");

    let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("a watch event for the newly created widget must arrive within 5s")
        .expect("the watch channel must not close");
    assert_eq!(event.kind, nodeapiserver::cacher::EventKind::Added);
    assert!(
        event.key.ends_with(b"streamed-widget"),
        "the event's own key must name the widget just created, got {:?}",
        String::from_utf8_lossy(&event.key)
    );

    let _ = child.kill().await;
    let _ = child.wait().await;
}
