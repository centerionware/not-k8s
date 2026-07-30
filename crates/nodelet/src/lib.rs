//! nodelet — a lean, event-driven Kubernetes node agent for single-device / edge use.
//!
//! Library surface so integration tests / examples (e.g. the containerd smoke
//! test) can drive the same code the binary runs. See `main.rs` for the agent.

pub mod config;
pub mod eviction;
pub mod gc;
pub mod metrics;
pub mod node;
pub mod pods;
pub mod probes;
pub mod runtime;
pub mod svc;
