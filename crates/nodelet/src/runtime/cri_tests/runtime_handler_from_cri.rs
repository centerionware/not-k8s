//! runtime_handler_from_cri(): CRI's RuntimeHandler -> nodelet's own
//! RuntimeHandlerInfo, feeding Node.status.runtimeHandlers (Round 53;
//! found in round 50's re-audit).
use super::*;

#[test]
fn carries_name_and_features_through() {
    let h = v1::RuntimeHandler {
        name: "kata".to_string(),
        features: Some(v1::RuntimeHandlerFeatures { recursive_read_only_mounts: true, user_namespaces: false }),
    };
    let info = runtime_handler_from_cri(h);
    assert_eq!(info.name, "kata");
    assert!(info.recursive_read_only_mounts);
    assert!(!info.user_namespaces);
}

#[test]
fn missing_features_defaults_to_false_not_an_error() {
    let h = v1::RuntimeHandler { name: "runc".to_string(), features: None };
    let info = runtime_handler_from_cri(h);
    assert_eq!(info.name, "runc");
    assert!(!info.recursive_read_only_mounts);
    assert!(!info.user_namespaces);
}

#[test]
fn empty_name_is_preserved_as_the_default_handler_marker() {
    let h = v1::RuntimeHandler { name: String::new(), features: None };
    assert_eq!(runtime_handler_from_cri(h).name, "");
}
