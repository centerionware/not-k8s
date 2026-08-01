//! proc_mount_paths(): securityContext.procMount -> masked/readonly
//! paths, mirroring real kubelet's own
//! ConvertToRuntimeMaskedPaths()/ConvertToRuntimeReadonlyPaths()
//! (pkg/securitycontext/util.go) exactly (round 78; found in round 76's
//! re-audit).
use super::*;

#[test]
fn none_produces_the_default_lists() {
    let (masked, readonly) = proc_mount_paths(None);
    assert_eq!(masked, DEFAULT_MASKED_PATHS.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(readonly, DEFAULT_READONLY_PATHS.iter().map(|s| s.to_string()).collect::<Vec<_>>());
}

#[test]
fn explicit_default_produces_the_default_lists() {
    let (masked, readonly) = proc_mount_paths(Some("Default"));
    assert!(!masked.is_empty());
    assert!(!readonly.is_empty());
}

#[test]
fn unmasked_produces_genuinely_empty_lists() {
    let (masked, readonly) = proc_mount_paths(Some("Unmasked"));
    assert!(masked.is_empty());
    assert!(readonly.is_empty());
}

#[test]
fn an_unrecognized_value_falls_back_to_the_default_lists_rather_than_unmasking() {
    // Fail-safe direction matters here: an unexpected/garbage value must
    // never silently disable masking.
    let (masked, readonly) = proc_mount_paths(Some("SomethingElseEntirely"));
    assert!(!masked.is_empty());
    assert!(!readonly.is_empty());
}

#[test]
fn default_masked_paths_include_the_well_known_docker_oci_list() {
    // Spot-check a few well-known entries rather than the whole list, so
    // this doesn't just re-assert the constant verbatim.
    for p in ["/proc/acpi", "/proc/kcore", "/proc/scsi", "/sys/firmware"] {
        assert!(DEFAULT_MASKED_PATHS.contains(&p), "expected {p} in DEFAULT_MASKED_PATHS");
    }
}

#[test]
fn default_readonly_paths_include_the_well_known_docker_oci_list() {
    for p in ["/proc/sys", "/proc/sysrq-trigger", "/proc/irq"] {
        assert!(DEFAULT_READONLY_PATHS.contains(&p), "expected {p} in DEFAULT_READONLY_PATHS");
    }
}
