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

/// Minimal shape of a CRI `Image` needed to decide referenced-ness and
/// (round 70) how much space removing it would free — kept separate from
/// the generated `v1::Image` proto type so this stays testable without
/// the `cri` feature / a real CRI socket.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ImageRef {
    pub id: String,
    pub repo_tags: Vec<String>,
    pub repo_digests: Vec<String>,
    pub size_bytes: u64,
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

/// `disk_path`'s used-space percentage, rounded to the nearest whole
/// percent — the same measurement `DiskPressure` already makes, reused
/// here as the input to real kubelet's image-GC watermark policy (round
/// 70; found in round 69's fresh gap re-audit). `0` for a `total_bytes`
/// of `0` (can't compute a percentage of nothing) rather than a NaN/panic.
pub fn disk_usage_percent(total_bytes: u64, available_bytes: u64) -> u8 {
    if total_bytes == 0 {
        return 0;
    }
    let used = total_bytes.saturating_sub(available_bytes);
    (((used as f64 / total_bytes as f64) * 100.0).round() as u64).min(100) as u8
}

/// Real kubelet's actual image-GC trigger: unreferenced images are left
/// alone entirely — regardless of how long they've sat unused — until
/// disk usage crosses `--image-gc-high-threshold` (default 85%). This is
/// what makes image GC purely reactive to real pressure rather than an
/// opportunistic "clean up anything unused" sweep (which is what
/// `images_to_gc()` alone would produce if called on every cycle
/// unconditionally, the pre-round-70 behavior).
pub fn should_start_image_gc(usage_percent: u8, high_threshold_percent: u8) -> bool {
    usage_percent >= high_threshold_percent
}

/// Once triggered, which unreferenced images to actually remove this
/// cycle, and in what order — real kubelet's own policy: only images
/// unreferenced for at least `min_age_secs` are eligible at all
/// (`--image-minimum-gc-age`, avoids evicting something mid-rollout
/// that's about to be reused), removed oldest-unreferenced-first,
/// stopping once simulated usage drops to `low_threshold_percent` or
/// there's nothing left eligible — whichever comes first. `candidates`
/// should already be filtered to unreferenced images (`images_to_gc()`'s
/// output, resolved back to full `ImageRef`s for their `size_bytes`);
/// `unreferenced_since` is `image id -> unix seconds first observed
/// unreferenced` (an image missing from this map — briefly possible
/// right after it *becomes* unreferenced, before the caller's had a
/// chance to record it — is treated as maximally young, i.e. not yet
/// eligible, never as an error). Pure so the ordering/stopping logic is
/// unit-testable without a live disk or CRI socket.
pub fn images_to_reclaim_space(
    candidates: &[ImageRef],
    unreferenced_since: &std::collections::HashMap<String, u64>,
    now_secs: u64,
    min_age_secs: u64,
    total_bytes: u64,
    available_bytes: u64,
    low_threshold_percent: u8,
) -> Vec<String> {
    let mut eligible: Vec<&ImageRef> = candidates
        .iter()
        .filter(|img| unreferenced_since.get(&img.id).is_some_and(|&since| now_secs.saturating_sub(since) >= min_age_secs))
        .collect();
    eligible.sort_by_key(|img| unreferenced_since[&img.id]);

    let mut available = available_bytes;
    let mut out = Vec::new();
    for img in eligible {
        if disk_usage_percent(total_bytes, available) <= low_threshold_percent {
            break;
        }
        out.push(img.id.clone());
        available = available.saturating_add(img.size_bytes);
    }
    out
}

#[cfg(test)]
#[path = "gc_tests/orphaned_sandboxes.rs"]
mod tests_orphaned_sandboxes;
#[cfg(test)]
#[path = "gc_tests/images_to_gc.rs"]
mod tests_images_to_gc;
#[cfg(test)]
#[path = "gc_tests/image_gc_watermark.rs"]
mod tests_image_gc_watermark;
