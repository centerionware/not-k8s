//! User namespace allocation for `spec.hostUsers: false` (round 25).
//!
//! CRI's `LinuxSandboxSecurityContext.namespace_options.userns_options`
//! wants a `POD`-mode `UserNamespace` carrying UID/GID `IDMapping`s that
//! remap the container's whole 0-65535 ID space into an **exclusive**
//! host range — no two pods on the node may share a host UID/GID, or the
//! isolation the feature exists for is defeated. This is the same "give
//! this pod its own slice of host ID space" approach real kubelet's own
//! `usernsManager` takes (`pkg/kubelet/kuberuntime/kuberuntime_sandbox.go`),
//! simplified here to a single fixed-length allocator (every pod gets the
//! same size range) rather than upstream's variable-length pool.
//!
//! Opt-in per pod, not a node-wide policy toggle like CPU/Memory/Topology
//! Manager: `spec.hostUsers` unset or `true` (the default, matching
//! upstream) means no user namespace at all — this allocator is never
//! consulted, containers run in the host's own UID/GID space exactly as
//! before this round. Only `hostUsers: false` triggers an allocation.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

/// On-disk checkpoint dir for allocated ranges (round 124) — same
/// memory-first-disk-fallback shape as `csi.rs`'s `MountMeta` and
/// `device_plugins.rs`'s `DEVICE_ALLOC_CHECKPOINT_DIR`. Without this,
/// `claims` starts empty on every nodelet restart, but `run_sandbox()`
/// (the only `allocate()` call site) is skipped for a pod whose sandbox
/// already exists and is just being reused — so a survived pod's
/// mapping was never re-derived. `assigned()` (read by both
/// `container_create.rs`, for any container the reused sandbox still
/// needs to *create* post-restart — a crash-loop restart, say — and
/// `volumes_resolve.rs`, for chowning re-materialized volumes to the
/// userns base) would then silently return `None`: a new container in
/// an already-namespaced pod would come up in the *host* ID space
/// instead of its isolated range (a real isolation regression, not just
/// a bookkeeping one), and volume ownership would get chowned wrong.
const CHECKPOINT_DIR: &str = "/var/lib/nodelet/userns/allocations";

pub struct UsernsAllocator {
    base_uid: u32,
    length: u32,
    max_slots: u32,
    claims: Mutex<HashMap<String, u32>>,
}

/// Duck-typed on-disk form of one claim — self-contained per the
/// established sidecar-checkpoint convention, no cross-module type
/// sharing.
#[derive(serde::Serialize, serde::Deserialize)]
struct ClaimMeta {
    pod_uid: String,
    slot: u32,
}

fn checkpoint_path(key: &str) -> std::path::PathBuf {
    // Pod UIDs are UUIDs and never contain '/', but sanitize defensively
    // anyway since this becomes a filename.
    std::path::PathBuf::from(CHECKPOINT_DIR).join(format!("{}.json", key.replace('/', "_")))
}

impl UsernsAllocator {
    pub fn new(base_uid: u32, length: u32, max_slots: u32) -> Self {
        Self { base_uid, length, max_slots, claims: Mutex::new(Self::restore_claims(max_slots)) }
    }

    // Unit tests run under `cargo test` (CI runs it as root, via `sudo`,
    // for the CRI-gated half) — real disk I/O against a single hardcoded
    // path here would let one test's checkpoint leak into every other
    // test's "fresh" allocator, since nothing scopes CHECKPOINT_DIR
    // per-test. Production is the only build where this path is real.
    #[cfg(not(test))]
    fn restore_claims(max_slots: u32) -> HashMap<String, u32> {
        let mut claims = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(CHECKPOINT_DIR) {
            for entry in entries.flatten() {
                let Ok(content) = std::fs::read_to_string(entry.path()) else { continue };
                let Ok(meta) = serde_json::from_str::<ClaimMeta>(&content) else { continue };
                if meta.slot < max_slots {
                    claims.insert(meta.pod_uid, meta.slot);
                }
            }
        }
        claims
    }

    #[cfg(test)]
    fn restore_claims(_max_slots: u32) -> HashMap<String, u32> {
        HashMap::new()
    }

    /// Allocate (or return the already-allocated) exclusive
    /// `(host_id_base, length)` range for `key` (a pod uid) — idempotent,
    /// matching every other per-pod resource claim in this codebase (a
    /// reconcile may call this again for a pod whose sandbox already
    /// exists, and must get the same range back, not a second one).
    /// `None` if every slot is already claimed — the caller falls back to
    /// no user namespace for this pod rather than failing it outright,
    /// the same graceful-degradation posture CPU/Memory Manager already
    /// have for their own exhaustion cases.
    pub fn allocate(&self, key: &str) -> Option<(u32, u32)> {
        let mut claims = self.claims.lock().unwrap();
        if let Some(&slot) = claims.get(key) {
            return Some((self.base_uid + slot * self.length, self.length));
        }
        let used: BTreeSet<u32> = claims.values().copied().collect();
        let slot = (0..self.max_slots).find(|s| !used.contains(s))?;
        claims.insert(key.to_string(), slot);
        #[cfg(not(test))]
        if let Err(e) = std::fs::create_dir_all(CHECKPOINT_DIR).and_then(|_| {
            std::fs::write(checkpoint_path(key), serde_json::to_vec(&ClaimMeta { pod_uid: key.to_string(), slot }).unwrap())
        }) {
            tracing::warn!(pod_uid = key, error = %e, "userns: failed to checkpoint allocation to disk — a nodelet restart before this pod's sandbox is torn down could lose its UID/GID range");
        }
        Some((self.base_uid + slot * self.length, self.length))
    }

    /// Give back `key`'s range — call on pod removal (or orphan GC) so
    /// the slot can be reused by a later pod.
    pub fn release(&self, key: &str) {
        self.claims.lock().unwrap().remove(key);
        #[cfg(not(test))]
        let _ = std::fs::remove_file(checkpoint_path(key));
    }

    /// `key`'s currently-allocated `(host_id_base, length)` range, if
    /// any — a read-only lookup for call sites (round 88's `Mount.uidMappings`/
    /// `.gidMappings` wiring) that need the same range `run_sandbox()`
    /// already allocated, without re-deriving or re-triggering allocation
    /// themselves.
    pub fn assigned(&self, key: &str) -> Option<(u32, u32)> {
        let claims = self.claims.lock().unwrap();
        let &slot = claims.get(key)?;
        Some((self.base_uid + slot * self.length, self.length))
    }

    #[cfg(test)]
    fn claimed_count(&self) -> usize {
        self.claims.lock().unwrap().len()
    }
}

#[cfg(test)]
#[path = "userns_tests/allocation.rs"]
mod tests_allocation;
