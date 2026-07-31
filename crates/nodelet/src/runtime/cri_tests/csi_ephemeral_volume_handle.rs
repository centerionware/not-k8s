//! csi_ephemeral_volume_handle(): the synthetic volume_id minted for a CSI
//! ephemeral (inline) volume, since there's no PV/PVC to derive a real one
//! from (Round 46; found in round 45's re-audit).
use super::*;

#[test]
fn combines_pod_uid_and_volume_name() {
    assert_eq!(csi_ephemeral_volume_handle("uid-1", "secrets"), "uid-1-secrets");
}

#[test]
fn different_volumes_in_the_same_pod_get_different_handles() {
    let a = csi_ephemeral_volume_handle("uid-1", "vol-a");
    let b = csi_ephemeral_volume_handle("uid-1", "vol-b");
    assert_ne!(a, b);
}

#[test]
fn the_same_volume_name_in_different_pods_gets_different_handles() {
    // Stability across pod recreations under the same name is the whole
    // point of keying by uid, not pod name.
    let a = csi_ephemeral_volume_handle("uid-1", "secrets");
    let b = csi_ephemeral_volume_handle("uid-2", "secrets");
    assert_ne!(a, b);
}
