//! nodelet — a lean, event-driven Kubernetes node agent for single-device / edge use.
//!
//! Library surface so integration tests / examples (e.g. the containerd smoke
//! test) — and the combined `notk8s` binary — can drive the same code the
//! binary runs. See `app.rs` for the agent itself.

// `app.rs` moved here verbatim from `main.rs`, where it referred to this
// crate's modules by name (`nodelet::node`, `nodelet::eviction`, ...) as any
// external caller would. This alias keeps those paths resolving from inside
// the crate, so the move stayed a move rather than a rewrite of every call
// site in it.
extern crate self as nodelet;

pub mod app;
pub mod bootstrap;
#[cfg(feature = "cri")]
pub mod cgroup;
pub mod config;
#[cfg(feature = "cri")]
pub mod cpu_manager;
#[cfg(feature = "cri")]
pub mod credential_provider;
#[cfg(feature = "cri")]
pub mod csi_node;
#[cfg(feature = "cri")]
pub mod device_plugins;
#[cfg(feature = "cri")]
pub mod dra;
pub mod eviction;
pub mod gc;
#[cfg(feature = "cri")]
pub mod memory_manager;
pub mod metrics;
pub mod node;
#[cfg(feature = "cri")]
pub mod pod_resources;
pub mod pods;
#[cfg(feature = "cri")]
pub mod plugin_registry;
pub mod probes;
pub mod runtime;
#[cfg(feature = "cri")]
pub mod server;
#[cfg(feature = "cri")]
pub mod shutdown;
pub mod static_pods;
#[cfg(feature = "cri")]
pub mod topology;
#[cfg(feature = "cri")]
pub mod userns;
