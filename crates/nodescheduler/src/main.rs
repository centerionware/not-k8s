//! nodescheduler — pod placement for not-k8s.
//!
//! kube-scheduler's job: watch for pods nobody has placed yet, pick a node
//! for each, and write the `Binding`. Pairs with `nodelet` (which then runs
//! the pod) and depends on nothing in it — the two processes share only the
//! apiserver they both watch.
//!
//! Replaceable on purpose, exactly like `nodeproxy`: a cluster that wants
//! the real kube-scheduler, or a custom scheduler, simply doesn't run this
//! binary. `deploy/bootstrap-source.sh --scheduler=none` is that path, and
//! it leaves k3s's own bundled kube-scheduler enabled.
//!
//! Needs only an apiserver — no root, no kernel features, no runtime.
//! See `cycle.rs` for the scheduling cycle's invariants, and `queue/` for
//! the event-driven queue that is this component's reason to exist.
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

    // Runs forever; only returns on a condition that makes the whole process
    // pointless (an unreachable apiserver, an unparseable config). Returning
    // the error rather than exiting on it is what gets it *printed* — see
    // nodeproxy's main.rs for the incident that established this rule.
    nodescheduler::run().await
}
