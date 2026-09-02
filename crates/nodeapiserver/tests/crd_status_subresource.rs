//! Status-subresource behavior for CRD-defined resources.
//!
//! The shared nodestore fixtures live in `crd_roundtrip.rs`; this focused
//! module keeps that broad lifecycle test below the repository's file-size
//! ceiling without duplicating its setup.

mod support;
use nodeapiserver::server::rest;
use serde_json::json;
use support::{a_crd, find_nodestore_binary, spawn_nodestore};

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

    rest::create(
        &mut storage,
        "apiextensions.k8s.io",
        "v1",
        "customresourcedefinitions",
        None,
        &a_crd(),
    )
    .await
    .expect("rest::create(CRD) must not itself error");
    let widget = json!({
        "apiVersion": "example.com/v1",
        "kind": "Widget",
        "metadata": {"name": "no-status-widget", "namespace": "default"},
        "spec": {"color": "red"},
    });
    let created = match rest::create(
        &mut storage,
        "example.com",
        "v1",
        "widgets",
        Some("default"),
        &widget,
    )
    .await
    .expect("rest::create must not itself error")
    {
        rest::CreateOutcome::Created(object) => object,
        other => panic!("expected Created, got {other:?}"),
    };

    // 1. a_crd()'s own schema never declares subresources.status -- a
    // real UnknownResource, not a silent write.
    let mut with_status = created.clone();
    with_status["status"] = json!({"phase": "Ready"});
    let outcome = rest::update_status(
        &mut storage,
        "example.com",
        "v1",
        "widgets",
        Some("default"),
        "no-status-widget",
        &with_status,
        false,
    )
    .await
    .expect("rest::update_status must not itself error");
    assert_eq!(outcome, rest::UpdateOutcome::UnknownResource);

    let status_patch = json!({"status": {"phase": "Ready"}});
    let outcome = rest::patch_status(
        &mut storage,
        "example.com",
        "v1",
        "widgets",
        Some("default"),
        "no-status-widget",
        rest::PatchKind::Merge,
        &status_patch,
        false,
    )
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
    rest::create(
        &mut storage,
        "apiextensions.k8s.io",
        "v1",
        "customresourcedefinitions",
        None,
        &crd_with_status,
    )
    .await
    .expect("rest::create(CRD) must not itself error");
    let tracker = json!({
        "apiVersion": "example.com/v1",
        "kind": "Tracker",
        "metadata": {"name": "t1", "namespace": "default"},
        "spec": {},
    });
    let created_tracker = match rest::create(
        &mut storage,
        "example.com",
        "v1",
        "trackers",
        Some("default"),
        &tracker,
    )
    .await
    .expect("rest::create must not itself error")
    {
        rest::CreateOutcome::Created(object) => object,
        other => panic!("expected Created, got {other:?}"),
    };
    let mut tracker_with_status = created_tracker.clone();
    tracker_with_status["status"] = json!({"phase": "Running"});
    let updated = match rest::update_status(
        &mut storage,
        "example.com",
        "v1",
        "trackers",
        Some("default"),
        "t1",
        &tracker_with_status,
        false,
    )
    .await
    .expect("rest::update_status must not itself error")
    {
        rest::UpdateOutcome::Updated(object) => object,
        other => panic!("expected Updated, got {other:?}"),
    };
    assert_eq!(updated["status"]["phase"], "Running");
    assert_eq!(
        updated["spec"],
        json!({}),
        "update_status must never touch spec"
    );

    let _ = child.kill().await;
    let _ = child.wait().await;
}
