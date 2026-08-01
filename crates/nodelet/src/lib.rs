//! nodelet — a lean, event-driven Kubernetes node agent for single-device / edge use.
//!
//! Library surface so integration tests / examples (e.g. the containerd smoke
//! test) can drive the same code the binary runs. See `main.rs` for the agent.

pub mod bootstrap;
#[cfg(feature = "cri")]
pub mod cgroup;
pub mod config;
#[cfg(feature = "cri")]
pub mod cpu_manager;
#[cfg(feature = "cri")]
pub mod credential_provider;
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
pub mod svc;
#[cfg(feature = "cri")]
pub mod topology;
#[cfg(feature = "cri")]
pub mod userns;
