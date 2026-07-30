use super::*;

fn set(keys: &[&str]) -> HashSet<String> {
    keys.iter().map(|s| s.to_string()).collect()
}

fn image(id: &str, tags: &[&str], digests: &[&str]) -> ImageRef {
    ImageRef {
        id: id.to_string(),
        repo_tags: tags.iter().map(|s| s.to_string()).collect(),
        repo_digests: digests.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn image_referenced_by_id_is_kept() {
    let images = vec![image("sha256:abc", &[], &[])];
    let referenced = set(&["sha256:abc"]);
    assert!(images_to_gc(&images, &referenced).is_empty());
}

#[test]
fn image_referenced_only_by_repo_tag_is_kept() {
    // Containers are usually created with a human ref ("busybox:latest"),
    // not the resolved digest — this is the common case, not the edge case.
    let images = vec![image("sha256:abc", &["busybox:latest"], &[])];
    let referenced = set(&["busybox:latest"]);
    assert!(images_to_gc(&images, &referenced).is_empty());
}

#[test]
fn image_referenced_only_by_repo_digest_is_kept() {
    let images = vec![image("sha256:abc", &[], &["busybox@sha256:def"])];
    let referenced = set(&["busybox@sha256:def"]);
    assert!(images_to_gc(&images, &referenced).is_empty());
}

#[test]
fn image_matching_none_of_id_tags_or_digests_is_collected() {
    let images = vec![image("sha256:abc", &["old-app:v1"], &["app@sha256:stale"])];
    let referenced = set(&["busybox:latest"]);
    assert_eq!(images_to_gc(&images, &referenced), vec!["sha256:abc".to_string()]);
}

#[test]
fn mixed_referenced_and_unreferenced_only_collects_the_unreferenced() {
    let images = vec![
        image("sha256:keep", &["nginx:latest"], &[]),
        image("sha256:gc", &["old:v0"], &[]),
    ];
    let referenced = set(&["nginx:latest"]);
    assert_eq!(images_to_gc(&images, &referenced), vec!["sha256:gc".to_string()]);
}

#[test]
fn empty_image_list_returns_empty() {
    assert!(images_to_gc(&[], &set(&["anything"])).is_empty());
}
