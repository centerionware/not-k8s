//! registry_host_for_image() / parse_dockerconfigjson(): before this,
//! PullImageRequest.auth was hardcoded None — private registries could never
//! be pulled from regardless of imagePullSecrets.
use super::*;

#[test]
fn unqualified_image_resolves_to_docker_io() {
    assert_eq!(registry_host_for_image("busybox:latest"), "docker.io");
    assert_eq!(registry_host_for_image("library/busybox"), "docker.io");
}

#[test]
fn image_with_a_dotted_registry_host_is_recognized() {
    assert_eq!(registry_host_for_image("myregistry.example.com/team/app:v1"), "myregistry.example.com");
}

#[test]
fn image_with_a_port_in_the_registry_host_is_recognized() {
    assert_eq!(registry_host_for_image("localhost:5000/app:v1"), "localhost:5000");
}

#[test]
fn bare_localhost_without_a_port_is_recognized_as_a_host() {
    assert_eq!(registry_host_for_image("localhost/app:v1"), "localhost");
}

fn dockerconfigjson(host: &str, user: &str, pass: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "auths": { host: { "username": user, "password": pass } }
    }))
    .unwrap()
}

#[test]
fn plain_username_password_entry_is_extracted() {
    let bytes = dockerconfigjson("myregistry.example.com", "alice", "hunter2");
    let auth = parse_dockerconfigjson(&bytes, "myregistry.example.com").unwrap();
    assert_eq!(auth, ("alice".to_string(), "hunter2".to_string()));
}

#[test]
fn base64_auth_field_entry_is_decoded() {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode("bob:s3cret");
    let bytes = serde_json::to_vec(&serde_json::json!({
        "auths": { "myregistry.example.com": { "auth": encoded } }
    }))
    .unwrap();
    let auth = parse_dockerconfigjson(&bytes, "myregistry.example.com").unwrap();
    assert_eq!(auth, ("bob".to_string(), "s3cret".to_string()));
}

#[test]
fn docker_hub_alias_index_docker_io_v1_is_recognized_for_docker_io() {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "auths": { "https://index.docker.io/v1/": { "username": "alice", "password": "hunter2" } }
    }))
    .unwrap();
    let auth = parse_dockerconfigjson(&bytes, "docker.io").unwrap();
    assert_eq!(auth, ("alice".to_string(), "hunter2".to_string()));
}

#[test]
fn no_matching_host_returns_none() {
    let bytes = dockerconfigjson("otherregistry.example.com", "alice", "hunter2");
    assert!(parse_dockerconfigjson(&bytes, "myregistry.example.com").is_none());
}

#[test]
fn malformed_json_returns_none_not_a_panic() {
    assert!(parse_dockerconfigjson(b"not json", "myregistry.example.com").is_none());
}

#[test]
fn missing_auths_key_returns_none() {
    let bytes = serde_json::to_vec(&serde_json::json!({})).unwrap();
    assert!(parse_dockerconfigjson(&bytes, "myregistry.example.com").is_none());
}
