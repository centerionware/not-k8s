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

pub struct UsernsAllocator {
    base_uid: u32,
    length: u32,
    max_slots: u32,
    claims: Mutex<HashMap<String, u32>>,
}

impl UsernsAllocator {
    pub fn new(base_uid: u32, length: u32, max_slots: u32) -> Self {
        Self { base_uid, length, max_slots, claims: Mutex::new(HashMap::new()) }
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
        Some((self.base_uid + slot * self.length, self.length))
    }

    /// Give back `key`'s range — call on pod removal (or orphan GC) so
    /// the slot can be reused by a later pod.
    pub fn release(&self, key: &str) {
        self.claims.lock().unwrap().remove(key);
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
