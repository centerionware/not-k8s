//! node-ipam-controller: allocates `Node.spec.podCIDR` out of
//! `--cluster-cidr`/`--node-cidr-mask-size`. Pure event — a Node either has
//! a podCIDR or it doesn't, there's nothing to poll for. flannel is dead in
//! the water without this (`deploy/lib/cni.sh` already documents the
//! dependency on `spec.podCIDR` being set) — see
//! docs/CONTROLLER_MANAGER.md, Group A.
//!
//! IPv4 single-stack only, matching this project's own default
//! (`CLUSTER_CIDR=10.42.0.0/16` in `deploy/setup-control-plane.sh`) and its
//! current CNI setup (flannel, no dual-stack wiring today). Dual-stack is
//! additive later, not a rework of this — the allocator is deliberately one
//! address family at a time rather than baking in an IPv4/IPv6 pair.

use anyhow::{bail, Context, Result};
use k8s_openapi::api::core::v1::Node;
use kube::runtime::watcher::Event;
use kube::{Api, Client, ResourceExt};
use std::collections::HashSet;

/// Parses `"a.b.c.d"` into its 32-bit representation. No external crate for
/// this — the parse is small, exact, and this crate otherwise has no need
/// for a general IP-address library (see Cargo.toml's dependency comment).
fn parse_ipv4(s: &str) -> Option<u32> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut out: u32 = 0;
    for p in parts {
        let octet: u32 = p.parse().ok()?;
        if octet > 255 {
            return None;
        }
        out = (out << 8) | octet;
    }
    Some(out)
}

fn format_ipv4(addr: u32) -> String {
    format!(
        "{}.{}.{}.{}",
        (addr >> 24) & 0xFF,
        (addr >> 16) & 0xFF,
        (addr >> 8) & 0xFF,
        addr & 0xFF
    )
}

pub fn parse_ipv4_cidr(s: &str) -> Result<(u32, u8)> {
    let (addr_str, prefix_str) = s
        .split_once('/')
        .with_context(|| format!("'{s}' is not a CIDR (missing '/')"))?;
    let addr = parse_ipv4(addr_str).with_context(|| format!("'{addr_str}' is not a valid IPv4 address"))?;
    let prefix: u8 = prefix_str
        .parse()
        .with_context(|| format!("'{prefix_str}' is not a valid prefix length"))?;
    if prefix > 32 {
        bail!("'{s}': prefix length {prefix} is greater than 32");
    }
    Ok((addr, prefix))
}

/// Allocates fixed-size `/node_prefix` blocks out of one `/base_prefix`
/// cluster range — the same shape as upstream's `cidrset.CidrSet`, minus
/// dual-stack and minus the bitmap-of-a-billion-bits optimization upstream
/// needs for very large clusters (a `HashSet<u32>` of allocated block
/// indices is plenty for the cluster sizes this project targets, and is far
/// simpler to get right).
pub struct CidrAllocator {
    base: u32,
    node_prefix: u8,
    total_blocks: u32,
    allocated: HashSet<u32>,
}

impl CidrAllocator {
    pub fn new(cluster_cidr: &str, node_prefix: u8) -> Result<Self> {
        let (base, base_prefix) = parse_ipv4_cidr(cluster_cidr)
            .with_context(|| format!("parsing cluster CIDR '{cluster_cidr}'"))?;
        if node_prefix < base_prefix {
            bail!(
                "node CIDR mask size (/{node_prefix}) must be >= the cluster CIDR's own prefix \
                 (/{base_prefix}) — a per-node block can't be larger than the range it comes from."
            );
        }
        let total_blocks = 1u64
            .checked_shl((node_prefix - base_prefix) as u32)
            .unwrap_or(u64::MAX)
            .min(u32::MAX as u64) as u32;
        Ok(CidrAllocator { base, node_prefix, total_blocks, allocated: HashSet::new() })
    }

    fn block_size(&self) -> u32 {
        if self.node_prefix >= 32 {
            1
        } else {
            1u32 << (32 - self.node_prefix)
        }
    }

    fn index_of(&self, addr: u32) -> u32 {
        addr.wrapping_sub(self.base) / self.block_size()
    }

    fn address_of(&self, index: u32) -> u32 {
        self.base.wrapping_add(index.wrapping_mul(self.block_size()))
    }

    /// Record `subnet` as already taken, without allocating a new one. Used
    /// while consuming the shared watch's initial Node snapshot — this
    /// controller keeps no state of its own across a restart; the Node
    /// objects themselves are the durable record, the same "the leader
    /// resolves state from what's actually there" rule `nodestore::command`
    /// states for its own determinism.
    pub fn mark_allocated(&mut self, subnet: &str) -> Result<()> {
        let (addr, prefix) = parse_ipv4_cidr(subnet)?;
        if prefix != self.node_prefix {
            // A hand-assigned podCIDR outside the configured mask size isn't
            // this allocator's business — ignored, not an error, so one odd
            // Node doesn't stop every other Node from being allocated.
            return Ok(());
        }
        self.allocated.insert(self.index_of(addr));
        Ok(())
    }

    pub fn release(&mut self, subnet: &str) -> Result<()> {
        let (addr, prefix) = parse_ipv4_cidr(subnet)?;
        if prefix == self.node_prefix {
            self.allocated.remove(&self.index_of(addr));
        }
        Ok(())
    }

    /// The next free block, or `None` if the cluster CIDR is exhausted.
    pub fn allocate(&mut self) -> Option<String> {
        for idx in 0..self.total_blocks {
            if !self.allocated.contains(&idx) {
                self.allocated.insert(idx);
                return Some(format!("{}/{}", format_ipv4(self.address_of(idx)), self.node_prefix));
            }
        }
        None
    }
}

async fn reconcile_node(api: &Api<Node>, allocator: &mut CidrAllocator, node: &Node) {
    let name = node.name_any();
    let already_has_one = node.spec.as_ref().and_then(|s| s.pod_cidr.as_ref()).is_some();
    if already_has_one {
        return;
    }
    let Some(cidr) = allocator.allocate() else {
        tracing::error!(node = %name, "cluster CIDR exhausted — no podCIDR block left to allocate");
        return;
    };
    tracing::info!(node = %name, pod_cidr = %cidr, "allocating podCIDR");
    let patch = serde_json::json!({ "spec": { "podCIDR": cidr, "podCIDRs": [cidr] } });
    if let Err(e) = api
        .patch(&name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(&patch))
        .await
    {
        tracing::warn!(node = %name, error = ?e, "failed to patch podCIDR onto Node — will retry on the next event");
        // Give the block back rather than leaking it — the next Apply event
        // for this same Node (or the next relist) will retry the patch and
        // needs a free block to try again with.
        let _ = allocator.release(&cidr);
    }
}

pub async fn run(client: Client, cfg: &crate::config::Config) -> Result<()> {
    let api: Api<Node> = Api::all(client.clone());
    let mut allocator = CidrAllocator::new(&cfg.cluster_cidr, cfg.node_cidr_mask_size)?;

    let mut stream = crate::watch::watch_nodes(&client);
    use futures::StreamExt;
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(Event::Apply(node)) | Ok(Event::InitApply(node)) => {
                if let Some(cidr) = node.spec.as_ref().and_then(|s| s.pod_cidr.as_ref()) {
                    if let Err(e) = allocator.mark_allocated(cidr) {
                        tracing::warn!(node = %node.name_any(), pod_cidr = %cidr, error = ?e, "couldn't parse an existing Node's podCIDR while initializing the CIDR allocator");
                    }
                }
                reconcile_node(&api, &mut allocator, &node).await;
            }
            Ok(Event::Delete(node)) => {
                if let Some(cidr) = node.spec.as_ref().and_then(|s| s.pod_cidr.clone()) {
                    let _ = allocator.release(&cidr);
                }
            }
            Ok(Event::Init | Event::InitDone) => {}
            Err(e) => tracing::warn!(error = ?e, "node watch error in node-ipam-controller"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_cidr() {
        assert_eq!(parse_ipv4_cidr("10.42.0.0/16").unwrap(), (0x0A2A0000, 16));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_ipv4_cidr("not-a-cidr").is_err());
        assert!(parse_ipv4_cidr("10.42.0.0").is_err());
        assert!(parse_ipv4_cidr("10.42.0.0/99").is_err());
        assert!(parse_ipv4_cidr("999.1.1.1/24").is_err());
    }

    #[test]
    fn allocates_sequential_blocks_from_a_16_slash_24() {
        let mut a = CidrAllocator::new("10.42.0.0/16", 24).unwrap();
        assert_eq!(a.allocate().unwrap(), "10.42.0.0/24");
        assert_eq!(a.allocate().unwrap(), "10.42.1.0/24");
        assert_eq!(a.allocate().unwrap(), "10.42.2.0/24");
    }

    #[test]
    fn seeding_with_an_existing_allocation_skips_it_on_the_next_allocate() {
        let mut a = CidrAllocator::new("10.42.0.0/16", 24).unwrap();
        a.mark_allocated("10.42.0.0/24").unwrap();
        assert_eq!(a.allocate().unwrap(), "10.42.1.0/24");
    }

    #[test]
    fn releasing_a_block_makes_it_allocatable_again() {
        let mut a = CidrAllocator::new("10.42.0.0/16", 24).unwrap();
        let first = a.allocate().unwrap();
        a.release(&first).unwrap();
        assert_eq!(a.allocate().unwrap(), first);
    }

    #[test]
    fn exhausting_a_tiny_range_returns_none_instead_of_panicking() {
        let mut a = CidrAllocator::new("10.42.0.0/24", 24).unwrap(); // exactly one block
        assert_eq!(a.allocate().unwrap(), "10.42.0.0/24");
        assert!(a.allocate().is_none());
    }

    #[test]
    fn a_node_mask_smaller_than_the_cluster_prefix_is_rejected() {
        assert!(CidrAllocator::new("10.42.0.0/24", 16).is_err());
    }

    #[test]
    fn an_out_of_family_existing_podcidr_is_ignored_not_an_error() {
        let mut a = CidrAllocator::new("10.42.0.0/16", 24).unwrap();
        // Different prefix than this allocator's node_prefix — a hand-set
        // outlier, ignored rather than corrupting this allocator's index math.
        assert!(a.mark_allocated("10.42.0.0/28").is_ok());
        assert_eq!(a.allocate().unwrap(), "10.42.0.0/24");
    }
}
