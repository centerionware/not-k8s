//! attach_required()/find_volume_attachment()/attachment_publish_context():
//! the pure logic behind waiting on a CSI driver's attach (via the
//! VolumeAttachment object an external-attacher produces) before
//! Stage/Publish, in resolve_csi_source().
use super::*;
use k8s_openapi::api::storage::v1::{
    CSIDriver, CSIDriverSpec, VolumeAttachment, VolumeAttachmentSource, VolumeAttachmentSpec, VolumeAttachmentStatus,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::collections::BTreeMap;

fn driver_with(attach_required: Option<bool>) -> CSIDriver {
    CSIDriver {
        metadata: ObjectMeta { name: Some("csi.example.com".to_string()), ..Default::default() },
        spec: CSIDriverSpec { attach_required, ..Default::default() },
    }
}

#[test]
fn no_csi_driver_object_defaults_to_attach_required() {
    assert!(attach_required(None));
}

#[test]
fn attach_required_unset_defaults_to_true() {
    assert!(attach_required(Some(&driver_with(None))));
}

#[test]
fn attach_required_explicitly_true() {
    assert!(attach_required(Some(&driver_with(Some(true)))));
}

#[test]
fn attach_required_explicitly_false() {
    assert!(!attach_required(Some(&driver_with(Some(false)))));
}

fn attachment(attacher: &str, node_name: &str, pv_name: &str, status: Option<VolumeAttachmentStatus>) -> VolumeAttachment {
    VolumeAttachment {
        metadata: ObjectMeta { name: Some("csi-abc123".to_string()), ..Default::default() },
        spec: VolumeAttachmentSpec {
            attacher: attacher.to_string(),
            node_name: node_name.to_string(),
            source: VolumeAttachmentSource { persistent_volume_name: Some(pv_name.to_string()), ..Default::default() },
        },
        status,
    }
}

#[test]
fn finds_the_matching_attachment() {
    let attachments = vec![attachment("csi.example.com", "node-a", "pv-1", None)];
    let found = find_volume_attachment(&attachments, "csi.example.com", "node-a", "pv-1");
    assert!(found.is_some());
}

#[test]
fn ignores_attachments_for_a_different_driver_node_or_volume() {
    let attachments = vec![
        attachment("other.example.com", "node-a", "pv-1", None),
        attachment("csi.example.com", "node-b", "pv-1", None),
        attachment("csi.example.com", "node-a", "pv-2", None),
    ];
    assert!(find_volume_attachment(&attachments, "csi.example.com", "node-a", "pv-1").is_none());
}

#[test]
fn no_attachments_at_all_is_none() {
    assert!(find_volume_attachment(&[], "csi.example.com", "node-a", "pv-1").is_none());
}

#[test]
fn no_status_yet_is_no_publish_context() {
    let att = attachment("csi.example.com", "node-a", "pv-1", None);
    assert!(attachment_publish_context(&att).is_none());
}

#[test]
fn not_yet_attached_is_no_publish_context() {
    let att = attachment(
        "csi.example.com",
        "node-a",
        "pv-1",
        Some(VolumeAttachmentStatus { attached: false, ..Default::default() }),
    );
    assert!(attachment_publish_context(&att).is_none());
}

#[test]
fn attached_with_no_metadata_is_an_empty_publish_context() {
    let att = attachment(
        "csi.example.com",
        "node-a",
        "pv-1",
        Some(VolumeAttachmentStatus { attached: true, attachment_metadata: None, ..Default::default() }),
    );
    assert_eq!(attachment_publish_context(&att), Some(HashMap::new()));
}

#[test]
fn attached_with_metadata_is_returned_as_publish_context() {
    let mut meta = BTreeMap::new();
    meta.insert("devicePath".to_string(), "/dev/xvdf".to_string());
    let att = attachment(
        "csi.example.com",
        "node-a",
        "pv-1",
        Some(VolumeAttachmentStatus { attached: true, attachment_metadata: Some(meta), ..Default::default() }),
    );
    let ctx = attachment_publish_context(&att).unwrap();
    assert_eq!(ctx.get("devicePath"), Some(&"/dev/xvdf".to_string()));
}
