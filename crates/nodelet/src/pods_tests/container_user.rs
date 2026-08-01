//! container_user_field() (round 90; found in round 89's re-audit):
//! containerStatuses[].user -- the resolved UID/GID/supplementalGroups
//! a container's first process actually started with.
use super::*;

#[test]
fn none_produces_none() {
    assert!(container_user_field(None).is_none());
}

#[test]
fn maps_uid_and_gid_through() {
    let user = container_user_field(Some(&(1000, 2000, vec![]))).unwrap();
    let linux = user.linux.unwrap();
    assert_eq!(linux.uid, 1000);
    assert_eq!(linux.gid, 2000);
}

#[test]
fn empty_supplemental_groups_stays_unset_not_an_empty_list() {
    let user = container_user_field(Some(&(0, 0, vec![]))).unwrap();
    assert!(user.linux.unwrap().supplemental_groups.is_none());
}

#[test]
fn non_empty_supplemental_groups_is_carried_through() {
    let user = container_user_field(Some(&(0, 0, vec![5000, 6000]))).unwrap();
    assert_eq!(user.linux.unwrap().supplemental_groups, Some(vec![5000, 6000]));
}
