//! Per-component skip/enable predicates, mirroring
//! `deploy/lib/components.sh`'s `NOTK8S_COMPONENTS` table.
//!
//! Kept as a distinct module rather than folded into `config.rs` so the
//! Rust-side table has one obvious place to add a row when a component is
//! added, same discipline `components.sh`'s own header comment asks for.
//! **Do not let this drift from `deploy/lib/components.sh`** while both
//! exist side by side (Phase 1 replaces the shell script's *bootstrap*
//! callers, but the table itself may need to stay in sync until the shell
//! side is deleted) -- see `docs/NODEBOOTSTRAP_PLAN.md`'s "Phasing" section.

/// One row of `deploy/lib/components.sh`'s `NOTK8S_COMPONENTS` table.
pub struct ComponentSpec {
    /// Installed binary name / combined-binary applet name (must match).
    pub name: &'static str,
    /// `-p` argument for a split build.
    pub cargo_package: &'static str,
    /// Env var carrying an already-built binary, if the caller supplied one
    /// (the prebuilt seam CI's e2e shards and the build-in-CI/test-on-device
    /// loop use -- see `CLAUDE.md`).
    pub prebuilt_env: &'static str,
    /// Whether this component's build needs `protoc` on PATH.
    pub needs_protoc: bool,
}

pub const COMPONENTS: &[ComponentSpec] = &[
    ComponentSpec {
        name: "nodelet",
        cargo_package: "nodelet",
        prebuilt_env: "NOTK8S_NODELET_PREBUILT",
        needs_protoc: true, // only under --features cri; see components.sh's own caveat
    },
    ComponentSpec {
        name: "nodeproxy",
        cargo_package: "nodeproxy",
        prebuilt_env: "NOTK8S_NODEPROXY_PREBUILT",
        needs_protoc: false,
    },
    ComponentSpec {
        name: "nodestore",
        cargo_package: "nodestore",
        prebuilt_env: "NOTK8S_NODESTORE_PREBUILT",
        needs_protoc: true,
    },
    ComponentSpec {
        name: "nodescheduler",
        cargo_package: "nodescheduler",
        prebuilt_env: "NOTK8S_NODESCHEDULER_PREBUILT",
        needs_protoc: false,
    },
    ComponentSpec {
        name: "nodecontroller",
        cargo_package: "nodecontroller",
        prebuilt_env: "NOTK8S_NODECONTROLLER_PREBUILT",
        needs_protoc: false,
    },
    // nodeapiserver is added here once it's wired into components.sh itself
    // (APISERVER.md notes components.sh:6 / measure.sh:98 already name it
    // in anticipation) -- and nodebootstrap when the nodeapiserver target
    // (targets.rs) lands on the nodeapiserver branch.
];
