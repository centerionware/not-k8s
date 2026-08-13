//! nodestore — the datastore for not-k8s: the etcd v3 gRPC API over sqlite,
//! event-driven instead of polled. See `lib.rs` for what it is and why.
//!
//! This file is only the process boundary (logging, tokio, exit code), so the
//! combined `notk8s` binary runs the identical code path — same reasoning as
//! nodelet's and nodeproxy's own `main.rs`.

use anyhow::Result;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_target(false))
        .init();

    // Returned, not swallowed: an error here means the datastore never came
    // up, and a control plane whose store is missing needs to see why in the
    // logs rather than a bare non-zero exit. (nodeproxy learned this the hard
    // way — see its main.rs.)
    nodestore::run().await
}
