//! nodeapiserver — kube-apiserver's job, done for not-k8s.
//!
//! The last k3s component: REST + watch over every built-in and CRD
//! resource, backed by `nodestore`. See `docs/APISERVER.md` for the design
//! and the `crate`-level doc comment in `lib.rs` for the module map.
//!
//! Everything below the process boundary lives in `lib.rs`, so the combined
//! `notk8s` binary can run exactly this code path without a second copy of
//! the dependency tree.

use anyhow::Result;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_target(false))
        .init();

    // Returning the error (rather than exiting on it) is what gets it
    // printed — see nodeproxy's main.rs for the incident that established
    // this rule across every component's main.rs in this workspace.
    nodeapiserver::run().await
}
