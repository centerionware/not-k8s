//! Pod watch ordering around same-name replacement.

use super::*;

#[test]
fn an_older_replacement_event_is_ignored_by_resource_version() {
    let replacement = ObservedPod {
        uid: "new-uid".to_string(),
        resource_version: Some(20),
    };
    let stale = ObservedPod {
        uid: "old-uid".to_string(),
        resource_version: Some(19),
    };

    assert!(watch_event_is_stale(Some(&replacement), &stale));
}

#[test]
fn a_newer_pod_event_replaces_the_previous_uid() {
    let old = ObservedPod {
        uid: "old-uid".to_string(),
        resource_version: Some(19),
    };
    let replacement = ObservedPod {
        uid: "new-uid".to_string(),
        resource_version: Some(20),
    };

    assert!(!watch_event_is_stale(Some(&old), &replacement));
}

#[test]
fn an_older_status_event_for_the_same_uid_is_ignored() {
    let current = ObservedPod {
        uid: "pod-uid".to_string(),
        resource_version: Some(20),
    };
    let stale = ObservedPod {
        uid: "pod-uid".to_string(),
        resource_version: Some(19),
    };

    assert!(watch_event_is_stale(Some(&current), &stale));
}
