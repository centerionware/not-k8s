use super::*;
use v1::{node_service_capability, node_service_capability::rpc};

fn rpc_cap(t: rpc::Type) -> NodeServiceCapability {
    NodeServiceCapability { r#type: Some(node_service_capability::Type::Rpc(node_service_capability::Rpc { r#type: t as i32 })) }
}

#[test]
fn empty_capabilities_means_no_stage_unstage_support() {
    assert!(!has_stage_unstage_capability(&[]));
}

#[test]
fn stage_unstage_capability_present_is_detected() {
    let caps = vec![rpc_cap(rpc::Type::StageUnstageVolume)];
    assert!(has_stage_unstage_capability(&caps));
}

#[test]
fn other_capabilities_alone_do_not_imply_stage_unstage() {
    let caps = vec![rpc_cap(rpc::Type::GetVolumeStats), rpc_cap(rpc::Type::ExpandVolume)];
    assert!(!has_stage_unstage_capability(&caps));
}

#[test]
fn stage_unstage_among_several_capabilities_is_still_found() {
    let caps = vec![rpc_cap(rpc::Type::GetVolumeStats), rpc_cap(rpc::Type::StageUnstageVolume), rpc_cap(rpc::Type::ExpandVolume)];
    assert!(has_stage_unstage_capability(&caps));
}

#[test]
fn a_capability_with_no_type_set_is_ignored_not_a_panic() {
    let caps = vec![NodeServiceCapability { r#type: None }];
    assert!(!has_stage_unstage_capability(&caps));
}
