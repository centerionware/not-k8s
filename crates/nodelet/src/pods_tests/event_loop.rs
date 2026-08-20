//! Runtime-event channel behavior.
//!
//! A closed channel must park the runtime-event branch, and duplicate keys
//! must remain ordinary events rather than causing a hidden busy loop in the
//! receiver itself. These tests isolate the controller's event plumbing from
//! the apiserver and CRI so an event-source regression is obvious.

use super::*;
use crate::runtime::mock::MockRuntime;
use http::{Request, Response};
use kube::client::Body;
use kube::core::PartialObjectMeta;
use std::convert::Infallible;
use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};
use std::time::Duration;
use tokio::sync::mpsc::unbounded_channel;
use tower::service_fn;

#[tokio::test]
async fn runtime_event_yields_the_pod_key_unchanged() {
    let (tx, rx) = unbounded_channel();
    tx.send("kube-system/coredns-abc".to_string()).unwrap();

    let mut events = Some(rx);
    assert_eq!(next_event(&mut events).await, "kube-system/coredns-abc");
}

#[tokio::test]
async fn closed_runtime_event_channel_is_parked_instead_of_spinning() {
    let (tx, rx) = unbounded_channel::<String>();
    drop(tx);
    let mut events = Some(rx);

    // next_event() consumes the close, clears the receiver, then parks. If a
    // future change returned immediately here, the select! loop would spin at
    // 100% CPU even with no runtime events.
    let result = tokio::time::timeout(Duration::from_millis(50), next_event(&mut events)).await;
    assert!(
        result.is_err(),
        "closed runtime channel must not return repeatedly"
    );
    assert!(
        events.is_none(),
        "closed receiver should be removed from the select loop"
    );
}

#[tokio::test]
async fn deleted_referenced_objects_refresh_matching_pods() {
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": "app", "namespace": "default"},
        "spec": {
            "nodeName": "node",
            "containers": [{"name": "app", "image": "busybox"}],
            "volumes": [
                {"name": "cfg", "configMap": {"name": "settings"}},
                {"name": "secret", "secret": {"secretName": "credentials"}}
            ]
        }
    }))
    .unwrap();
    let pod_json = serde_json::to_vec(&pod).unwrap();
    let gets = Arc::new(AtomicUsize::new(0));
    let patches = Arc::new(AtomicUsize::new(0));
    let service = {
        let gets = gets.clone();
        let patches = patches.clone();
        let pod_json = pod_json.clone();
        service_fn(move |request: Request<Body>| {
            let gets = gets.clone();
            let patches = patches.clone();
            let pod_json = pod_json.clone();
            async move {
                if request.method() == http::Method::GET && request.uri().path().ends_with("/pods/app") {
                    gets.fetch_add(1, Ordering::SeqCst);
                }
                if request.method() == http::Method::PATCH {
                    patches.fetch_add(1, Ordering::SeqCst);
                }
                Ok::<_, Infallible>(Response::new(Body::from(pod_json)))
            }
        })
    };
    let client = Client::new(service, "default");
    let runtime = Arc::new(MockRuntime::new());
    let controller = PodController::new(client, runtime, "node".to_string());
    controller.pod_refs.lock().unwrap().insert(
        pod_key("default", "app"),
        refs("default", "app", &["settings"], &["credentials"]),
    );

    let deleted_configmap: PartialObjectMeta<ConfigMap> = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {"name": "settings", "namespace": "default"}
    }))
    .unwrap();
    controller
        .on_referenced_object_event(Event::Delete(deleted_configmap), ReferencedKind::ConfigMap)
        .await;

    let deleted_secret: PartialObjectMeta<Secret> = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {"name": "credentials", "namespace": "default"}
    }))
    .unwrap();
    controller
        .on_referenced_object_event(Event::Delete(deleted_secret), ReferencedKind::Secret)
        .await;

    assert_eq!(gets.load(Ordering::SeqCst), 2, "both delete events must fetch the matching pod");
    assert_eq!(patches.load(Ordering::SeqCst), 2, "both delete events must reconcile the matching pod");
}

fn refs(namespace: &str, name: &str, configmaps: &[&str], secrets: &[&str]) -> PodRefs {
    PodRefs {
        namespace: namespace.to_string(),
        name: name.to_string(),
        configmaps: configmaps.iter().map(|s| s.to_string()).collect(),
        secrets: secrets.iter().map(|s| s.to_string()).collect(),
    }
}
