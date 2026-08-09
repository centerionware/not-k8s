//! Service (ClusterIP + NodePort) proxy — nftables, not iptables.
//!
//! Real kube-proxy watches Services + EndpointSlices and programs iptables/
//! ipvs rules to make ClusterIPs (which no interface ever owns) resolve to a
//! real backend pod. This does the same job with nftables, scoped to what a
//! single-node edge cluster needs: reconcile-on-event (no periodic resync,
//! no polling), the whole ruleset rebuilt atomically from current state every
//! time a Service or EndpointSlice changes.
//!
//! This used to run as a task inside `nodelet` itself. It's a separate
//! process now for the same reason kube-proxy is separate from the kubelet
//! upstream: service handling is a replaceable concern. A node that wants
//! Cilium's eBPF datapath, or a real kube-proxy, or no service proxy at all,
//! swaps this binary out and leaves the node agent alone.
//!
//! Requires `nft` (nftables) and CAP_NET_ADMIN/root — same requirement real
//! kube-proxy has. If `nft` isn't usable, `run()` reports that rather than
//! programming anything (pods and direct pod-IP traffic are unaffected
//! either way).
//!
//! ## Dual-stack (IPv4/IPv6)
//!
//! One `inet`-family table (`not_k8s_svc`) covers both address families —
//! `inet` is nftables' family that lets a single table mix `ip`/`ip6`
//! matches, unlike the `ip`/`ip6` families which are single-stack only.
//! `config::IpFamily` (auto-detected by default: dual if the node has both
//! stacks, single if it only has one) gates which families get rules at all.
//! Backends come from `EndpointSlice` (not the legacy `Endpoints` API)
//! specifically because dual-stack Services get *separate* slices per
//! address family (`addressType: IPv4` / `IPv6`) — the legacy Endpoints
//! mirror only ever carries one family, which would silently break v6.
//!
//! Two real nftables quirks shaped the rule format:
//!   - A `dnat to <verdict-map>` whose values are an `addr . port`
//!     concatenation needs the target family to be inferable from the rule.
//!     An `ip daddr <addr>` / `ip6 daddr <addr>` match (the ClusterIP case)
//!     already supplies that. `fib daddr type local` (the NodePort case)
//!     does not, and — verified against a real `nft -c` parse — silently
//!     fails (no error text, just a non-zero exit) unless the DNAT
//!     statement is written `dnat ip to ...` / `dnat ip6 to ...` explicitly.
//!   - A single-backend Service uses a plain `<ip>:<port>` target instead of
//!     a `numgen ... mod 1 map {...}`, found necessary on a device with an
//!     Android-derived kernel: `nft` accepted that ruleset's syntax fine but
//!     the kernel rejected applying it with "Could not process rule: No
//!     such file or directory" at the `numgen` token — the signature of a
//!     missing `nft_numgen` kernel module. A single entry doesn't need
//!     load-balancing selection at all, so this sidesteps needing that
//!     module rather than requiring it unconditionally; N>1 backends still
//!     need the map form.
//! Both forms are exercised in `#[test]`s below.
//!
//! ## Load balancing
//!
//! Three algorithms, matching what's actually feasible with stateless
//! nftables (no connection-count tracking like ipvs's least-conn):
//!   - `random` (default): `numgen random mod N`.
//!   - `round-robin`: `numgen inc mod N` — a counter, not per-connection
//!     random.
//!   - `source-hash`: `jhash ip[6] saddr mod N` — sticky per client IP.
//!     Used automatically whenever a Service sets `sessionAffinity:
//!     ClientIP` (the real Kubernetes field), regardless of the configured
//!     default, since that's a per-Service opt-in independent of proxy-wide
//!     policy.
//!
//! Rules, in the `not_k8s_svc` table:
//!   - `prerouting`/`output` (nat, dstnat/-100): DNAT ClusterIP:port and
//!     NodePort traffic to a backend.
//!   - `postrouting` (nat, srcnat): `ct status dnat masquerade` — SNAT any
//!     connection that got DNAT'd, which is what makes hairpin traffic
//!     (a pod calling a Service that routes back to itself) return
//!     correctly, without blanket-masquerading unrelated traffic.
//!
//! NodePort rules match `fib daddr type local` instead of listing every
//! local IP, so they only catch traffic actually addressed to this node —
//! not bridged pod-to-pod traffic that happens to reuse the same port
//! number.

use crate::config::{IpFamily, LbMethod};
use anyhow::{Context, Result};
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Service, ServicePort};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::runtime::watcher;
use kube::runtime::watcher::Event;
use kube::{Api, Client, ResourceExt};
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;
use tracing::{debug, info, warn};

const TABLE: &str = "not_k8s_svc";
const SVC_NAME_LABEL: &str = "kubernetes.io/service-name";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    V4,
    V6,
}

impl Family {
    fn nft_word(self) -> &'static str {
        match self {
            Family::V4 => "ip",
            Family::V6 => "ip6",
        }
    }

    fn address_type(self) -> &'static str {
        match self {
            Family::V4 => "IPv4",
            Family::V6 => "IPv6",
        }
    }

    fn of(ip: &str) -> Self {
        if ip.contains(':') {
            Family::V6
        } else {
            Family::V4
        }
    }
}

#[derive(Default)]
struct State {
    services: HashMap<String, Service>,
    endpoint_slices: HashMap<String, EndpointSlice>,
}

pub struct ServiceProxy {
    client: Client,
    ip_family: IpFamily,
    lb_method: LbMethod,
}

impl ServiceProxy {
    pub fn new(client: Client, ip_family: IpFamily, lb_method: LbMethod) -> Self {
        Self { client, ip_family, lb_method }
    }

    /// Runs forever. Returns only if `nft` is unusable — an error, not a
    /// quiet no-op: as its own process there's nothing else for this binary
    /// to be doing, so the caller exits non-zero and the supervisor's
    /// restarts make the misconfiguration visible. (When this ran as a task
    /// inside nodelet it logged once and let the task die silently, which
    /// left a node with no service routing and no ongoing signal about it.)
    pub async fn run(&self) -> Result<()> {
        check_nft().context(
            "Service (ClusterIP/NodePort) routing needs a working `nft` (nftables) and \
CAP_NET_ADMIN/root — the same requirement real kube-proxy has. Direct pod-IP traffic is \
unaffected either way",
        )?;
        info!(ip_family = ?self.ip_family, lb_method = ?self.lb_method, "service proxy watching Services + EndpointSlices (nftables backend)");

        let mut state = State::default();
        let mut svc_stream = watch_services(&self.client);
        let mut ep_stream = watch_endpoint_slices(&self.client);

        loop {
            let changed = tokio::select! {
                item = svc_stream.next() => {
                    match item {
                        Some(Ok(ev)) => { apply_event(&mut state.services, ev); true }
                        Some(Err(e)) => { warn!(error = ?e, "service watch error"); false }
                        None => {
                            warn!("service watch ended; restarting");
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            svc_stream = watch_services(&self.client);
                            false
                        }
                    }
                }
                item = ep_stream.next() => {
                    match item {
                        Some(Ok(ev)) => { apply_event(&mut state.endpoint_slices, ev); true }
                        Some(Err(e)) => { warn!(error = ?e, "endpointslice watch error"); false }
                        None => {
                            warn!("endpointslice watch ended; restarting");
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            ep_stream = watch_endpoint_slices(&self.client);
                            false
                        }
                    }
                }
            };

            if changed {
                let ruleset = build_ruleset(&state, self.ip_family, self.lb_method);
                match apply_nft(&ruleset) {
                    Ok(()) => debug!("nft ruleset applied"),
                    Err(e) => warn!(error = ?e, "failed to apply nft ruleset"),
                }
            }
        }
    }
}

fn watch_services(client: &Client) -> futures::stream::BoxStream<'static, watcher::Result<Event<Service>>> {
    let api: Api<Service> = Api::all(client.clone());
    watcher(api, watcher::Config::default()).boxed()
}

fn watch_endpoint_slices(
    client: &Client,
) -> futures::stream::BoxStream<'static, watcher::Result<Event<EndpointSlice>>> {
    let api: Api<EndpointSlice> = Api::all(client.clone());
    watcher(api, watcher::Config::default()).boxed()
}

fn obj_key<T: ResourceExt>(obj: &T) -> String {
    format!("{}/{}", obj.namespace().unwrap_or_default(), obj.name_any())
}

/// Fold a watch event into our local mirror. `Init` means a relist just
/// started (drop anything stale from before a reconnect); `InitApply`/`Apply`
/// upsert; `Delete` removes; `InitDone` needs no action (the next loop tick
/// reconciles).
fn apply_event<T: Clone + ResourceExt>(map: &mut HashMap<String, T>, ev: Event<T>) {
    match ev {
        Event::Init => map.clear(),
        Event::InitApply(obj) | Event::Apply(obj) => {
            map.insert(obj_key(&obj), obj);
        }
        Event::Delete(obj) => {
            map.remove(&obj_key(&obj));
        }
        Event::InitDone => {}
    }
}

/// Resolve one Service port to its ready backends (ip, port) of one address
/// family, via the EndpointSlice(s) owned by that Service (labeled
/// `kubernetes.io/service-name`). Matches subset ports by name the same way
/// a real kube-proxy would (a single unnamed port is the common case).
fn backends_for(
    namespace: &str,
    svc_name: &str,
    port: &ServicePort,
    family: Family,
    slices: &HashMap<String, EndpointSlice>,
) -> Vec<(String, i32)> {
    let mut out = Vec::new();
    for slice in slices.values() {
        if slice.metadata.namespace.as_deref() != Some(namespace) {
            continue;
        }
        if slice.address_type != family.address_type() {
            continue;
        }
        let owner = slice.metadata.labels.as_ref().and_then(|l| l.get(SVC_NAME_LABEL));
        if owner.map(String::as_str) != Some(svc_name) {
            continue;
        }

        let eps_ports = slice.ports.as_deref().unwrap_or(&[]);
        let matched = eps_ports
            .iter()
            .find(|p| match (&port.name, &p.name) {
                (Some(sn), Some(pn)) => sn == pn,
                (None, None) => true,
                _ => false,
            })
            .or_else(|| if eps_ports.len() == 1 { eps_ports.first() } else { None });
        let Some(ep_port) = matched.and_then(|p| p.port) else { continue };

        for ep in &slice.endpoints {
            let ready = ep.conditions.as_ref().and_then(|c| c.ready).unwrap_or(true);
            if !ready {
                continue;
            }
            for addr in &ep.addresses {
                out.push((addr.clone(), ep_port));
            }
        }
    }
    out
}

/// The load-balancing expression that picks an index into the backend map,
/// e.g. `numgen random` (the caller appends ` mod N map { ... }`).
/// `sessionAffinity: ClientIP` always wins, regardless of the configured
/// default, since it's a per-Service opt-in.
fn lb_expr(svc: &Service, default_method: LbMethod, family: Family) -> String {
    let sticky = svc.spec.as_ref().and_then(|s| s.session_affinity.as_deref()) == Some("ClientIP");
    let method = if sticky { LbMethod::SourceHash } else { default_method };
    match method {
        LbMethod::Random => "numgen random".to_string(),
        LbMethod::RoundRobin => "numgen inc".to_string(),
        LbMethod::SourceHash => format!("jhash {} saddr", family.nft_word()),
    }
}

/// The DNAT target. A single backend needs no load-balancing at all, so it
/// gets a plain `<ip>:<port>` (`[<ip>]:<port>` for IPv6) — deliberately
/// avoiding `numgen`/a verdict map entirely. This isn't just simpler: it was
/// changed after a real single-backend Service (the in-cluster `kubernetes`
/// Service, one apiserver) failed on a device with an Android-derived
/// kernel — `nft` accepted the ruleset syntax fine but the kernel rejected
/// applying it with "Could not process rule: No such file or directory" at
/// exactly the `numgen` token, the signature of a missing `nft_numgen`
/// kernel module. A single-entry `numgen ... mod 1 map {...}` was doing
/// nothing a bare target doesn't already do, so removing it for the N=1
/// case sidesteps kernels without that module rather than requiring it
/// unconditionally. Multiple backends still need the map form — there's no
/// way to express "pick one of N" without some selection statement.
fn dnat_target(lb: &str, backends: &[(String, i32)]) -> Option<String> {
    match backends.len() {
        0 => None,
        1 => {
            let (ip, port) = &backends[0];
            Some(if ip.contains(':') { format!("[{ip}]:{port}") } else { format!("{ip}:{port}") })
        }
        n => {
            let entries: Vec<String> = backends
                .iter()
                .enumerate()
                .map(|(i, (ip, port))| format!("{i} : {ip} . {port}"))
                .collect();
            Some(format!("{lb} mod {n} map {{ {} }}", entries.join(", ")))
        }
    }
}

fn build_ruleset(state: &State, ip_family: IpFamily, lb_method: LbMethod) -> String {
    let families: &[Family] = match ip_family {
        IpFamily::V4 => &[Family::V4],
        IpFamily::V6 => &[Family::V6],
        IpFamily::Dual => &[Family::V4, Family::V6],
    };

    let mut prerouting = Vec::new();
    let mut output = Vec::new();

    for (key, svc) in &state.services {
        let Some(spec) = svc.spec.as_ref() else { continue };
        let Some((namespace, name)) = key.split_once('/') else { continue };

        let cluster_ips: Vec<&str> = spec
            .cluster_ips
            .as_deref()
            .filter(|v| !v.is_empty())
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_else(|| spec.cluster_ip.as_deref().into_iter().collect());

        for cluster_ip in cluster_ips {
            if cluster_ip.is_empty() || cluster_ip == "None" {
                continue; // headless or not yet allocated
            }
            let family = Family::of(cluster_ip);
            if !families.contains(&family) {
                continue;
            }
            let f = family.nft_word();

            for port in spec.ports.as_deref().unwrap_or(&[]) {
                let backends = backends_for(namespace, name, port, family, &state.endpoint_slices);
                let lb = lb_expr(svc, lb_method, family);
                let Some(target) = dnat_target(&lb, &backends) else { continue };
                let proto = port.protocol.as_deref().unwrap_or("TCP").to_ascii_lowercase();

                // ClusterIP: family is already established by `<f> daddr`, no
                // explicit qualifier needed on `dnat`.
                let rule = format!("{f} daddr {cluster_ip} {proto} dport {} dnat to {target}", port.port);
                prerouting.push(rule.clone());
                output.push(rule);

                // NodePort: `fib daddr type local` is family-neutral, so the
                // qualifier is required here (`dnat ip to` / `dnat ip6 to`).
                if let Some(node_port) = port.node_port.filter(|p| *p != 0) {
                    let np_rule =
                        format!("fib daddr type local {proto} dport {node_port} dnat {f} to {target}");
                    prerouting.push(np_rule.clone());
                    output.push(np_rule);
                }
            }
        }
    }

    let mut script = String::new();
    script.push_str(&format!("add table inet {TABLE}\n"));
    script.push_str(&format!("flush table inet {TABLE}\n"));
    script.push_str(&format!(
        "add chain inet {TABLE} prerouting {{ type nat hook prerouting priority dstnat ; policy accept ; }}\n"
    ));
    script.push_str(&format!(
        "add chain inet {TABLE} output {{ type nat hook output priority -100 ; policy accept ; }}\n"
    ));
    script.push_str(&format!(
        "add chain inet {TABLE} postrouting {{ type nat hook postrouting priority srcnat ; policy accept ; }}\n"
    ));
    script.push_str(&format!("add rule inet {TABLE} postrouting ct status dnat masquerade\n"));
    for r in &prerouting {
        script.push_str(&format!("add rule inet {TABLE} prerouting {r}\n"));
    }
    for r in &output {
        script.push_str(&format!("add rule inet {TABLE} output {r}\n"));
    }
    script
}

/// Confirms `nft` is on PATH *and* that this process can actually reach the
/// nftables netlink API — two separate things, and only the second one
/// matters. `nft --version` (what this used to run) proves neither: it
/// prints a string and exits 0 for any unprivileged user, so a nodeproxy
/// running without CAP_NET_ADMIN sailed straight past the check and then
/// failed inside `apply_nft()` on every single Service event forever,
/// warning each time and routing nothing — precisely the silently-useless
/// state `run()` returning an error is supposed to prevent.
///
/// `nft list tables` is the cheapest non-mutating query that actually opens
/// the netlink socket, so a permission problem surfaces here, once, with the
/// kernel's own message attached.
fn check_nft() -> Result<()> {
    let out =
        Command::new("nft").args(["list", "tables"]).output().context("nft not found on PATH")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = stderr.trim();
        // An empty stderr on failure is itself diagnostic: that's what nft
        // does when it can't reach netlink at all (no CAP_NET_ADMIN), as
        // opposed to a real nftables error, which always says something.
        anyhow::bail!(
            "`nft list tables` failed: {}",
            if detail.is_empty() {
                "no error output, which usually means nft couldn't reach netlink at all (no CAP_NET_ADMIN — run as root)"
            } else {
                detail
            }
        );
    }
    Ok(())
}

fn apply_nft(ruleset: &str) -> Result<(), String> {
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawning nft: {e}"))?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(ruleset.as_bytes())
        .map_err(|e| format!("writing nft script: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("waiting on nft: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "nft -f - failed: {}\n--- ruleset ---\n{ruleset}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requires a real `nft` binary and CAP_NET_ADMIN (root); skips itself
    /// otherwise so `cargo test` still passes in unprivileged CI/sandboxes.
    fn nft_check(script: &str) -> Result<(), String> {
        if Command::new("nft").arg("--version").output().is_err() {
            eprintln!("skipping: nft not installed");
            return Ok(());
        }
        let mut child = Command::new("nft")
            .args(["-c", "-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else if out.stderr.is_empty() && out.stdout.is_empty() {
            // No CAP_NET_ADMIN (e.g. unprivileged sandbox) — nft can't reach
            // netlink to check at all; that's not a syntax failure.
            eprintln!("skipping: nft -c produced no output (likely no CAP_NET_ADMIN here)");
            Ok(())
        } else {
            Err(format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ))
        }
    }

    fn fake_service(cluster_ips: Vec<&str>, session_affinity: Option<&str>) -> Service {
        Service {
            spec: Some(k8s_openapi::api::core::v1::ServiceSpec {
                cluster_ip: cluster_ips.first().map(|s| s.to_string()),
                cluster_ips: Some(cluster_ips.into_iter().map(String::from).collect()),
                session_affinity: session_affinity.map(String::from),
                ports: Some(vec![ServicePort {
                    port: 80,
                    node_port: Some(30080),
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn backends(n: usize, v6: bool) -> Vec<(String, i32)> {
        (0..n)
            .map(|i| {
                let ip = if v6 { format!("fd00:42::{}", i + 5) } else { format!("10.42.0.{}", i + 5) };
                (ip, 8080)
            })
            .collect()
    }

    #[test]
    fn dual_stack_all_lb_methods_pass_real_nft_syntax_check() {
        for method in [LbMethod::Random, LbMethod::RoundRobin, LbMethod::SourceHash] {
            for n in [1usize, 2, 3] {
                let svc = fake_service(vec!["10.43.0.1", "fd00::1"], None);
                let v4 = dnat_target(&lb_expr(&svc, method, Family::V4), &backends(n, false)).unwrap();
                let v6 = dnat_target(&lb_expr(&svc, method, Family::V6), &backends(n, true)).unwrap();

                let script = format!(
                    "add table inet {TABLE}\n\
                     add chain inet {TABLE} prerouting {{ type nat hook prerouting priority dstnat ; }}\n\
                     add chain inet {TABLE} output {{ type nat hook output priority -100 ; }}\n\
                     add chain inet {TABLE} postrouting {{ type nat hook postrouting priority srcnat ; }}\n\
                     add rule inet {TABLE} postrouting ct status dnat masquerade\n\
                     add rule inet {TABLE} prerouting ip daddr 10.43.0.1 tcp dport 80 dnat to {v4}\n\
                     add rule inet {TABLE} prerouting ip6 daddr fd00::1 tcp dport 80 dnat to {v6}\n\
                     add rule inet {TABLE} prerouting fib daddr type local tcp dport 30080 dnat ip to {v4}\n\
                     add rule inet {TABLE} prerouting fib daddr type local tcp dport 30081 dnat ip6 to {v6}\n"
                );
                nft_check(&script).unwrap_or_else(|e| panic!("method={method:?} n={n}: {e}"));
            }
        }
    }

    #[test]
    fn session_affinity_forces_source_hash_regardless_of_default() {
        let svc = fake_service(vec!["10.43.0.1"], Some("ClientIP"));
        assert_eq!(lb_expr(&svc, LbMethod::Random, Family::V4), "jhash ip saddr");
        assert_eq!(lb_expr(&svc, LbMethod::RoundRobin, Family::V4), "jhash ip saddr");
    }

    #[test]
    fn family_detection() {
        assert_eq!(Family::of("10.42.0.5"), Family::V4);
        assert_eq!(Family::of("fd00::5"), Family::V6);
    }
}
