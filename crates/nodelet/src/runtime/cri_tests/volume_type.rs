//! volume_source_type(): make unsupported-volume diagnostics identify the
//! actual Kubernetes VolumeSource variant.
use super::*;

#[test]
fn identifies_the_service_account_volume_as_projected() {
    let volume = Volume {
        name: "kube-api-access-test".to_string(),
        projected: Some(Default::default()),
        ..Default::default()
    };
    assert_eq!(volume_source_type(&volume), "projected");
}

#[test]
fn identifies_host_path_separately_from_projected() {
    let volume = Volume {
        name: "local-data".to_string(),
        host_path: Some(Default::default()),
        ..Default::default()
    };
    assert_eq!(volume_source_type(&volume), "hostPath");
}

#[test]
fn identifies_the_supported_volume_sources() {
    let config_map = Volume {
        name: "config".to_string(),
        config_map: Some(Default::default()),
        ..Default::default()
    };
    let secret = Volume {
        name: "secret".to_string(),
        secret: Some(Default::default()),
        ..Default::default()
    };
    let empty_dir = Volume {
        name: "scratch".to_string(),
        empty_dir: Some(Default::default()),
        ..Default::default()
    };

    assert_eq!(volume_source_type(&config_map), "configMap");
    assert_eq!(volume_source_type(&secret), "secret");
    assert_eq!(volume_source_type(&empty_dir), "emptyDir");
}
