//! nodelet — a lean, event-driven Kubernetes node agent for single-device / edge use.
//!
//! Pairs with a stripped real control plane (`k3s server --disable-agent`) so the
//! device speaks 1:1 kubectl/CRD Kubernetes while shedding the kubelet's idle cost
//! (PLEG polling, cAdvisor housekeeping). See docs/ARCHITECTURE.md.
//!
//! The agent itself is `nodelet::app` — this file is only the process
//! boundary (logging setup, tokio runtime, exit code), so the combined
//! `notk8s` binary can run the same agent without duplicating it.

use anyhow::Result;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_target(false))
        .init();

    nodelet::app::run().await
}
