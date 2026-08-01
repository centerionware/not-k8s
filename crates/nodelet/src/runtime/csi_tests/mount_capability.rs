use super::*;
use v1::volume_capability::AccessType as AT;

#[test]
fn read_write_uses_single_node_writer_access_mode() {
    let cap = mount_capability("ext4", false, false);
    assert_eq!(cap.access_mode.unwrap().mode, access_mode::Mode::SingleNodeWriter as i32);
}

#[test]
fn read_only_uses_multi_node_reader_only_access_mode() {
    let cap = mount_capability("ext4", true, false);
    assert_eq!(cap.access_mode.unwrap().mode, access_mode::Mode::MultiNodeReaderOnly as i32);
}

#[test]
fn fs_type_is_carried_into_the_mount_volume_access_type() {
    let cap = mount_capability("xfs", false, false);
    match cap.access_type {
        Some(AT::Mount(m)) => assert_eq!(m.fs_type, "xfs"),
        other => panic!("expected Mount access type, got {other:?}"),
    }
}

#[test]
fn empty_fs_type_means_let_the_driver_pick() {
    let cap = mount_capability("", false, false);
    match cap.access_type {
        Some(AT::Mount(m)) => assert_eq!(m.fs_type, ""),
        other => panic!("expected Mount access type, got {other:?}"),
    }
}

#[test]
fn block_true_produces_a_block_access_type_regardless_of_fs_type() {
    let cap = mount_capability("ext4", false, true);
    match cap.access_type {
        Some(AT::Block(_)) => {}
        other => panic!("expected Block access type, got {other:?}"),
    }
}

#[test]
fn block_still_respects_read_only_access_mode() {
    let cap = mount_capability("", true, true);
    assert_eq!(cap.access_mode.unwrap().mode, access_mode::Mode::MultiNodeReaderOnly as i32);
}
