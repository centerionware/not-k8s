//! REST-level regression coverage for Pod graceful deletion.

mod support;

use nodeapiserver::server::rest;
use serde_json::json;
use support::{find_nodestore_binary, spawn_nodestore};

#[tokio::test]
async fn pod_delete_preserves_the_object_until_a_force_delete() {
    let Some(nodestore_bin) = find_nodestore_binary() else {
        eprintln!("SKIPPED: no nodestore binary available and building one on demand failed");
        return;
    };
    let (_child, _data_dir, mut storage) = spawn_nodestore(&nodestore_bin, 23814).await;
    let pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": "graceful-delete", "namespace": "default"},
        "spec": {
            "terminationGracePeriodSeconds": 8,
            "nodeName": "node-a",
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }
    });
    assert!(matches!(
        rest::create(&mut storage, "", "v1", "pods", Some("default"), &pod)
            .await
            .expect("create must not error"),
        rest::CreateOutcome::Created(_)
    ));

    let deleting = match rest::delete_with_options(
        &mut storage,
        "",
        "v1",
        "pods",
        Some("default"),
        "graceful-delete",
        None,
        None,
        false,
    )
    .await
    .expect("graceful delete must not error")
    {
        rest::DeleteOutcome::Deleted(object) => object,
        other => panic!("expected the deleting Pod, got {other:?}"),
    };
    assert!(deleting["metadata"]["deletionTimestamp"].is_string());
    assert_eq!(deleting["metadata"]["deletionGracePeriodSeconds"], json!(8));

    let still_present = rest::get(
        &mut storage,
        None,
        "",
        "v1",
        "pods",
        Some("default"),
        "graceful-delete",
    )
    .await
    .expect("get must not error");
    assert!(matches!(still_present, rest::GetOutcome::Found(_)));

    let deleted = rest::delete_with_options(
        &mut storage,
        "",
        "v1",
        "pods",
        Some("default"),
        "graceful-delete",
        None,
        Some(0),
        false,
    )
    .await
    .expect("force delete must not error");
    assert!(matches!(deleted, rest::DeleteOutcome::Deleted(_)));
    assert!(matches!(
        rest::get(
            &mut storage,
            None,
            "",
            "v1",
            "pods",
            Some("default"),
            "graceful-delete"
        )
        .await
        .expect("get must not error"),
        rest::GetOutcome::ObjectNotFound
    ));
}
