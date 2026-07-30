//! Garbage collection: orphaned sandbox/container cleanup and unreferenced
//! image cleanup. CRI-only — the mock runtime has nothing real to collect,
//! so `PodRuntime::gc()` defaults to a no-op there (see `runtime/mod.rs`).
//!
//! Runs on its own coarse interval (default 300s, `NODELET_GC_INTERVAL_SECS`)
//! — not a per-second poll, so it doesn't touch the idle-cost story the rest
//! of nodelet is built around.
//!
//! Two things accumulate over time that nothing else in nodelet cleans up:
//!   1. Orphaned sandboxes/containers — a Pod deleted from the apiserver
//!      while nodelet was down (or missed the delete watch event somehow)
//!      never gets `remove_pod()` called for it, so its CRI-level sandbox
//!      and containers just sit there forever.
//!   2. Unreferenced images — every image ever pulled stays on disk; a
//!      workload that churns through many image versions slowly fills the
//!      node's storage.

use std::collections::HashSet;

/// Which of the CRI-known, nodelet-managed sandboxes have no matching Pod
/// bound to this node in the apiserver anymore. Pure so it's unit-testable
/// without a CRI socket. `cri_sandboxes` is `(namespace, name, sandbox_id)`;
/// `live_pod_keys` is `namespace/name` for every Pod currently bound to this
/// node per the apiserver.
pub fn orphaned_sandboxes(
    cri_sandboxes: &[(String, String, String)],
    live_pod_keys: &HashSet<String>,
) -> Vec<String> {
    cri_sandboxes
        .iter()
        .filter(|(ns, name, _id)| !live_pod_keys.contains(&crate::runtime::pod_key(ns, name)))
        .map(|(_, _, id)| id.clone())
        .collect()
}

/// Minimal shape of a CRI `Image` needed to decide referenced-ness — kept
/// separate from the generated `v1::Image` proto type so this stays
/// testable without the `cri` feature / a real CRI socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRef {
    pub id: String,
    pub repo_tags: Vec<String>,
    pub repo_digests: Vec<String>,
}

/// Which images aren't referenced (by id, repo tag, or repo digest) by any
/// container currently on the node, and should be removed. `referenced` is
/// whatever string each existing container's `ImageSpec.image` holds —
/// containerd doesn't guarantee that matches `Image.id` (it may be the
/// human-given ref like `busybox:latest` instead of the resolved digest), so
/// this checks all three identities before calling an image unreferenced.
pub fn images_to_gc(images: &[ImageRef], referenced: &HashSet<String>) -> Vec<String> {
    images
        .iter()
        .filter(|img| {
            !referenced.contains(&img.id)
                && !img.repo_tags.iter().any(|t| referenced.contains(t))
                && !img.repo_digests.iter().any(|d| referenced.contains(d))
        })
        .map(|img| img.id.clone())
        .collect()
}

#[cfg(test)]
#[path = "gc_tests/orphaned_sandboxes.rs"]
mod tests_orphaned_sandboxes;
#[cfg(test)]
#[path = "gc_tests/images_to_gc.rs"]
mod tests_images_to_gc;
