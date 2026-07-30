//! write_downward_api_volume(): before this, downwardAPI *volumes* weren't
//! materialized at all (only the env-var form of the downward API worked).
use super::*;
use k8s_openapi::api::core::v1::{DownwardAPIVolumeFile, ObjectFieldSelector};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

fn pod() -> Pod {
    Pod {
        metadata: ObjectMeta { name: Some("web-1".to_string()), namespace: Some("default".to_string()), ..Default::default() },
        ..Default::default()
    }
}

fn item(path: &str, field_path: &str) -> DownwardAPIVolumeFile {
    DownwardAPIVolumeFile {
        path: path.to_string(),
        field_ref: Some(ObjectFieldSelector { field_path: field_path.to_string(), ..Default::default() }),
        mode: None,
        resource_field_ref: None,
    }
}

#[test]
fn writes_a_file_per_item_with_the_resolved_field_value() {
    let dir = std::env::temp_dir().join(format!("nodelet-test-downward-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_downward_api_volume(&dir, &pod(), &[item("pod_name", "metadata.name"), item("pod_namespace", "metadata.namespace")])
        .unwrap();

    assert_eq!(std::fs::read_to_string(dir.join("pod_name")).unwrap(), "web-1");
    assert_eq!(std::fs::read_to_string(dir.join("pod_namespace")).unwrap(), "default");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_path_with_subdirectories_creates_them() {
    let dir = std::env::temp_dir().join(format!("nodelet-test-downward-nested-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_downward_api_volume(&dir, &pod(), &[item("labels/team", "metadata.name")]).unwrap();

    assert_eq!(std::fs::read_to_string(dir.join("labels").join("team")).unwrap(), "web-1");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unresolvable_field_ref_is_skipped_not_errored() {
    let dir = std::env::temp_dir().join(format!("nodelet-test-downward-skip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_downward_api_volume(&dir, &pod(), &[item("uid", "metadata.uid")]).unwrap(); // pod() has no uid set

    assert!(!dir.join("uid").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn resource_field_ref_items_without_a_field_ref_are_skipped() {
    let dir = std::env::temp_dir().join(format!("nodelet-test-downward-resource-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let item = DownwardAPIVolumeFile {
        path: "cpu-limit".to_string(),
        field_ref: None,
        mode: None,
        resource_field_ref: Some(k8s_openapi::api::core::v1::ResourceFieldSelector {
            resource: "limits.cpu".to_string(),
            ..Default::default()
        }),
    };
    write_downward_api_volume(&dir, &pod(), &[item]).unwrap();
    assert!(!dir.join("cpu-limit").exists(), "resourceFieldRef isn't supported and must be skipped, not panic");
    std::fs::remove_dir_all(&dir).unwrap();
}
