//! image_matches_pattern(): real kubelet's own documented matchImages
//! glob rules — domain-segment wildcards, exact port matching, path
//! prefix matching.
use super::*;

#[test]
fn exact_bare_registry_matches_itself() {
    assert!(image_matches_pattern("gcr.io/project/image:tag", "gcr.io"));
}

#[test]
fn exact_registry_does_not_match_a_different_one() {
    assert!(!image_matches_pattern("gcr.io/project/image:tag", "quay.io"));
}

#[test]
fn leading_subdomain_glob_matches_one_segment() {
    assert!(image_matches_pattern("myregistry.azurecr.io/image:tag", "*.azurecr.io"));
}

#[test]
fn glob_never_spans_multiple_segments() {
    // *.io has 2 domain segments; foo.k8s.io has 3 -- segment counts
    // differ, so this must NOT match (the documented "*.io does not
    // match *.k8s.io" case).
    assert!(!image_matches_pattern("foo.k8s.io/image", "*.io"));
}

#[test]
fn ecr_style_pattern_matches_a_real_account_registry() {
    assert!(image_matches_pattern("123456789012.dkr.ecr.us-east-1.amazonaws.com/repo:tag", "*.dkr.ecr.*.amazonaws.com"));
}

#[test]
fn ecr_style_pattern_does_not_match_a_different_domain_shape() {
    assert!(!image_matches_pattern("123456789012.dkr.ecr.us-east-1.amazonaws.com.cn/repo:tag", "*.dkr.ecr.*.amazonaws.com"));
}

#[test]
fn port_in_pattern_must_match_exactly() {
    assert!(image_matches_pattern("registry.io:8080/path/image:tag", "registry.io:8080/path"));
    assert!(!image_matches_pattern("registry.io:9090/path/image:tag", "registry.io:8080/path"));
}

#[test]
fn pattern_with_no_port_matches_regardless_of_the_images_port() {
    assert!(image_matches_pattern("registry.io:8080/image", "registry.io"));
    assert!(image_matches_pattern("registry.io/image", "registry.io"));
}

#[test]
fn pattern_path_must_be_a_prefix_of_the_images_path() {
    assert!(image_matches_pattern("registry.io/team/app/image:tag", "registry.io/team"));
    assert!(!image_matches_pattern("registry.io/other/app/image:tag", "registry.io/team"));
}

#[test]
fn bare_pattern_with_no_path_matches_any_path() {
    assert!(image_matches_pattern("registry.io/deeply/nested/path/image:tag", "registry.io"));
}

#[test]
fn multi_segment_glob_pattern_matches_two_wildcard_segments() {
    assert!(image_matches_pattern("a.b.registry.io/image", "*.*.registry.io"));
}
