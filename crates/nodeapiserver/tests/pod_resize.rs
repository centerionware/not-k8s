//! REST-level regression coverage for the core Pod `resize` subresource.

mod support;

use nodeapiserver::server::rest;
use serde_json::json;
use support::{find_nodestore_binary, spawn_nodestore};

#[tokio::test]
async fn pod_resize_updates_resources_without_replacing_the_pod() {
    let Some(nodestore_bin) = find_nodestore_binary() else {
        eprintln!("SKIPPED: no nodestore binary available and building one on demand failed");
        return;
    };
    let (_child, _data_dir, mut storage) = spawn_nodestore(&nodestore_bin, 23813).await;
    let pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": "resize-pod", "namespace": "default", "labels": {"keep": "yes"}},
        "spec": {
            "nodeName": "node-a",
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sleep", "3600"],
                "resources": {"limits": {"memory": "128Mi"}}
            }]
        }
    });
    let created = rest::create(&mut storage, "", "v1", "pods", Some("default"), &pod)
        .await
        .expect("rest::create must not itself error");
    assert!(matches!(created, rest::CreateOutcome::Created(_)));

    let outcome = rest::patch_pod_resize(
        &mut storage,
        "default",
        "resize-pod",
        rest::PatchKind::Merge,
        &json!({
            "metadata": {"labels": {"keep": "no"}},
            "spec": {
                "nodeName": "node-b",
                "containers": [{
                    "name": "app",
                    "image": "should-be-ignored",
                    "resources": {"limits": {"memory": "256Mi"}}
                }]
            }
        }),
        false,
        None,
    )
    .await
    .expect("rest::patch_pod_resize must not itself error");
    let resized = match outcome {
        rest::UpdateOutcome::Updated(object) => object,
        other => panic!("expected Updated, got {other:?}"),
    };
    assert_eq!(resized["metadata"]["labels"]["keep"], "yes");
    assert_eq!(resized["spec"]["nodeName"], "node-a");
    assert_eq!(resized["spec"]["containers"][0]["image"], "busybox:latest");
    assert_eq!(
        resized["spec"]["containers"][0]["resources"]["limits"]["memory"],
        "256Mi"
    );

    let fetched = rest::get_pod_resize(&mut storage, "default", "resize-pod")
        .await
        .expect("rest::get_pod_resize must not itself error");
    match fetched {
        rest::GetOutcome::Found(object) => assert_eq!(object, resized),
        other => panic!("expected Found, got {other:?}"),
    }

    // A second write exercises a payload carrying metadata from a previous
    // write, rather than only the RV-less freshly-created Pod.
    let outcome = rest::patch_pod_resize(&mut storage, "default", "resize-pod",
        rest::PatchKind::Merge,
        &json!({"spec":{"containers":[{"name":"app","resources":{"limits":{"memory":"512Mi"}}}]}}),
        false, None).await.unwrap();
    assert!(matches!(outcome, rest::UpdateOutcome::Updated(_)), "{outcome:?}");
    let stale = rest::patch_pod_resize(&mut storage, "default", "resize-pod",
        rest::PatchKind::Merge,
        &json!({"metadata":{"resourceVersion":resized["metadata"]["resourceVersion"]}}),
        false, None).await.unwrap();
    assert!(matches!(stale, rest::UpdateOutcome::Conflict), "{stale:?}");
}
