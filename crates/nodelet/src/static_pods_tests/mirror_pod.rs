use super::*;
use k8s_openapi::api::core::v1::{Container, PodSpec};

fn pod(name: &str, namespace: Option<&str>) -> Pod {
    Pod {
        metadata: ObjectMeta { name: Some(name.to_string()), namespace: namespace.map(|s| s.to_string()), ..Default::default() },
        spec: Some(PodSpec {
            containers: vec![Container { name: "app".to_string(), ..Default::default() }],
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn mirror_pod_name_appends_the_node_name() {
    assert_eq!(mirror_pod_name("static-web", "node-1"), "static-web-node-1");
}

#[test]
fn prepare_static_pod_binds_node_name_and_defaults_namespace() {
    let prepared = prepare_static_pod(pod("static-web", None), "node-1");
    assert_eq!(prepared.metadata.namespace.as_deref(), Some("default"));
    assert_eq!(prepared.spec.unwrap().node_name.as_deref(), Some("node-1"));
}

#[test]
fn prepare_static_pod_preserves_an_explicit_namespace() {
    let prepared = prepare_static_pod(pod("static-web", Some("kube-system")), "node-1");
    assert_eq!(prepared.metadata.namespace.as_deref(), Some("kube-system"));
}

#[test]
fn build_mirror_pod_uses_the_derived_name_and_carries_the_spec() {
    let prepared = prepare_static_pod(pod("static-web", Some("kube-system")), "node-1");
    let mirror = build_mirror_pod(&prepared, "node-1");
    assert_eq!(mirror.metadata.name.as_deref(), Some("static-web-node-1"));
    assert_eq!(mirror.metadata.namespace.as_deref(), Some("kube-system"));
    assert_eq!(mirror.spec.as_ref().unwrap().containers[0].name, "app");
}

#[test]
fn build_mirror_pod_sets_the_mirror_annotations() {
    let prepared = prepare_static_pod(pod("static-web", None), "node-1");
    let mirror = build_mirror_pod(&prepared, "node-1");
    let annotations = mirror.metadata.annotations.unwrap();
    assert_eq!(annotations.get("kubernetes.io/config.source").map(|s| s.as_str()), Some("file"));
    assert!(annotations.contains_key("kubernetes.io/config.mirror"));
}

#[test]
fn build_mirror_pod_preserves_existing_labels() {
    let mut source = pod("static-web", None);
    source.metadata.labels = Some([("app".to_string(), "web".to_string())].into_iter().collect());
    let prepared = prepare_static_pod(source, "node-1");
    let mirror = build_mirror_pod(&prepared, "node-1");
    assert_eq!(mirror.metadata.labels.unwrap().get("app").map(|s| s.as_str()), Some("web"));
}
