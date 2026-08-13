//! The cluster state a scheduling decision is made from.
//!
//! Three concerns, deliberately separate:
//!
//!   * [`pod`] / [`node`] — **projections**. What a scheduler needs to know
//!     about a Pod or a Node, and nothing else. Storing these instead of whole
//!     API objects is the memory half of this component's footprint claim, and
//!     it matters more as the cluster grows.
//!   * [`snapshot`] — the [`Cache`] (authoritative, mutated by the watch
//!     layer) and the [`Snapshot`] (immutable, one per scheduling cycle,
//!     refreshed incrementally). The incremental refresh is the CPU half.
//!   * [`assume`] — the optimistic reservations that let the scheduling loop
//!     run ahead of binding without double-booking capacity.
//!
//! Nothing here makes a scheduling *decision*; it only answers questions. The
//! decisions live in `framework/plugins/` and `cycle.rs`.

pub mod assume;
pub mod node;
pub mod pod;
pub mod snapshot;

pub use assume::{Assumed, AssumedPods};
pub use node::{ImageState, NodeInfo};
pub use pod::{
    HostPort, OwnerRef, PodInfo, PreferredTerm, Resources, DEFAULT_MEMORY_REQUEST,
    DEFAULT_MILLI_CPU_REQUEST,
};
pub use snapshot::{Cache, Snapshot};
