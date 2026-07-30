use super::*;

const YAML_MANIFEST: &str = "\
apiVersion: v1
kind: Pod
metadata:
  name: static-web
  namespace: kube-system
spec:
  containers:
    - name: web
      image: nginx:latest
";

const JSON_MANIFEST: &str = r#"{
  "apiVersion": "v1",
  "kind": "Pod",
  "metadata": {"name": "static-web-json"},
  "spec": {"containers": [{"name": "web", "image": "nginx:latest"}]}
}"#;

#[test]
fn parses_a_yaml_manifest() {
    let pod = parse_manifest(YAML_MANIFEST.as_bytes()).unwrap();
    assert_eq!(pod.metadata.name.as_deref(), Some("static-web"));
    assert_eq!(pod.metadata.namespace.as_deref(), Some("kube-system"));
    assert_eq!(pod.spec.unwrap().containers[0].name, "web");
}

#[test]
fn parses_a_json_manifest_too() {
    // YAML is a JSON superset — one parser handles both.
    let pod = parse_manifest(JSON_MANIFEST.as_bytes()).unwrap();
    assert_eq!(pod.metadata.name.as_deref(), Some("static-web-json"));
}

#[test]
fn garbage_bytes_return_an_error_not_a_panic() {
    assert!(parse_manifest(b"not: [valid yaml or json pod").is_err());
}

#[test]
fn empty_bytes_parse_to_a_default_pod_not_a_panic() {
    // k8s_openapi's Deserialize impls default missing/null fields rather
    // than requiring them, so an empty manifest parses successfully into a
    // Pod with no name — not an error. A pod like that just won't have a
    // usable identity downstream (mirror_pod_name would be "-<node>").
    let pod = parse_manifest(b"").unwrap();
    assert!(pod.metadata.name.is_none());
}
