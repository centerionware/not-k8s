//! Standalone `nodebootstrap` entrypoint. The combined `notk8s` binary calls
//! `nodebootstrap::run_embedded` so this file remains a thin wrapper around
//! the same CLI and orchestration code.

use anyhow::Result;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_target(false))
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    nodebootstrap::run_args(&args)
}
