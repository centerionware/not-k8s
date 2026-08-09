//! Configuration for `nodeproxy`, resolved from environment variables.
//!
//! Deliberately tiny — the proxy needs a kubeconfig (via `KUBECONFIG`, read
//! by `kube::Client::try_default()` and not by this module), an address
//! family, and a load-balancing method. Everything has a default, so
//! `nodeproxy` runs with no configuration at all.
//!
//! ## Legacy variable names
//!
//! These settings used to live in `nodelet`'s own `Config` as
//! `NODELET_IP_FAMILY` / `NODELET_LB_METHOD`, back when the Service proxy ran
//! inside the nodelet process. An in-place upgrade of an existing install can
//! still have those set in a systemd unit or shell profile, so they're
//! accepted as a fallback rather than ignored — a node whose service routing
//! silently changed behaviour on upgrade would be a miserable thing to debug.

use anyhow::Result;
use tracing::debug;

/// Which address family(ies) the Service proxy programs rules for. Defaults
/// to whatever the node actually has: both stacks if both work, otherwise
/// whichever one does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpFamily {
    V4,
    V6,
    Dual,
}

/// Load-balancing algorithm for Services without `sessionAffinity: ClientIP`
/// set (that field always forces source-hash — see `svc.rs::lb_expr`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LbMethod {
    Random,
    RoundRobin,
    SourceHash,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub ip_family: IpFamily,
    pub lb_method: LbMethod,
}

/// Read `NODEPROXY_<name>`, falling back to the pre-split `NODELET_<name>`.
fn var(name: &str) -> Option<String> {
    if let Ok(v) = std::env::var(format!("NODEPROXY_{name}")) {
        return Some(v);
    }
    match std::env::var(format!("NODELET_{name}")) {
        Ok(v) => {
            debug!("using legacy NODELET_{name} (prefer NODEPROXY_{name})");
            Some(v)
        }
        Err(_) => None,
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let ip_family = match var("IP_FAMILY").as_deref() {
            Some("ipv4") => IpFamily::V4,
            Some("ipv6") => IpFamily::V6,
            Some("dual") => IpFamily::Dual,
            Some("auto") | None => detect_ip_family(),
            Some(other) => anyhow::bail!(
                "unknown NODEPROXY_IP_FAMILY '{other}' (want 'auto', 'ipv4', 'ipv6', or 'dual')"
            ),
        };

        let lb_method = match var("LB_METHOD").as_deref() {
            Some("random") | None => LbMethod::Random,
            Some("round-robin") => LbMethod::RoundRobin,
            Some("source-hash") => LbMethod::SourceHash,
            Some(other) => anyhow::bail!(
                "unknown NODEPROXY_LB_METHOD '{other}' (want 'random', 'round-robin', or 'source-hash')"
            ),
        };

        Ok(Self { ip_family, lb_method })
    }
}

/// Whether this host has an actual route for the given family — not just a
/// working socket API for it. A bare `bind()` only proves the kernel has
/// that address family compiled in and enabled, which is true on nearly
/// every modern Linux kernel regardless of whether there's any real
/// connectivity; that produced a real false positive (a machine with IPv6
/// support but no default v6 route was detected as dual-stack, and the
/// flannel CNI daemon this feeds into crash-looped forever trying to find a
/// v6 interface that didn't exist). `connect()` on a UDP socket sends no
/// packets — it's a local routing-table lookup for the given destination —
/// so this works fully offline and doesn't need the probe address itself to
/// be reachable, only *routable*.
fn has_route(probe_addr: &str, bind_addr: &str) -> bool {
    let Ok(sock) = std::net::UdpSocket::bind(bind_addr) else { return false };
    sock.connect(probe_addr).is_ok()
}

fn detect_ip_family() -> IpFamily {
    let v4 = has_route("8.8.8.8:53", "0.0.0.0:0");
    let v6 = has_route("[2001:4860:4860::8888]:53", "[::]:0");
    match (v4, v6) {
        (true, true) => IpFamily::Dual,
        (true, false) => IpFamily::V4,
        (false, true) => IpFamily::V6,
        (false, false) => IpFamily::V4, // shouldn't happen; fall back to the common case
    }
}
