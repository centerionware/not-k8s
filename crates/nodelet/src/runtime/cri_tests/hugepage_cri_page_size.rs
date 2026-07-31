//! hugepage_cri_page_size(): k8s hugepage resource name suffix -> CRI's
//! HugepageLimit.page_size format (Round 59; found in round 58's
//! re-audit).
use super::*;

#[test]
fn converts_binary_mebibyte_suffix() {
    assert_eq!(hugepage_cri_page_size("2Mi"), Some("2MB".to_string()));
}

#[test]
fn converts_binary_gibibyte_suffix() {
    assert_eq!(hugepage_cri_page_size("1Gi"), Some("1GB".to_string()));
}

#[test]
fn converts_binary_kibibyte_suffix() {
    assert_eq!(hugepage_cri_page_size("64Ki"), Some("64KB".to_string()));
}

#[test]
fn a_suffix_without_the_trailing_i_is_rejected() {
    // Real hugepage resource names always use the binary Ki/Mi/Gi suffix
    // (apiserver validation enforces this) — a malformed one should be
    // skipped, not silently mistranslated.
    assert_eq!(hugepage_cri_page_size("2M"), None);
}
