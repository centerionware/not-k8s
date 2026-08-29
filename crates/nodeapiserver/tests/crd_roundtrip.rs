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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

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
                "additionalPrinterColumns": [{"name": "Color", "type": "string", "jsonPath": ".spec.color"}],
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

fn conversion_crd(webhook_url: &str) -> serde_json::Value {
    json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": "convertedwidgets.example.com"},
        "spec": {
            "group": "example.com",
            "scope": "Namespaced",
            "names": {"plural": "convertedwidgets", "singular": "convertedwidget", "kind": "ConvertedWidget", "listKind": "ConvertedWidgetList"},
            "conversion": {
                "strategy": "Webhook",
                "webhook": {
                    "conversionReviewVersions": ["v1"],
                    "clientConfig": {"url": webhook_url}
                }
            },
            "versions": [
                {
                    "name": "v1",
                    "served": true,
                    "storage": true,
                    "schema": {"openAPIV3Schema": {"type": "object", "properties": {"spec": {"type": "object", "properties": {"value": {"type": "string"}, "convertedVersion": {"type": "string"}}}}}}
                },
                {
                    "name": "v1beta1",
                    "served": true,
                    "storage": false,
                    "schema": {"openAPIV3Schema": {"type": "object", "properties": {"spec": {"type": "object", "properties": {"value": {"type": "string"}, "convertedVersion": {"type": "string"}}}}}}
                }
            ]
        }
    })
}

fn storage_validation_crd(webhook_url: &str) -> serde_json::Value {
    json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": "strictconvertedwidgets.example.com"},
        "spec": {
            "group": "example.com",
            "scope": "Namespaced",
            "names": {"plural": "strictconvertedwidgets", "singular": "strictconvertedwidget", "kind": "StrictConvertedWidget", "listKind": "StrictConvertedWidgetList"},
            "conversion": {
                "strategy": "Webhook",
                "webhook": {
                    "conversionReviewVersions": ["v1"],
                    "clientConfig": {"url": webhook_url}
                }
            },
            "versions": [
                {
                    "name": "v1",
                    "served": true,
                    "storage": true,
                    "schema": {"openAPIV3Schema": {"type": "object", "properties": {"spec": {"type": "object", "required": ["storageOnly"], "properties": {"storageOnly": {"type": "string"}}}}}}
                },
                {
                    "name": "v1beta1",
                    "served": true,
                    "storage": false,
                    "schema": {"openAPIV3Schema": {"type": "object", "properties": {"spec": {"type": "object", "required": ["value"], "properties": {"value": {"type": "string"}}}}}}
                }
            ]
        }
    })
}

fn spawn_conversion_webhook(
    listener: TcpListener,
    expected_versions: Vec<&'static str>,
    invalid_storage_value: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        for expected_version in expected_versions {
            let (mut stream, _) = listener
                .accept()
                .await
                .expect("conversion webhook should accept a request");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            let body_start = loop {
                let read = stream
                    .read(&mut chunk)
                    .await
                    .expect("read conversion webhook request");
                assert!(read > 0, "conversion webhook request ended before its headers");
                request.extend_from_slice(&chunk[..read]);
                if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let content_length = String::from_utf8_lossy(&request[..body_start])
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value)
                })
                .and_then(|value| value.trim().parse::<usize>().ok())
                .expect("conversion webhook request should include Content-Length");
            while request.len() < body_start + content_length {
                let read = stream
                    .read(&mut chunk)
                    .await
                    .expect("read conversion webhook body");
                assert!(read > 0, "conversion webhook request ended before its body");
                request.extend_from_slice(&chunk[..read]);
            }
            let payload: serde_json::Value = serde_json::from_slice(&request[body_start..body_start + content_length])
                .expect("conversion webhook body should be JSON");
            assert_eq!(payload["kind"], "ConversionReview");
            assert_eq!(payload["apiVersion"], "apiextensions.k8s.io/v1");
            assert_eq!(payload["request"]["desiredAPIVersion"], format!("example.com/{expected_version}"));
            let uid = payload["request"]["uid"].as_str().expect("conversion request UID");
            let mut converted = payload["request"]["objects"][0].clone();
            converted["apiVersion"] = json!(format!("example.com/{expected_version}"));
            converted["spec"]["convertedVersion"] = json!(expected_version);
            if invalid_storage_value && expected_version == "v1" {
                converted["spec"]["storageOnly"] = json!(42);
            }
            let response = json!({
                "apiVersion": "apiextensions.k8s.io/v1",
                "kind": "ConversionReview",
                "response": {
                    "uid": uid,
                    "result": {"status": "Success"},
                    "convertedObjects": [converted]
                }
            });
            let body = serde_json::to_vec(&response).expect("encode conversion response");
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.expect("write conversion webhook headers");
            stream.write_all(&body).await.expect("write conversion webhook body");
        }
    })
}

/// A versioned CRD must store objects in its one nominated storage version,
/// while every request and response still uses the version named by the URL.
/// This drives the real ConversionReview HTTP contract through a local
/// webhook and a real nodestore, including a LIST response rather than only
/// checking the CREATE return value.
#[tokio::test]
async fn conversion_webhook_round_trips_cr_objects_between_served_versions() {
    let Some(nodestore_bin) = find_nodestore_binary() else {
        eprintln!("SKIPPED: no nodestore binary found and building one on demand failed");
        return;
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind conversion webhook listener");
    let webhook_url = format!("http://{}/convert", listener.local_addr().expect("conversion webhook listener address"));
    let webhook_task = spawn_conversion_webhook(listener, vec!["v1", "v1beta1", "v1beta1", "v1beta1", "v1beta1"], false);
    let (mut child, _data_dir, mut storage) = spawn_nodestore(&nodestore_bin, 23807).await;

    rest::create(
        &mut storage,
        "apiextensions.k8s.io",
        "v1",
        "customresourcedefinitions",
        None,
        &conversion_crd(&webhook_url),
    )
    .await
    .expect("rest::create(CRD) must not itself error");

    let resolved = rest::resolve_dynamic_resource(&mut storage, "example.com", "v1beta1", "convertedwidgets")
        .await
        .expect("resolve_dynamic_resource must not itself error")
        .expect("the converted CRD must resolve");
    let conversion_webhook = resolved.conversion_webhook.clone().expect("the converted CRD must retain its webhook configuration");
    let cache_registry = nodeapiserver::cacher::CacheRegistry::new();
    let cache = cache_registry.spawn(storage.clone(), "example.com", "v1beta1", "convertedwidgets");
    tokio::time::timeout(Duration::from_secs(5), cache.wait_until_synced())
        .await
        .expect("the converted CRD cache must synchronize");
    let (replay, mut watch_rx) = cache.watch_from(0).expect("the converted CRD watch must start");
    assert!(replay.is_empty());

    let object = json!({
        "apiVersion": "example.com/v1beta1",
        "kind": "ConvertedWidget",
        "metadata": {"name": "converted-widget", "namespace": "default"},
        "spec": {"value": "hello"},
    });
    let created = match rest::create(&mut storage, "example.com", "v1beta1", "convertedwidgets", Some("default"), &object)
        .await
        .expect("rest::create(ConvertedWidget) must not itself error")
    {
        rest::CreateOutcome::Created(object) => object,
        other => panic!("expected Created, got {other:?}"),
    };
    assert_eq!(created["apiVersion"], "example.com/v1beta1");
    assert_eq!(created["spec"]["convertedVersion"], "v1beta1");

    // The storage-version read must not invoke the webhook: it should expose
    // the version and shape the webhook returned for storage.
    let storage_version = match rest::get(&mut storage, None, "example.com", "v1", "convertedwidgets", Some("default"), "converted-widget")
        .await
        .expect("rest::get(storage version) must not error")
    {
        rest::GetOutcome::Found(object) => object,
        other => panic!("expected Found, got {other:?}"),
    };
    assert_eq!(storage_version["apiVersion"], "example.com/v1");
    assert_eq!(storage_version["spec"]["convertedVersion"], "v1");

    let served_version = match rest::get(&mut storage, None, "example.com", "v1beta1", "convertedwidgets", Some("default"), "converted-widget")
        .await
        .expect("rest::get(served version) must not error")
    {
        rest::GetOutcome::Found(object) => object,
        other => panic!("expected Found, got {other:?}"),
    };
    assert_eq!(served_version["apiVersion"], "example.com/v1beta1");
    assert_eq!(served_version["spec"]["convertedVersion"], "v1beta1");

    let listed = match rest::list(&mut storage, None, "example.com", "v1beta1", "convertedwidgets", Some("default"), "", "", 0, "")
        .await
        .expect("rest::list(served version) must not error")
    {
        rest::ListOutcome::Found(list) => list,
        other => panic!("expected Found, got {other:?}"),
    };
    assert_eq!(listed["items"][0]["apiVersion"], "example.com/v1beta1");
    assert_eq!(listed["items"][0]["spec"]["convertedVersion"], "v1beta1");

    let event = tokio::time::timeout(Duration::from_secs(5), watch_rx.recv())
        .await
        .expect("the converted CRD cache must observe the created object")
        .expect("the converted CRD watch must remain open");
    assert_eq!(event.kind, nodeapiserver::cacher::EventKind::Added);
    let watch_object = nodeapiserver::server::watch_event::to_watch_event_json_with_conversion(
        &event,
        "ConvertedWidget",
        "example.com/v1beta1",
        Some(&mut storage),
        "example.com",
        "convertedwidgets",
        Some(&conversion_webhook),
    )
    .await
    .expect("the converted watch event must have an object")
    .expect("the converted watch event must decode");
    assert_eq!(watch_object["object"]["apiVersion"], "example.com/v1beta1");
    assert_eq!(watch_object["object"]["spec"]["convertedVersion"], "v1beta1");

    tokio::time::timeout(Duration::from_secs(5), webhook_task)
        .await
        .expect("conversion webhook should receive all expected requests")
        .expect("conversion webhook task must not fail");

    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[tokio::test]
async fn conversion_output_is_checked_against_the_storage_version_schema() {
    let Some(nodestore_bin) = find_nodestore_binary() else {
        eprintln!("SKIPPED: no nodestore binary found and building one on demand failed");
        return;
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind conversion webhook listener");
    let webhook_url = format!("http://{}/convert", listener.local_addr().expect("conversion webhook listener address"));
    let webhook_task = spawn_conversion_webhook(listener, vec!["v1"], true);
    let (mut child, _data_dir, mut storage) = spawn_nodestore(&nodestore_bin, 23808).await;

    rest::create(
        &mut storage,
        "apiextensions.k8s.io",
        "v1",
        "customresourcedefinitions",
        None,
        &storage_validation_crd(&webhook_url),
    )
    .await
    .expect("rest::create(CRD) must not itself error");

    let object = json!({
        "apiVersion": "example.com/v1beta1",
        "kind": "StrictConvertedWidget",
        "metadata": {"name": "strict-widget", "namespace": "default"},
        "spec": {"value": "hello"},
    });
    let result = rest::create(&mut storage, "example.com", "v1beta1", "strictconvertedwidgets", Some("default"), &object)
        .await
        .expect("rest::create(StrictConvertedWidget) must not itself error");
    match result {
        rest::CreateOutcome::Invalid(violations) => assert!(violations.iter().any(|violation| violation.contains("spec.storageOnly") && violation.contains("expected type")), "unexpected violations: {violations:?}"),
        other => panic!("expected storage-schema validation failure, got {other:?}"),
    }

    assert_eq!(
        rest::get(&mut storage, None, "example.com", "v1beta1", "strictconvertedwidgets", Some("default"), "strict-widget")
            .await
            .expect("rest::get must not error"),
        rest::GetOutcome::ObjectNotFound,
        "an invalid conversion result must not be persisted",
    );

    tokio::time::timeout(Duration::from_secs(5), webhook_task)
        .await
        .expect("conversion webhook should receive the expected request")
        .expect("conversion webhook task must not fail");

    let _ = child.kill().await;
    let _ = child.wait().await;
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
    let resolved = rest::resolve_dynamic_resource(&mut storage, "example.com", "v1", "widgets")
        .await
        .expect("resolving the CRD must not itself error")
        .expect("the CRD must resolve after creation");
    assert_eq!(resolved.additional_printer_columns[0]["name"], "Color");

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
    let table = nodeapiserver::codec::table::convert_to_table_for_resource_with_crd_columns(
        "example.com",
        "v1",
        "widgets",
        Some(&resolved.additional_printer_columns),
        &listed,
    );
    assert_eq!(table["columnDefinitions"][1]["name"], "Color");
    assert_eq!(table["rows"][0]["cells"], json!(["my-widget", "red"]));

    // 4. Server-Side Apply uses the CRD's runtime schema, rather than
    // returning the built-in-only 501 path. The first manager claims the
    // field, and a different manager changing that field gets a real
    // ownership conflict.
    let apply_config = json!({
        "apiVersion": "example.com/v1",
        "kind": "Widget",
        "metadata": {"name": "my-widget", "namespace": "default"},
        "spec": {"color": "blue"},
    });
    let applied = match rest::server_side_apply(&mut storage, "example.com", "v1", "widgets", Some("default"), "my-widget", "crd-manager", false, &apply_config)
        .await
        .expect("CRD Server-Side Apply must not itself error")
    {
        rest::ApplyOutcome::Applied(object) => object,
        other => panic!("expected Applied, got {other:?}"),
    };
    assert_eq!(applied["spec"]["color"], "blue");
    assert_eq!(applied["metadata"]["managedFields"][0]["manager"], "crd-manager");

    let conflicting_config = json!({
        "apiVersion": "example.com/v1",
        "kind": "Widget",
        "metadata": {"name": "my-widget", "namespace": "default"},
        "spec": {"color": "green"},
    });
    assert!(matches!(
        rest::server_side_apply(&mut storage, "example.com", "v1", "widgets", Some("default"), "my-widget", "other-manager", false, &conflicting_config)
            .await
            .expect("conflicting CRD Server-Side Apply must not itself error"),
        rest::ApplyOutcome::Conflict(conflicts) if conflicts.iter().any(|conflict| conflict.manager == "crd-manager")
    ));

    // 5. A resource this CRD does NOT define is still a genuine
    // UnknownResource -- the dynamic registry doesn't turn into "anything
    // goes."
    let unknown = rest::get(&mut storage, None, "example.com", "v1", "gizmos", Some("default"), "whatever").await.expect("rest::get must not error");
    assert_eq!(unknown, rest::GetOutcome::UnknownResource);

    // 6. DELETE removes it, returning the object as it was.
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
/// built-in; `strategic-merge-patch` is real too now
/// (`apiextensions::schema_strategic_merge`) — the scalar-replacement
/// case here, `strategic_merge_patch_merges_a_crd_list_field_by_its_own_x_kubernetes_list_map_keys`
/// below is the real by-key list-merge case.
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
    let patched = match rest::patch_persist(&mut storage, "example.com", "v1", "widgets", Some("default"), "editable-widget", context, candidate, false).await.expect("rest::patch_persist must not itself error") {
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
    let patched = match rest::patch_persist(&mut storage, "example.com", "v1", "widgets", Some("default"), "editable-widget", context, candidate, false).await.expect("rest::patch_persist must not itself error") {
        rest::UpdateOutcome::Updated(object) => object,
        other => panic!("expected Updated, got {other:?}"),
    };
    assert_eq!(patched["spec"]["color"], "yellow");

    // 4. strategic-merge-patch against a CRD is real now too --
    // a_crd()'s own schema has no list field, so this exercises just the
    // scalar-replacement case; the dedicated test below proves the real
    // by-key list-merge behavior.
    let strategic_patch = json!({"spec": {"color": "purple"}});
    let (candidate, context) = match rest::patch_prepare(&mut storage, "example.com", "v1", "widgets", Some("default"), "editable-widget", rest::PatchKind::StrategicMerge, &strategic_patch)
        .await
        .expect("rest::patch_prepare must not itself error")
    {
        rest::PatchPrepareOutcome::Ready(candidate, context) => (candidate, context),
        other => panic!("expected Ready, got {other:?}"),
    };
    let patched = match rest::patch_persist(&mut storage, "example.com", "v1", "widgets", Some("default"), "editable-widget", context, candidate, false).await.expect("rest::patch_persist must not itself error") {
        rest::UpdateOutcome::Updated(object) => object,
        other => panic!("expected Updated, got {other:?}"),
    };
    assert_eq!(patched["spec"]["color"], "purple");

    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// The real by-key list-merge half of strategic-merge-patch against a
/// CRD: a dedicated small CRD schema declaring `x-kubernetes-list-type:
/// map` / `x-kubernetes-list-map-keys` on a `ports` field, proving a
/// patch element matching an existing one by key merges into it while a
/// non-matching one appends — the same real behavior
/// `crate::patch::strategic_merge`'s own compiled path gives built-in
/// types, now genuinely available for a CRD too.
#[tokio::test]
async fn strategic_merge_patch_merges_a_crd_list_field_by_its_own_x_kubernetes_list_map_keys() {
    let Some(nodestore_bin) = find_nodestore_binary() else {
        eprintln!("SKIPPED: no nodestore binary available and building one on demand failed");
        return;
    };
    let (mut child, _data_dir, mut storage) = spawn_nodestore(&nodestore_bin, 23805).await;

    let crd = json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": "services.example.com"},
        "spec": {
            "group": "example.com",
            "scope": "Namespaced",
            "names": {"plural": "services", "singular": "service", "kind": "ExampleService", "listKind": "ExampleServiceList"},
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
                                    "ports": {
                                        "type": "array",
                                        "x-kubernetes-list-type": "map",
                                        "x-kubernetes-list-map-keys": ["name"],
                                        "items": {"type": "object", "properties": {"name": {"type": "string"}, "port": {"type": "integer"}}},
                                    },
                                },
                            },
                        },
                    },
                },
            }],
        },
    });
    rest::create(&mut storage, "apiextensions.k8s.io", "v1", "customresourcedefinitions", None, &crd).await.expect("rest::create(CRD) must not itself error");

    let object = json!({
        "apiVersion": "example.com/v1",
        "kind": "ExampleService",
        "metadata": {"name": "svc", "namespace": "default"},
        "spec": {"ports": [{"name": "http", "port": 80}, {"name": "https", "port": 443}]},
    });
    rest::create(&mut storage, "example.com", "v1", "services", Some("default"), &object).await.expect("rest::create must not itself error");

    // Patches "http" (matches by name -> merges the port) and adds
    // "metrics" (no match -> appends) in one patch, leaving "https"
    // completely untouched -- proves this is a real merge, not a
    // wholesale replace that happened to look right.
    let patch = json!({"spec": {"ports": [{"name": "http", "port": 8080}, {"name": "metrics", "port": 9090}]}});
    let (candidate, context) = match rest::patch_prepare(&mut storage, "example.com", "v1", "services", Some("default"), "svc", rest::PatchKind::StrategicMerge, &patch)
        .await
        .expect("rest::patch_prepare must not itself error")
    {
        rest::PatchPrepareOutcome::Ready(candidate, context) => (candidate, context),
        other => panic!("expected Ready, got {other:?}"),
    };
    let patched = match rest::patch_persist(&mut storage, "example.com", "v1", "services", Some("default"), "svc", context, candidate, false).await.expect("rest::patch_persist must not itself error") {
        rest::UpdateOutcome::Updated(object) => object,
        other => panic!("expected Updated, got {other:?}"),
    };
    let ports = patched["spec"]["ports"].as_array().expect("ports must be an array");
    assert_eq!(ports.len(), 3, "http merged, https untouched, metrics appended: {ports:?}");
    let http = ports.iter().find(|p| p["name"] == "http").expect("http must still be present");
    assert_eq!(http["port"], 8080, "http's own port must have been merged, not left alone");
    let https = ports.iter().find(|p| p["name"] == "https").expect("https must be untouched");
    assert_eq!(https["port"], 443);
    let metrics = ports.iter().find(|p| p["name"] == "metrics").expect("metrics must have been appended");
    assert_eq!(metrics["port"], 9090);

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

/// Real required/type validation against a CRD's own schema
/// (`apiextensions::schema_validation`) — a separate small CRD (its own
/// dedicated schema declaring `required: ["color"]`) rather than
/// widening `a_crd()`'s own shared fixture, which every other test in
/// this file already relies on accepting a `spec` with no `color` set.
#[tokio::test]
async fn create_rejects_a_crd_defined_object_that_violates_its_own_schema() {
    let Some(nodestore_bin) = find_nodestore_binary() else {
        eprintln!("SKIPPED: no nodestore binary available and building one on demand failed");
        return;
    };
    let (mut child, _data_dir, mut storage) = spawn_nodestore(&nodestore_bin, 23803).await;

    let strict_crd = json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": "gadgets.example.com"},
        "spec": {
            "group": "example.com",
            "scope": "Namespaced",
            "names": {"plural": "gadgets", "singular": "gadget", "kind": "Gadget", "listKind": "GadgetList"},
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
                                "required": ["color"],
                                "properties": {
                                    "color": {"type": "string", "enum": ["red", "blue"], "minLength": 3, "pattern": "^[a-z]+$"},
                                    "weight": {"type": "integer", "minimum": 1, "maximum": 5}
                                },
                            },
                        },
                    },
                },
            }],
        },
    });
    rest::create(&mut storage, "apiextensions.k8s.io", "v1", "customresourcedefinitions", None, &strict_crd).await.expect("rest::create(CRD) must not itself error");

    // Missing the required `color` field entirely.
    let missing_required = json!({
        "apiVersion": "example.com/v1",
        "kind": "Gadget",
        "metadata": {"name": "bad-gadget-1", "namespace": "default"},
        "spec": {"weight": 3},
    });
    match rest::create(&mut storage, "example.com", "v1", "gadgets", Some("default"), &missing_required).await.expect("rest::create must not itself error") {
        rest::CreateOutcome::Invalid(violations) => {
            assert!(violations.iter().any(|v| v.contains("spec.color") && v.contains("Required")), "expected a spec.color required violation, got {violations:?}");
        }
        other => panic!("expected Invalid, got {other:?}"),
    }

    // color present but the wrong JSON kind.
    let wrong_type = json!({
        "apiVersion": "example.com/v1",
        "kind": "Gadget",
        "metadata": {"name": "bad-gadget-2", "namespace": "default"},
        "spec": {"color": "red", "weight": "not-a-number"},
    });
    match rest::create(&mut storage, "example.com", "v1", "gadgets", Some("default"), &wrong_type).await.expect("rest::create must not itself error") {
        rest::CreateOutcome::Invalid(violations) => {
            assert!(violations.iter().any(|v| v.contains("spec.weight") && v.contains("integer")), "expected a spec.weight type violation, got {violations:?}");
        }
        other => panic!("expected Invalid, got {other:?}"),
    }

    let wrong_constraints = json!({
        "apiVersion": "example.com/v1",
        "kind": "Gadget",
        "metadata": {"name": "bad-gadget-3", "namespace": "default"},
        "spec": {"color": "green", "weight": 6},
    });
    match rest::create(&mut storage, "example.com", "v1", "gadgets", Some("default"), &wrong_constraints).await.expect("rest::create must not itself error") {
        rest::CreateOutcome::Invalid(violations) => {
            assert!(violations.iter().any(|v| v.contains("spec.color") && v.contains("one of")), "expected enum violation, got {violations:?}");
            assert!(violations.iter().any(|v| v.contains("spec.weight") && v.contains("at most")), "expected maximum violation, got {violations:?}");
        }
        other => panic!("expected Invalid, got {other:?}"),
    }

    // A genuinely valid object still succeeds.
    let valid = json!({
        "apiVersion": "example.com/v1",
        "kind": "Gadget",
        "metadata": {"name": "good-gadget", "namespace": "default"},
        "spec": {"color": "blue", "weight": 5},
    });
    match rest::create(&mut storage, "example.com", "v1", "gadgets", Some("default"), &valid).await.expect("rest::create must not itself error") {
        rest::CreateOutcome::Created(object) => assert_eq!(object["spec"]["color"], "blue"),
        other => panic!("expected Created, got {other:?}"),
    }

    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Structural-schema pruning (`apiextensions::schema_pruning`) — a
/// client-submitted field the CRD's own schema doesn't declare is
/// silently dropped on `CREATE`, while `metadata` survives untouched
/// even though `a_crd()`'s own schema never mentions it at all (the
/// overwhelmingly common real case: an operator's schema only ever
/// describes `spec`/`status`).
#[tokio::test]
async fn create_prunes_a_field_the_crd_schema_does_not_declare() {
    let Some(nodestore_bin) = find_nodestore_binary() else {
        eprintln!("SKIPPED: no nodestore binary available and building one on demand failed");
        return;
    };
    let (mut child, _data_dir, mut storage) = spawn_nodestore(&nodestore_bin, 23804).await;

    rest::create(&mut storage, "apiextensions.k8s.io", "v1", "customresourcedefinitions", None, &a_crd()).await.expect("rest::create(CRD) must not itself error");

    let widget = json!({
        "apiVersion": "example.com/v1",
        "kind": "Widget",
        "metadata": {"name": "pruned-widget", "namespace": "default", "annotations": {"a": "b"}},
        "spec": {"color": "red", "totallyUndeclaredField": "should be dropped"},
    });
    let created = match rest::create(&mut storage, "example.com", "v1", "widgets", Some("default"), &widget).await.expect("rest::create must not itself error") {
        rest::CreateOutcome::Created(object) => object,
        other => panic!("expected Created, got {other:?}"),
    };
    assert_eq!(created["spec"]["color"], "red", "a declared field must survive");
    assert!(created["spec"].get("totallyUndeclaredField").is_none(), "an undeclared field must be pruned, got {created}");
    assert_eq!(created["metadata"]["annotations"], json!({"a": "b"}), "metadata must survive even though the CRD schema never declares it");

    // Reading it back confirms the pruned shape was what was actually
    // persisted, not just what create()'s own in-memory return value
    // happened to show.
    let read_back = match rest::get(&mut storage, None, "example.com", "v1", "widgets", Some("default"), "pruned-widget").await.expect("rest::get must not error") {
        rest::GetOutcome::Found(object) => object,
        other => panic!("expected Found, got {other:?}"),
    };
    assert!(read_back["spec"].get("totallyUndeclaredField").is_none());

    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// The `status` subresource only exists for a CRD version that actually
/// declares `subresources.status` — `a_crd()`'s own shared schema never
/// does, so `rest::update_status`/`patch_status` against it must be a
/// real `UnknownResource`, not a silent write; a CRD that *does* declare
/// it gets a real, working status write.
#[tokio::test]
async fn status_subresource_is_gated_on_the_crd_declaring_it() {
    let Some(nodestore_bin) = find_nodestore_binary() else {
        eprintln!("SKIPPED: no nodestore binary available and building one on demand failed");
        return;
    };
    let (mut child, _data_dir, mut storage) = spawn_nodestore(&nodestore_bin, 23806).await;

    rest::create(&mut storage, "apiextensions.k8s.io", "v1", "customresourcedefinitions", None, &a_crd()).await.expect("rest::create(CRD) must not itself error");
    let widget = json!({
        "apiVersion": "example.com/v1",
        "kind": "Widget",
        "metadata": {"name": "no-status-widget", "namespace": "default"},
        "spec": {"color": "red"},
    });
    let created = match rest::create(&mut storage, "example.com", "v1", "widgets", Some("default"), &widget).await.expect("rest::create must not itself error") {
        rest::CreateOutcome::Created(object) => object,
        other => panic!("expected Created, got {other:?}"),
    };

    // 1. a_crd()'s own schema never declares subresources.status -- a
    // real UnknownResource, not a silent write.
    let mut with_status = created.clone();
    with_status["status"] = json!({"phase": "Ready"});
    let outcome = rest::update_status(&mut storage, "example.com", "v1", "widgets", Some("default"), "no-status-widget", &with_status, false).await.expect("rest::update_status must not itself error");
    assert_eq!(outcome, rest::UpdateOutcome::UnknownResource);

    let status_patch = json!({"status": {"phase": "Ready"}});
    let outcome = rest::patch_status(&mut storage, "example.com", "v1", "widgets", Some("default"), "no-status-widget", rest::PatchKind::Merge, &status_patch, false)
        .await
        .expect("rest::patch_status must not itself error");
    assert_eq!(outcome, rest::UpdateOutcome::UnknownResource);

    // 2. A CRD that *does* declare subresources.status gets a real,
    // working status write.
    let crd_with_status = json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": "trackers.example.com"},
        "spec": {
            "group": "example.com",
            "scope": "Namespaced",
            "names": {"plural": "trackers", "singular": "tracker", "kind": "Tracker", "listKind": "TrackerList"},
            "versions": [{
                "name": "v1",
                "served": true,
                "storage": true,
                "schema": {"openAPIV3Schema": {"type": "object", "properties": {"spec": {"type": "object"}, "status": {"type": "object", "properties": {"phase": {"type": "string"}}}}}},
                "subresources": {"status": {}},
            }],
        },
    });
    rest::create(&mut storage, "apiextensions.k8s.io", "v1", "customresourcedefinitions", None, &crd_with_status).await.expect("rest::create(CRD) must not itself error");
    let tracker = json!({
        "apiVersion": "example.com/v1",
        "kind": "Tracker",
        "metadata": {"name": "t1", "namespace": "default"},
        "spec": {},
    });
    let created_tracker = match rest::create(&mut storage, "example.com", "v1", "trackers", Some("default"), &tracker).await.expect("rest::create must not itself error") {
        rest::CreateOutcome::Created(object) => object,
        other => panic!("expected Created, got {other:?}"),
    };
    let mut tracker_with_status = created_tracker.clone();
    tracker_with_status["status"] = json!({"phase": "Running"});
    let updated = match rest::update_status(&mut storage, "example.com", "v1", "trackers", Some("default"), "t1", &tracker_with_status, false).await.expect("rest::update_status must not itself error") {
        rest::UpdateOutcome::Updated(object) => object,
        other => panic!("expected Updated, got {other:?}"),
    };
    assert_eq!(updated["status"]["phase"], "Running");
    assert_eq!(updated["spec"], json!({}), "update_status must never touch spec");

    let _ = child.kill().await;
    let _ = child.wait().await;
}
