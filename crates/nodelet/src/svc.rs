//! Service (ClusterIP + NodePort) proxy — nftables, no kube-proxy.
//!
//! Real kube-proxy watches Services + Endpoints and programs iptables/ipvs
//! rules to make ClusterIPs (which no interface ever owns) resolve to a real
//! backend pod. This does the same job with nftables, scoped to what a
//! single-node edge cluster needs: reconcile-on-event (no periodic resync,
//! no polling), the whole ruleset rebuilt atomically from current state every
//! time a Service or Endpoints object changes.
//!
//! Requires `nft` (nftables) and CAP_NET_ADMIN/root — same requirement real
//! kube-proxy has. If `nft` isn't usable, this logs once and does nothing
//! further (pods and direct pod-IP traffic are unaffected either way).
//!
//! Rules, in an IPv4-only table `not_k8s_svc`:
//!   - `prerouting`/`output` (nat, dstnat/-100): DNAT ClusterIP:port and
//!     NodePort traffic to a backend, load-balanced across ready endpoints
//!     with `numgen random mod N` when there's more than one.
//!   - `postrouting` (nat, srcnat): `ct status dnat masquerade` — SNAT any
//!     connection that got DNAT'd, which is what makes hairpin traffic
//!     (a pod calling a Service that routes back to itself) return correctly,
//!     without blanket-masquerading unrelated traffic.
//!
//! NodePort rules match `fib daddr type local` instead of listing every local
//! IP, so they only catch traffic actually addressed to this node — not
//! bridged pod-to-pod traffic that happens to reuse the same port number.

use futures::StreamExt;
use k8s_openapi::api::core::v1::{Endpoints, Service, ServicePort};
use kube::runtime::watcher;
use kube::runtime::watcher::Event;
use kube::{Api, Client, ResourceExt};
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;
use tracing::{debug, info, warn};

const TABLE: &str = "not_k8s_svc";

#[derive(Default)]
struct State {
    services: HashMap<String, Service>,
    endpoints: HashMap<String, Endpoints>,
}

pub struct ServiceProxy {
    client: Client,
}

impl ServiceProxy {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// Runs forever. Returns only if `nft` is unusable (nothing to do).
    pub async fn run(&self) {
        if let Err(e) = check_nft() {
            warn!(error = %e, "nft unavailable; Service (ClusterIP/NodePort) routing disabled. \
Direct pod-IP traffic still works.");
            return;
        }

        let mut state = State::default();
        let mut svc_stream = watch_services(&self.client);
        let mut ep_stream = watch_endpoints(&self.client);

        info!("service proxy watching Services + Endpoints (nftables backend)");

        loop {
            let changed = tokio::select! {
                item = svc_stream.next() => {
                    match item {
                        Some(Ok(ev)) => { apply_event(&mut state.services, ev); true }
                        Some(Err(e)) => { warn!(error = %e, "service watch error"); false }
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
                        Some(Ok(ev)) => { apply_event(&mut state.endpoints, ev); true }
                        Some(Err(e)) => { warn!(error = %e, "endpoints watch error"); false }
                        None => {
                            warn!("endpoints watch ended; restarting");
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            ep_stream = watch_endpoints(&self.client);
                            false
                        }
                    }
                }
            };

            if changed {
                let ruleset = build_ruleset(&state);
                match apply_nft(&ruleset) {
                    Ok(()) => debug!("nft ruleset applied"),
                    Err(e) => warn!(error = %e, "failed to apply nft ruleset"),
                }
            }
        }
    }
}

fn watch_services(client: &Client) -> futures::stream::BoxStream<'static, watcher::Result<Event<Service>>> {
    let api: Api<Service> = Api::all(client.clone());
    watcher(api, watcher::Config::default()).boxed()
}

fn watch_endpoints(client: &Client) -> futures::stream::BoxStream<'static, watcher::Result<Event<Endpoints>>> {
    let api: Api<Endpoints> = Api::all(client.clone());
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

/// Resolve one Service port to its ready backends (ip, port) via the
/// matching Endpoints object, matching subset ports by name the same way a
/// real kube client would (single unnamed port is the common case).
fn backends_for(port: &ServicePort, eps: Option<&Endpoints>) -> Vec<(String, i32)> {
    let mut out = Vec::new();
    let Some(eps) = eps else { return out };
    for subset in eps.subsets.as_deref().unwrap_or(&[]) {
        let addrs = subset.addresses.as_deref().unwrap_or(&[]);
        if addrs.is_empty() {
            continue;
        }
        let ports = subset.ports.as_deref().unwrap_or(&[]);
        let matched = ports
            .iter()
            .find(|p| match (&port.name, &p.name) {
                (Some(sn), Some(pn)) => sn == pn,
                (None, None) => true,
                _ => false,
            })
            .or_else(|| if ports.len() == 1 { ports.first() } else { None });
        let Some(ep_port) = matched else { continue };
        for a in addrs {
            out.push((a.ip.clone(), ep_port.port));
        }
    }
    out
}

/// `to <ip>:<port>` for a single backend, or a load-balancing `numgen`
/// verdict map across all of them.
fn dnat_target(backends: &[(String, i32)]) -> Option<String> {
    match backends.len() {
        0 => None,
        1 => {
            let (ip, port) = &backends[0];
            Some(format!("{ip}:{port}"))
        }
        n => {
            // Map values are a concatenation (`.`), not `addr:port` like the
            // single-target form below — nft's grammar treats `:` in a map
            // literal as a range/interval separator, not part of the value.
            let entries: Vec<String> = backends
                .iter()
                .enumerate()
                .map(|(i, (ip, port))| format!("{i} : {ip} . {port}"))
                .collect();
            Some(format!("numgen random mod {n} map {{ {} }}", entries.join(", ")))
        }
    }
}

fn build_ruleset(state: &State) -> String {
    let mut prerouting = Vec::new();
    let mut output = Vec::new();

    for (key, svc) in &state.services {
        let Some(spec) = svc.spec.as_ref() else { continue };
        let cluster_ip = match spec.cluster_ip.as_deref() {
            Some(ip) if !ip.is_empty() && ip != "None" => ip,
            _ => continue, // headless or not yet allocated
        };
        let eps = state.endpoints.get(key);

        for port in spec.ports.as_deref().unwrap_or(&[]) {
            let backends = backends_for(port, eps);
            let Some(target) = dnat_target(&backends) else { continue };
            let proto = port.protocol.as_deref().unwrap_or("TCP").to_ascii_lowercase();

            let cluster_rule = format!("ip daddr {cluster_ip} {proto} dport {} dnat to {target}", port.port);
            prerouting.push(cluster_rule.clone());
            output.push(cluster_rule);

            if let Some(node_port) = port.node_port.filter(|p| *p != 0) {
                let np_rule = format!("fib daddr type local {proto} dport {node_port} dnat to {target}");
                prerouting.push(np_rule.clone());
                output.push(np_rule);
            }
        }
    }

    let mut script = String::new();
    script.push_str(&format!("add table ip {TABLE}\n"));
    script.push_str(&format!("flush table ip {TABLE}\n"));
    script.push_str(&format!(
        "add chain ip {TABLE} prerouting {{ type nat hook prerouting priority dstnat ; policy accept ; }}\n"
    ));
    script.push_str(&format!(
        "add chain ip {TABLE} output {{ type nat hook output priority -100 ; policy accept ; }}\n"
    ));
    script.push_str(&format!(
        "add chain ip {TABLE} postrouting {{ type nat hook postrouting priority srcnat ; policy accept ; }}\n"
    ));
    script.push_str(&format!("add rule ip {TABLE} postrouting ct status dnat masquerade\n"));
    for r in &prerouting {
        script.push_str(&format!("add rule ip {TABLE} prerouting {r}\n"));
    }
    for r in &output {
        script.push_str(&format!("add rule ip {TABLE} output {r}\n"));
    }
    script
}

fn check_nft() -> Result<(), String> {
    let out = Command::new("nft")
        .arg("--version")
        .output()
        .map_err(|e| format!("nft not found on PATH: {e}"))?;
    if !out.status.success() {
        return Err("`nft --version` exited non-zero".to_string());
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
