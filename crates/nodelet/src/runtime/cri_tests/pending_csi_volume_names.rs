//! pending_csi_volume_names(): round 124's gate on letting a container
//! start before its declared PVC-backed volume actually resolved.
use super::*;
use k8s_openapi::api::core::v1::{
    EphemeralVolumeSource, PersistentVolumeClaimVolumeSource, PodSpec, ProjectedVolumeSource,
    ServiceAccountTokenProjection, Volume, VolumeProjection,
};

fn pvc_volume(name: &str) -> Volume {
    Volume {
        name: name.to_string(),
        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
            claim_name: format!("{name}-claim"),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn ephemeral_volume(name: &str) -> Volume {
    Volume { name: name.to_string(), ephemeral: Some(EphemeralVolumeSource::default()), ..Default::default() }
}

fn csi_inline_volume(name: &str) -> Volume {
    Volume { name: name.to_string(), csi: Some(Default::default()), ..Default::default() }
}

fn pod_with_volumes(volumes: Vec<Volume>) -> Pod {
    Pod { spec: Some(PodSpec { volumes: Some(volumes), ..Default::default() }), ..Default::default() }
}

#[test]
fn unresolved_pvc_volume_is_pending() {
    let pod = pod_with_volumes(vec![pvc_volume("data")]);
    let resolved = HashMap::new();
    assert_eq!(pending_csi_volume_names(&pod, &resolved), vec!["data".to_string()]);
}

#[test]
fn resolved_pvc_volume_is_not_pending() {
    let pod = pod_with_volumes(vec![pvc_volume("data")]);
    let mut resolved = HashMap::new();
    resolved.insert("data".to_string(), ResolvedVolume::HostPath(PathBuf::from("/var/lib/nodelet/pods/x/volumes/data")));
    assert!(pending_csi_volume_names(&pod, &resolved).is_empty());
}

#[test]
fn unresolved_generic_ephemeral_volume_is_pending() {
    let pod = pod_with_volumes(vec![ephemeral_volume("scratch")]);
    let resolved = HashMap::new();
    assert_eq!(pending_csi_volume_names(&pod, &resolved), vec!["scratch".to_string()]);
}

#[test]
fn unresolved_csi_inline_volume_is_never_pending() {
    // No attach/Bound concept at all (round 46) — an unresolved one here
    // is a real driver error, not a timing race; blocking on it would
    // just wedge the pod forever.
    let pod = pod_with_volumes(vec![csi_inline_volume("data")]);
    let resolved = HashMap::new();
    assert!(pending_csi_volume_names(&pod, &resolved).is_empty());
}

#[test]
fn non_csi_volumes_are_never_pending() {
    let vol = Volume { name: "cm".to_string(), ..Default::default() };
    let pod = pod_with_volumes(vec![vol]);
    let resolved = HashMap::new();
    assert!(pending_csi_volume_names(&pod, &resolved).is_empty());
}

#[test]
fn no_volumes_at_all_is_not_pending() {
    let pod = Pod::default();
    let resolved = HashMap::new();
    assert!(pending_csi_volume_names(&pod, &resolved).is_empty());
}

#[test]
fn mix_of_resolved_and_unresolved_reports_only_the_unresolved_names() {
    let pod = pod_with_volumes(vec![pvc_volume("data"), pvc_volume("logs")]);
    let mut resolved = HashMap::new();
    resolved.insert("data".to_string(), ResolvedVolume::HostPath(PathBuf::from("/x")));
    assert_eq!(pending_csi_volume_names(&pod, &resolved), vec!["logs".to_string()]);
}

fn projected_token_volume(name: &str) -> Volume {
    Volume {
        name: name.to_string(),
        projected: Some(ProjectedVolumeSource {
            sources: Some(vec![VolumeProjection {
                service_account_token: Some(ServiceAccountTokenProjection {
                    path: "token".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn failed_projected_service_account_token_is_pending() {
    let pod = pod_with_volumes(vec![projected_token_volume("api-token")]);
    let mut resolved = HashMap::new();
    resolved.insert(
        "api-token".to_string(),
        ResolvedVolume::Invalid("TokenRequest returned 404".to_string()),
    );
    assert_eq!(
        pending_projected_token_volume_names(&pod, &resolved),
        vec!["api-token".to_string()]
    );
}

#[test]
fn resolved_projected_service_account_token_is_not_pending() {
    let pod = pod_with_volumes(vec![projected_token_volume("api-token")]);
    let mut resolved = HashMap::new();
    resolved.insert(
        "api-token".to_string(),
        ResolvedVolume::HostPath(PathBuf::from("/var/lib/nodelet/pods/x/volumes/api-token")),
    );
    assert!(pending_projected_token_volume_names(&pod, &resolved).is_empty());
}
