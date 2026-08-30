//! `/version` — the version.Info response for Group E. Real shape confirmed
//! directly against upstream's
//! `staging/src/k8s.io/apimachinery/pkg/version/types.go` `Info` struct,
//! not assumed from memory: `major`/`minor`/`gitVersion`/`gitCommit`/
//! `gitTreeState`/`buildDate`/`goVersion`/`compiler`/`platform`. The
//! `emulationMajor`/`emulationMinor`/`minCompatibilityMajor`/
//! `minCompatibilityMinor` fields are all `omitempty` in the real struct
//! and only meaningful for upstream's version-skew emulation feature,
//! which this crate doesn't implement — genuinely absent here, not
//! silently zero-valued, matching what a real server with that feature
//! off also sends.
//!
//! # What's real here and what's necessarily approximate
//!
//! `major`/`minor` are parsed from `vendor/REF` (`"release-1.34"` ->
//! `"1"`/`"34"`) — the actual API-compatibility level this build targets,
//! not invented. `gitVersion` follows the same `vX.Y.Z+<suffix>` build
//! metadata convention real distros use to mark a non-stock control plane
//! (K3s ships `v1.28.3+k3s1`, for example) — `+notk8s` here, so a client
//! parsing this as semver still gets a valid version, but one that's
//! honestly distinguishable from a stock kube-apiserver release.
//! `gitCommit`/`gitTreeState`/`buildDate` are captured for real at build
//! time (`build.rs`, via `git`/`date`, each degrading to `"unknown"`
//! rather than failing the build if unavailable — a release tarball build
//! host has no `.git` to ask). `goVersion`/`compiler` are inherently
//! Go-specific fields with no faithful equivalent in a Rust binary — real
//! upstream's own doc comment doesn't define what a non-Go implementation
//! should put there, so this module reports the actual `rustc`
//! version/compiler used to build *this* binary rather than fabricating a
//! Go toolchain version, on the reasoning that "the real toolchain that
//! actually produced this binary" is a more honest answer than either an
//! invented Go version or a blank field. `platform` is real
//! (`{os}/{go-style-arch}` — Rust's own arch names are translated to the
//! `GOARCH` spelling real clients expect, e.g. `x86_64` -> `amd64`).

use serde_json::{json, Value};

const VENDOR_REF: &str = include_str!("../../vendor/REF");

fn vendored_release() -> (&'static str, &'static str) {
    let line = VENDOR_REF.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).next_back().unwrap_or("release-0.0");
    let version = line.strip_prefix("release-").unwrap_or(line);
    let mut parts = version.splitn(2, '.');
    (parts.next().unwrap_or("0"), parts.next().unwrap_or("0"))
}

fn go_style_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    }
}

pub fn info() -> Value {
    let (major, minor) = vendored_release();
    json!({
        "major": major,
        "minor": minor,
        "gitVersion": format!("v{major}.{minor}.0+notk8s"),
        "gitCommit": env!("NODEAPISERVER_GIT_COMMIT"),
        "gitTreeState": env!("NODEAPISERVER_GIT_TREE_STATE"),
        "buildDate": env!("NODEAPISERVER_BUILD_DATE"),
        "goVersion": env!("NODEAPISERVER_RUSTC_VERSION"),
        "compiler": "rustc",
        "platform": format!("{}/{}", std::env::consts::OS, go_style_arch()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn major_minor_come_from_the_vendored_release_ref() {
        let (major, minor) = vendored_release();
        // vendor/REF is release-1.34 as of this crate's current vendoring —
        // if that's ever re-vendored to a new release this assertion (and
        // this module's own doc comment) should be updated alongside it.
        assert_eq!((major, minor), ("1", "34"));
    }

    #[test]
    fn info_has_the_real_upstream_field_set() {
        let v = info();
        for field in ["major", "minor", "gitVersion", "gitCommit", "gitTreeState", "buildDate", "goVersion", "compiler", "platform"] {
            assert!(v.get(field).is_some(), "missing field {field}");
        }
    }

    #[test]
    fn git_version_is_valid_semver_with_a_distinguishing_build_suffix() {
        let v = info();
        let git_version = v["gitVersion"].as_str().unwrap();
        assert!(git_version.starts_with("v1.34."));
        assert!(git_version.ends_with("+notk8s"), "must be distinguishable from a stock kube-apiserver release");
    }

    #[test]
    fn platform_uses_go_style_arch_names() {
        let v = info();
        let platform = v["platform"].as_str().unwrap();
        // Whatever this test binary's own arch is, it must never appear
        // in Rust's own spelling (e.g. "x86_64") — only Go's.
        assert!(!platform.contains("x86_64"), "got {platform:?}, expected the amd64 spelling");
    }
}
