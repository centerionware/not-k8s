//! Group G: JSON Patch, JSON Merge Patch, Strategic Merge Patch, and
//! Server-Side Apply / `managedFields`.
//!
//! `json_patch` — RFC 6902, `merge_patch` — RFC 7386, both thin wrappers
//! around the `json-patch` crate (`docs/APISERVER_PLAN.md` finding 8:
//! reused, not hand-written — no crate exists for k8s-specific Strategic
//! Merge Patch, so that piece has to be).
//!
//! Status: in progress (see docs/APISERVER.md). Landed: RFC 6902/7386.
//! **Not yet landed**: Strategic Merge Patch (needs Group A's codegen to
//! also resolve a field's *referenced* schema, not just its own
//! `x-kubernetes-*` flags — real, separate work) and Server-Side Apply/
//! `managedFields` (structured-merge-diff has no Rust crate to reuse
//! either, per finding 8).

pub mod json_patch;
pub mod merge_patch;
