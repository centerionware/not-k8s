//! image_tag()/effective_pull_policy(): imagePullPolicy enforcement
//! (Round 51; found in round 50's re-audit). Before this round,
//! create_and_start_container() called PullImage unconditionally,
//! regardless of Always/IfNotPresent/Never.
use super::*;

#[test]
fn image_tag_extracts_the_tag_after_the_repo_name() {
    assert_eq!(image_tag("nginx:1.25"), Some("1.25"));
    assert_eq!(image_tag("nginx:latest"), Some("latest"));
}

#[test]
fn image_tag_none_for_an_untagged_repo() {
    assert_eq!(image_tag("nginx"), None);
}

#[test]
fn image_tag_none_for_a_digest_reference() {
    assert_eq!(image_tag("nginx@sha256:abc123"), None);
}

#[test]
fn image_tag_ignores_a_registry_host_port_as_the_separator() {
    // The ':5000' here is a registry port, not a tag separator — only the
    // segment after the last '/' should ever be checked for a ':'.
    assert_eq!(image_tag("myregistry.io:5000/nginx"), None);
    assert_eq!(image_tag("myregistry.io:5000/nginx:1.25"), Some("1.25"));
}

#[test]
fn explicit_policy_always_wins_regardless_of_tag() {
    assert_eq!(effective_pull_policy(Some("Always"), "nginx:1.25"), "Always");
    assert_eq!(effective_pull_policy(Some("IfNotPresent"), "nginx:latest"), "IfNotPresent");
    assert_eq!(effective_pull_policy(Some("Never"), "nginx"), "Never");
}

#[test]
fn unset_policy_defaults_to_always_for_untagged_or_latest() {
    assert_eq!(effective_pull_policy(None, "nginx"), "Always");
    assert_eq!(effective_pull_policy(None, "nginx:latest"), "Always");
}

#[test]
fn unset_policy_defaults_to_if_not_present_for_a_specific_tag_or_digest() {
    assert_eq!(effective_pull_policy(None, "nginx:1.25"), "IfNotPresent");
    assert_eq!(effective_pull_policy(None, "nginx@sha256:abc123"), "IfNotPresent");
}

#[test]
fn an_unrecognized_explicit_policy_string_falls_back_to_the_default_heuristic() {
    // Defensive: the apiserver already validates this enum, but a garbage
    // value shouldn't silently misbehave as some other real policy either.
    assert_eq!(effective_pull_policy(Some("bogus"), "nginx:1.25"), "IfNotPresent");
}
