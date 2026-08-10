//! notk8s — the combined, single-binary build of the not-k8s node components.
//!
//! # Why this exists
//!
//! The components are separate crates and separate services on purpose (see
//! CLAUDE.md and docs/ARCHITECTURE.md): a node can run `nodelet` with a real
//! kube-proxy, or `nodeproxy` with something else as the node agent, or
//! neither. That independence is a property of the *crates*, and it costs
//! nothing to keep — but shipping it as one executable per component does
//! cost something real: each binary re-links the whole shared dependency
//! tree (tokio, kube, k8s-openapi, rustls), which on aarch64 measured as
//! ~11MB + ~6MB for a pair that was ~12MB as a single binary before the
//! split.
//!
//! So the build system produces both layouts (`NOTK8S_BUILD_LAYOUT`):
//!
//!   * **split** — one binary per component. Full modularity: install only
//!     what the node runs, upgrade one without touching the other.
//!   * **combined** — this binary, a busybox-style multi-call executable
//!     that *is* every component. One copy of the shared dependencies, so
//!     it costs roughly one component's size for all of them.
//!
//! Neither is "the" build. A device tight on flash takes the combined one; a
//! node mixing not-k8s components with upstream ones takes the split.
//!
//! # How dispatch works
//!
//! Same convention busybox/toybox use, in this order:
//!
//!   1. **argv[0]** — invoked as `nodelet` (typically a symlink or hardlink
//!      to this binary), it *is* nodelet. This is what makes the combined
//!      layout a drop-in: the service units, `run-nodelet.sh`, and every
//!      test still exec `bin/nodelet` and neither know nor care.
//!   2. **First argument** — `notk8s nodelet` runs the same thing, for when
//!      there's no symlink to invoke through.
//!
//! Remaining arguments are passed through untouched, and every component
//! reads its configuration from the environment exactly as it does when
//! built standalone. There is no combined-binary-only configuration and no
//! behavioural difference between the two layouts — if one ever appears,
//! it's a bug in this file, not a feature.

use anyhow::{bail, Result};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Every component this binary was built with. The `cfg`s are the only
/// place the applet list and the cargo features have to agree; adding a
/// component is one entry here plus one optional dependency in Cargo.toml.
const APPLETS: &[Applet] = &[
    #[cfg(feature = "nodelet")]
    Applet { name: "nodelet", summary: "node agent (kubelet's job): pods, probes, volumes, node status" },
    #[cfg(feature = "nodeproxy")]
    Applet { name: "nodeproxy", summary: "Service proxy (kube-proxy's job): ClusterIP/NodePort via nftables" },
];

struct Applet {
    name: &'static str,
    summary: &'static str,
}

#[tokio::main]
async fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    let arg0 = argv
        .first()
        .map(|a| a.rsplit('/').next().unwrap_or(a).to_string())
        .unwrap_or_default();

    // argv[0] first (symlink/hardlink install), then an explicit subcommand.
    let applet = if is_applet(&arg0) {
        arg0.clone()
    } else {
        match argv.get(1).map(String::as_str) {
            Some(name) if is_applet(name) => name.to_string(),
            Some("components") => {
                // Machine-readable: one component per line, so the build
                // system and e2e tests can assert what a binary actually
                // contains instead of inferring it from how it was built.
                for a in APPLETS {
                    println!("{}", a.name);
                }
                return Ok(());
            }
            Some("--version" | "-V") => {
                println!("notk8s {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            Some("--help" | "-h") | None => {
                print_usage();
                return Ok(());
            }
            Some(other) => {
                print_usage();
                bail!("unknown component '{other}'");
            }
        }
    };

    // Logging is set up here, not in the components, for the same reason
    // each standalone `main.rs` does it: exactly one process-wide
    // subscriber, installed before anything logs.
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_target(false))
        .init();

    run_applet(&applet).await
}

fn is_applet(name: &str) -> bool {
    APPLETS.iter().any(|a| a.name == name)
}

async fn run_applet(name: &str) -> Result<()> {
    match name {
        #[cfg(feature = "nodelet")]
        "nodelet" => nodelet::app::run().await,
        #[cfg(feature = "nodeproxy")]
        "nodeproxy" => nodeproxy::run().await,
        // Unreachable via main() — is_applet() gates every path here — but
        // cheaper to answer honestly than to unwrap.
        other => bail!("component '{other}' was not built into this binary (see `notk8s components`)"),
    }
}

fn print_usage() {
    println!("notk8s {} — combined build of the not-k8s node components", env!("CARGO_PKG_VERSION"));
    println!();
    println!("Usage:");
    println!("  notk8s <component> [args...]   run a component");
    println!("  <component> [args...]          same, when invoked through a symlink named for it");
    println!("  notk8s components              list components in this binary, one per line");
    println!();
    if APPLETS.is_empty() {
        println!("This binary was built with no components at all (--no-default-features");
        println!("without naming any), which makes it useless — rebuild with at least one.");
        return;
    }
    println!("Components in this binary:");
    let width = APPLETS.iter().map(|a| a.name.len()).max().unwrap_or(0);
    for a in APPLETS {
        println!("  {:width$}  {}", a.name, a.summary, width = width);
    }
    println!();
    println!("Configuration is per-component and read from the environment, identical to");
    println!("the separate per-component binaries. See docs/ARCHITECTURE.md.");
}

#[cfg(test)]
mod tests {
    use super::*;

    // The dispatch table is the whole crate; if it and the cargo features
    // ever disagree, the binary silently loses a component.
    #[test]
    fn default_build_contains_every_component() {
        #[cfg(feature = "nodelet")]
        assert!(is_applet("nodelet"));
        #[cfg(feature = "nodeproxy")]
        assert!(is_applet("nodeproxy"));
    }

    #[test]
    fn applet_names_are_unique() {
        let mut names: Vec<&str> = APPLETS.iter().map(|a| a.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate applet name in APPLETS");
    }

    #[test]
    fn unknown_names_are_not_applets() {
        assert!(!is_applet("components"));
        assert!(!is_applet("notk8s"));
        assert!(!is_applet(""));
    }
}
