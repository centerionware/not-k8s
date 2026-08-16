//! Service (ClusterIP + NodePort) proxy — nftables, not iptables.
//!
//! Real kube-proxy watches Services + EndpointSlices and programs iptables/
//! ipvs rules to make ClusterIPs (which no interface ever owns) resolve to a
//! real backend pod. This does the same job with nftables, scoped to what a
//! single-node edge cluster needs: reconcile-on-event (no periodic resync,
//! no polling), the whole ruleset rebuilt atomically from current state
//! every time a Service or EndpointSlice changes. Initial relists are folded
//! into one rebuild after both snapshots are complete, rather than rebuilding
//! once for every `InitApply` item.
//!
//! The whole ruleset is rebuilt on each Service or EndpointSlice event. Keep
//! an explicit `established,related accept` fast path ahead of the DNAT rules:
//! this is required for long-lived pod-to-apiserver watches in the target
//! kernels, where replacing the table can otherwise reset the connection
//! before the conntrack NAT binding is reused. The end-to-end
//! `nftables_rebuild_established_conns.sh` test keeps a real watch open across
//! a burst of replacements.
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
//! NodePort rules match `fib daddr type local`, where the kernel has it,
//! instead of listing every local IP — so they only catch traffic actually
//! addressed to this node, not bridged pod-to-pod traffic that happens to
//! reuse the same port number. The fallback for kernels without it is
//! below.
//!
//! ## Kernels that can't run all of this
//!
//! `fib`, `numgen` and `jhash` are each a separate kernel module, and a
//! build can simply omit them — confirmed on an Android-derived 6.12
//! aarch64 kernel missing all three. They fail identically and unhelpfully:
//! `nft` accepts the syntax, then the kernel rejects the rule with "Could
//! not process rule: No such file or directory" at the offending token.
//!
//! That is not a per-Service problem. The ruleset is applied atomically, so
//! one unusable rule makes `nft -f -` reject the whole file and every
//! Service on the node loses its rules. `probe_caps()` therefore probes each
//! feature up front — by really committing a rule, since syntax checks pass
//! on kernels that then refuse it — and only rules this kernel can run are
//! ever emitted:
//!
//!   - No `fib`: NodePort matches this node's own addresses explicitly.
//!     The address list is only as current as the last rebuild, which is
//!     the trade-off `fib` was chosen to avoid, but a rebuild happens on
//!     every Service/EndpointSlice event.
//!   - No `numgen`: multi-backend Services are load balanced through
//!     `iptables`' `statistic` match instead (`build_statistic_ruleset()`)
//!     — the same mechanism real kube-proxy's iptables backend uses, and
//!     available on that kernel when every native nftables selector is not.
//!     Those Services are then owned entirely by that chain and omitted
//!     here, so the two never race for the same connection.
//!   - Neither: several backends collapse to one. Routing survives; load
//!     balancing does not.
//!   - No `jhash`: `sessionAffinity: ClientIP` pins to a single backend,
//!     which still satisfies "same client, same backend". Such Services are
//!     also never handed to the statistic chain — that match re-randomises
//!     per connection, which is exactly what the field forbids, and
//!     xtables' own affinity match (`-m recent`) is missing on the same
//!     kernel.

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
    services_initialized: bool,
    endpoint_slices_initialized: bool,
    dirty: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventPhase {
    Init,
    InitApply,
    Apply,
    Delete,
    InitDone,
}

impl State {
    /// Records one watch event and reports whether the current snapshot is
    /// ready for a rebuild. `watcher` emits one `InitApply` per existing
    /// object during a relist; those events only dirty the mirror. Rebuild
    /// once both resource kinds have reached `InitDone`, then rebuild
    /// immediately for ordinary live changes.
    fn record_event(&mut self, services: bool, phase: EventPhase) -> bool {
        self.dirty = true;
        let initialized = if services {
            &mut self.services_initialized
        } else {
            &mut self.endpoint_slices_initialized
        };
        match phase {
            EventPhase::Init => *initialized = false,
            EventPhase::InitDone => *initialized = true,
            EventPhase::InitApply | EventPhase::Apply | EventPhase::Delete => {}
        }

        if self.dirty && self.services_initialized && self.endpoint_slices_initialized {
            self.dirty = false;
            true
        } else {
            false
        }
    }
}

pub struct ServiceProxy {
    client: Client,
    ip_family: IpFamily,
    lb_method: LbMethod,
}

impl ServiceProxy {
    pub fn new(client: Client, ip_family: IpFamily, lb_method: LbMethod) -> Self {
        Self {
            client,
            ip_family,
            lb_method,
        }
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
        let caps = probe_caps();
        info!(
            ip_family = ?self.ip_family,
            lb_method = ?self.lb_method,
            fib = caps.fib, numgen = caps.numgen, jhash = caps.jhash,
            "service proxy watching Services + EndpointSlices (nftables backend)"
        );

        let mut state = State::default();
        let mut svc_stream = watch_services(&self.client);
        let mut ep_stream = watch_endpoint_slices(&self.client);

        loop {
            let mut changed = false;
            tokio::select! {
                item = svc_stream.next() => {
                    match item {
                        Some(Ok(ev)) => {
                            let phase = apply_event(&mut state.services, ev);
                            changed |= state.record_event(true, phase);
                        }
                        Some(Err(e)) => warn!(error = ?e, "service watch error"),
                        None => {
                            warn!("service watch ended; restarting");
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            svc_stream = watch_services(&self.client);
                        }
                    }
                }
                item = ep_stream.next() => {
                    match item {
                        Some(Ok(ev)) => {
                            let phase = apply_event(&mut state.endpoint_slices, ev);
                            changed |= state.record_event(false, phase);
                        }
                        Some(Err(e)) => warn!(error = ?e, "endpointslice watch error"),
                        None => {
                            warn!("endpointslice watch ended; restarting");
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            ep_stream = watch_endpoint_slices(&self.client);
                        }
                    }
                }
            }
            if !changed {
                continue;
            }
            // Re-read on every rebuild rather than caching: an address
            // added or removed later must be picked up. Only costs a
            // subprocess on kernels that actually need the fallback.
            let local = if caps.fib { Vec::new() } else { local_addrs() };
            let ruleset = build_ruleset(&state, self.ip_family, self.lb_method, caps, &local);
            // Propagated, not just logged. Reconciliation here is purely
            // event-driven — the whole point of the design, but it means
            // there is no periodic resync to quietly retry a failed
            // apply. Logging and continuing would leave the kernel
            // holding the last good ruleset while nodeproxy reports
            // healthy, with the divergence persisting until some
            // unrelated Service or EndpointSlice happened to change.
            // Returning lets main() exit non-zero so the service manager
            // restarts, relists, and rebuilds from scratch — which also
            // recovers on its own once whatever produced the rejected
            // ruleset is gone.
            apply_nft(&ruleset)
                .map_err(|e| anyhow::anyhow!(e))
                .context("applying the nftables ruleset")?;
            // The xtables fallback, only ever non-empty on a kernel
            // that cannot select among backends natively. Applied after
            // the nft table so a failure here still leaves single-backend
            // routing in place.
            if !caps.numgen {
                for family in [Family::V4, Family::V6] {
                    // Gated per family: a host with iptables but no
                    // ip6tables must keep serving IPv4 rather than
                    // failing the whole apply over a tool it never
                    // needed.
                    if !caps.statistic(family) {
                        continue;
                    }
                    match build_statistic_ruleset(&state, self.ip_family, caps, family) {
                        Some(rs) => {
                            ensure_statistic_chain(family)?;
                            apply_statistic(family, &rs).with_context(|| {
                                format!("applying the {} statistic ruleset", ipt(family))
                            })?;
                        }
                        None => flush_statistic_chain(family)?,
                    }
                }
            }
            debug!("ruleset applied");
        }
    }
}

fn watch_services(
    client: &Client,
) -> futures::stream::BoxStream<'static, watcher::Result<Event<Service>>> {
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
/// upsert; `Delete` removes; `InitDone` marks the snapshot complete. The
/// caller batches `InitApply` events and rebuilds only after both resource
/// kinds finish their initial relist.
fn apply_event<T: Clone + ResourceExt>(map: &mut HashMap<String, T>, ev: Event<T>) -> EventPhase {
    match ev {
        Event::Init => {
            map.clear();
            EventPhase::Init
        }
        Event::InitApply(obj) => {
            map.insert(obj_key(&obj), obj);
            EventPhase::InitApply
        }
        Event::Apply(obj) => {
            map.insert(obj_key(&obj), obj);
            EventPhase::Apply
        }
        Event::Delete(obj) => {
            map.remove(&obj_key(&obj));
            EventPhase::Delete
        }
        Event::InitDone => EventPhase::InitDone,
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
        let owner = slice
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get(SVC_NAME_LABEL));
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
            .or_else(|| {
                if eps_ports.len() == 1 {
                    eps_ports.first()
                } else {
                    None
                }
            });
        let Some(ep_port) = matched.and_then(|p| p.port) else {
            continue;
        };

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
/// `None` means this kernel has no usable selector — the caller then sends
/// every connection to a single backend rather than emitting a rule that
/// would be rejected and take the whole ruleset with it.
/// The per-Service opt-in that overrides proxy-wide load-balancing policy.
fn wants_client_ip_affinity(svc: &Service) -> bool {
    svc.spec
        .as_ref()
        .and_then(|s| s.session_affinity.as_deref())
        == Some("ClientIP")
}

fn lb_expr(svc: &Service, default_method: LbMethod, family: Family, caps: Caps) -> Option<String> {
    let method = if wants_client_ip_affinity(svc) {
        LbMethod::SourceHash
    } else {
        default_method
    };
    match method {
        LbMethod::Random if caps.numgen => Some("numgen random".to_string()),
        LbMethod::RoundRobin if caps.numgen => Some("numgen inc".to_string()),
        LbMethod::SourceHash if caps.jhash => Some(format!("jhash {} saddr", family.nft_word())),
        // Note sessionAffinity degrades *correctly* rather than merely
        // acceptably: pinning every client to one backend still satisfies
        // "the same client reaches the same backend."
        _ => None,
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
fn dnat_target(lb: Option<&str>, backends: &[(String, i32)]) -> Option<String> {
    fn bare(b: &(String, i32)) -> String {
        let (ip, port) = b;
        if ip.contains(':') {
            format!("[{ip}]:{port}")
        } else {
            format!("{ip}:{port}")
        }
    }
    match backends.len() {
        0 => None,
        1 => Some(bare(&backends[0])),
        // No selector this kernel can run: pick one backend and route all of
        // it there. Degraded, but a working Service beats a rejected ruleset
        // that also takes every other Service down with it.
        _ if lb.is_none() => Some(bare(&backends[0])),
        n => {
            let lb = lb.expect("checked above");
            let entries: Vec<String> = backends
                .iter()
                .enumerate()
                .map(|(i, (ip, port))| format!("{i} : {ip} . {port}"))
                .collect();
            Some(format!("{lb} mod {n} map {{ {} }}", entries.join(", ")))
        }
    }
}

/// `local` is this node's own addresses, used only for the NodePort
/// fallback when the kernel has no `fib`. Passed in rather than looked up
/// here so this stays a pure function of its inputs and the fallback is
/// unit-testable without depending on the test machine's interfaces.
fn build_ruleset(
    state: &State,
    ip_family: IpFamily,
    lb_method: LbMethod,
    caps: Caps,
    local: &[String],
) -> String {
    let families: &[Family] = match ip_family {
        IpFamily::V4 => &[Family::V4],
        IpFamily::V6 => &[Family::V6],
        IpFamily::Dual => &[Family::V4, Family::V6],
    };

    let mut prerouting = Vec::new();
    let mut output = Vec::new();

    for (key, svc) in &state.services {
        let Some(spec) = svc.spec.as_ref() else {
            continue;
        };
        let Some((namespace, name)) = key.split_once('/') else {
            continue;
        };

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
                // Owned by the xtables statistic chain instead — emitting
                // here too would mean two DNAT rules racing for the same
                // connection.
                if caps.delegates_to_statistic(
                    family,
                    backends.len(),
                    wants_client_ip_affinity(svc),
                ) {
                    continue;
                }
                let lb = lb_expr(svc, lb_method, family, caps);
                let Some(target) = dnat_target(lb.as_deref(), &backends) else {
                    continue;
                };
                let proto = port
                    .protocol
                    .as_deref()
                    .unwrap_or("TCP")
                    .to_ascii_lowercase();

                // ClusterIP: family is already established by `<f> daddr`, no
                // explicit qualifier needed on `dnat`.
                let rule = format!(
                    "{f} daddr {cluster_ip} {proto} dport {} dnat to {target}",
                    port.port
                );
                prerouting.push(rule.clone());
                output.push(rule);

                // NodePort. With fib: `fib daddr type local` is family-neutral,
                // so the DNAT needs an explicit qualifier (`dnat ip to` /
                // `dnat ip6 to`) — see this module's header for the silent
                // failure that omitting it causes.
                //
                // Without fib: match this node's own addresses one by one
                // instead. `<f> daddr <addr>` establishes the family itself,
                // so the qualifier goes away, exactly as in the ClusterIP
                // case above. The trade-off is the one svc.rs originally
                // chose fib to avoid — the address list is only as current
                // as the last rebuild — but a rebuild happens on every
                // Service/EndpointSlice event, and it is the difference
                // between NodePort working on such a kernel and the entire
                // ruleset being rejected.
                if let Some(node_port) = port.node_port.filter(|p| *p != 0) {
                    if caps.fib {
                        let np_rule = format!(
                            "fib daddr type local {proto} dport {node_port} dnat {f} to {target}"
                        );
                        prerouting.push(np_rule.clone());
                        output.push(np_rule);
                    } else {
                        for addr in local.iter().filter(|a| Family::of(a) == family) {
                            let np_rule = format!(
                                "{f} daddr {addr} {proto} dport {node_port} dnat to {target}"
                            );
                            prerouting.push(np_rule.clone());
                            output.push(np_rule);
                        }
                    }
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
    // Keep already-established pod-to-Service flows out of the DNAT
    // selection rules while this table is being rebuilt. In particular, the
    // CSI provisioner's long-lived PVC watch otherwise gets reset and never
    // observes claims created after its initial informer list.
    script.push_str(&format!(
        "add rule inet {TABLE} prerouting ct state established,related accept\n"
    ));
    script.push_str(&format!(
        "add rule inet {TABLE} output ct state established,related accept\n"
    ));
    script.push_str(&format!(
        "add rule inet {TABLE} postrouting ct status dnat masquerade\n"
    ));
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
/// What this kernel's nftables can actually do.
///
/// Every one of these is a separate kernel module that a build can simply
/// not include, and the failure mode is identical and unhelpful in all
/// three cases: `nft` accepts the syntax, then the kernel rejects the rule
/// with "Could not process rule: No such file or directory" pointing at the
/// offending token. Confirmed on an Android-derived 6.12 aarch64 kernel
/// where all three are absent.
///
/// This matters far more than "one Service loses a feature", because the
/// ruleset is applied atomically: a single rule the kernel won't take means
/// `nft -f -` rejects the whole file, so one NodePort Service (or one
/// Service with two backends) removes routing for *every* Service on the
/// node. Probing up front and emitting only rules this kernel can run keeps
/// the node working, degraded and loudly logged, instead of not at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// `fib daddr type local` — how NodePort matches "addressed to me".
    pub fib: bool,
    /// `numgen random|inc` — random and round-robin backend selection.
    pub numgen: bool,
    /// `jhash <family> saddr` — source-hash / sessionAffinity selection.
    pub jhash: bool,
    /// xtables' `-m statistic --mode random --probability`, reachable
    /// through `iptables` even on a kernel with no `nft_numgen`. This is
    /// the mechanism real kube-proxy's *iptables* backend uses to spread
    /// connections across endpoints, and on the kernel that motivated all
    /// of this it is available when every native nftables selector is not.
    /// See `build_statistic_ruleset()`.
    ///
    /// Probed per address family, because `iptables` and `ip6tables` are
    /// separate binaries over separate kernel tables: a host can perfectly
    /// well have one without the other. Probing v4 and then applying to v6
    /// would turn a missing `ip6tables` into a failed apply, and since
    /// apply failures are fatal that means a restart loop on a node whose
    /// IPv4 Services were working fine.
    pub statistic_v4: bool,
    pub statistic_v6: bool,
}

impl Caps {
    /// All-supported, for tests and for reasoning about a normal kernel.
    #[cfg(test)]
    fn all() -> Self {
        Self {
            fib: true,
            numgen: true,
            jhash: true,
            statistic_v4: true,
            statistic_v6: true,
        }
    }

    fn statistic(self, family: Family) -> bool {
        match family {
            Family::V4 => self.statistic_v4,
            Family::V6 => self.statistic_v6,
        }
    }

    /// Whether a Service with this many ready backends has to be programmed
    /// through the xtables statistic fallback rather than the nft table.
    /// Never true on a kernel with `numgen` — normal hosts never touch
    /// iptables at all.
    fn delegates_to_statistic(self, family: Family, backends: usize, sticky: bool) -> bool {
        // `sticky` (sessionAffinity: ClientIP) must never reach the
        // statistic chain. That match re-randomises on every connection,
        // which is precisely what the field forbids, and xtables' own
        // affinity mechanism (`-m recent`) is missing on the same kernel
        // that lacks nft_numgen — verified, not assumed. Pinning such a
        // Service to one backend keeps its contract intact and is the only
        // correct degradation available.
        backends > 1 && !self.numgen && self.statistic(family) && !sticky
    }
}

/// Applies a one-rule probe table and deletes it again. Syntax alone proves
/// nothing here — `nft -c` parses these fine on a kernel that then refuses
/// them — so the probe has to really commit the rule.
fn probe_rule(rule: &str) -> bool {
    let table = "not_k8s_probe";
    let script = format!(
        "add table inet {table}\n\
         add chain inet {table} p {{ type nat hook prerouting priority dstnat ; policy accept ; }}\n\
         add rule inet {table} p {rule}\n"
    );
    let ok = apply_nft(&script).is_ok();
    let _ = apply_nft(&format!("delete table inet {table}\n"));
    ok
}

/// The xtables chain the statistic fallback owns, in the `nat` table.
/// Everything in it is rewritten wholesale on every rebuild; nothing else
/// in `nat` is touched (see `apply_statistic()`).
const IPT_CHAIN: &str = "NOTK8S-SVC";

fn ipt(family: Family) -> &'static str {
    match family {
        Family::V4 => "iptables",
        Family::V6 => "ip6tables",
    }
}

fn ipt_restore(family: Family) -> &'static str {
    match family {
        Family::V4 => "iptables-restore",
        Family::V6 => "ip6tables-restore",
    }
}

fn run_ok(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Can this host actually append a statistic-matched DNAT? Probed the same
/// way as the nft expressions — by really committing a rule to a scratch
/// chain, because `iptables` will happily accept the syntax of a match
/// whose kernel module is missing and only fail on append.
fn probe_statistic(family: Family) -> bool {
    let t = ipt(family);
    let chain = "NOTK8S-PROBE";
    let dest = match family {
        Family::V4 => "10.255.255.254:1",
        Family::V6 => "[fd00::1]:1",
    };
    let _ = run_ok(t, &["-w", "5", "-t", "nat", "-N", chain]);
    let ok = run_ok(
        t,
        &[
            "-w",
            "5",
            "-t",
            "nat",
            "-A",
            chain,
            "-p",
            "tcp",
            "-m",
            "statistic",
            "--mode",
            "random",
            "--probability",
            "0.5",
            "-j",
            "DNAT",
            "--to-destination",
            dest,
        ],
    );
    let _ = run_ok(t, &["-w", "5", "-t", "nat", "-F", chain]);
    let _ = run_ok(t, &["-w", "5", "-t", "nat", "-X", chain]);
    ok
}

fn probe_caps() -> Caps {
    let caps = Caps {
        statistic_v4: probe_statistic(Family::V4),
        statistic_v6: probe_statistic(Family::V6),
        fib: probe_rule("fib daddr type local tcp dport 65000 dnat ip to 10.255.255.254:1"),
        numgen: probe_rule(
            "ip daddr 10.255.255.254 tcp dport 65000 dnat to numgen random mod 2 \
map { 0 : 10.255.255.254 . 1, 1 : 10.255.255.253 . 1 }",
        ),
        jhash: probe_rule(
            "ip daddr 10.255.255.254 tcp dport 65000 dnat to jhash ip saddr mod 2 \
map { 0 : 10.255.255.254 . 1, 1 : 10.255.255.253 . 1 }",
        ),
    };
    if !caps.fib {
        warn!(
            "this kernel has no nft_fib — NodePort will match this node's own addresses \
explicitly instead of `fib daddr type local`. Equivalent for traffic addressed to a local \
address, but it only covers addresses present when each ruleset is built."
        );
    }
    if !caps.numgen && (caps.statistic_v4 || caps.statistic_v6) {
        info!(
            "this kernel has no nft_numgen, so multi-backend Services are load balanced through \
iptables' statistic match instead — the same mechanism kube-proxy's iptables backend uses. \
Single-backend Services and all other rules stay in nftables."
        );
    }
    if !caps.numgen && !caps.statistic_v4 && !caps.statistic_v6 {
        warn!(
            "this kernel can neither select among backends in nftables (no nft_numgen) nor fall \
back to iptables' statistic match — Services with more than one ready backend will be sent \
entirely to one of them. Routing works; load balancing does not."
        );
    }
    if !caps.jhash {
        warn!(
            "this kernel has no nft_hash, so sessionAffinity: ClientIP cannot be implemented by \
source hashing. Affected Services are pinned to a single backend, which satisfies \
'same client, same backend' but removes their load balancing."
        );
    }
    caps
}

/// This node's own IP addresses, for the NodePort fallback when `fib` is
/// unavailable. Shells out to `ip`, the same "use the host's own tools"
/// approach this module already takes with `nft` — and re-read on every
/// rebuild rather than cached, so an address added or removed later is
/// picked up by the next Service event.
fn local_addrs() -> Vec<String> {
    let Ok(out) = Command::new("ip").args(["-o", "addr", "show"]).output() else {
        warn!("`ip` not on PATH — cannot enumerate local addresses for the NodePort fallback");
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut it = line
                .split_whitespace()
                .skip_while(|t| *t != "inet" && *t != "inet6");
            let _family = it.next()?;
            let cidr = it.next()?;
            let addr = cidr.split('/').next()?;
            // Link-local v6 is per-interface and never a service address.
            if addr.starts_with("fe80:") {
                return None;
            }
            Some(addr.to_string())
        })
        .collect()
}

/// Renders the `nat`-table chain that load balances multi-backend Services
/// on kernels without `nft_numgen`, in `iptables-restore` syntax.
///
/// The probability scheme is real kube-proxy's, because it is the correct
/// one: rule *i* of *N* matches with probability `1/(N-i)` and the last
/// rule matches unconditionally, so sequential evaluation yields an even
/// split. A naive `1/N` on every rule would send `(1-1/N)^N` of traffic —
/// about 37% at large N — past the end of the chain entirely.
///
/// Returns `None` when nothing needs the fallback, which is the normal case
/// on any kernel with `numgen`.
fn build_statistic_ruleset(
    state: &State,
    ip_family: IpFamily,
    caps: Caps,
    family: Family,
) -> Option<String> {
    let families: &[Family] = match ip_family {
        IpFamily::V4 => &[Family::V4],
        IpFamily::V6 => &[Family::V6],
        IpFamily::Dual => &[Family::V4, Family::V6],
    };
    if !families.contains(&family) {
        return None;
    }

    let mut rules = Vec::new();
    for (key, svc) in &state.services {
        let Some(spec) = svc.spec.as_ref() else {
            continue;
        };
        let Some((namespace, name)) = key.split_once('/') else {
            continue;
        };

        let cluster_ips: Vec<&str> = spec
            .cluster_ips
            .as_deref()
            .filter(|v| !v.is_empty())
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_else(|| spec.cluster_ip.as_deref().into_iter().collect());

        for cluster_ip in cluster_ips {
            if cluster_ip.is_empty() || cluster_ip == "None" || Family::of(cluster_ip) != family {
                continue;
            }
            for port in spec.ports.as_deref().unwrap_or(&[]) {
                let backends = backends_for(namespace, name, port, family, &state.endpoint_slices);
                if !caps.delegates_to_statistic(
                    family,
                    backends.len(),
                    wants_client_ip_affinity(svc),
                ) {
                    continue;
                }
                let proto = port
                    .protocol
                    .as_deref()
                    .unwrap_or("TCP")
                    .to_ascii_lowercase();
                let n = backends.len();

                for (i, (ip, bport)) in backends.iter().enumerate() {
                    let dest = if ip.contains(':') {
                        format!("[{ip}]:{bport}")
                    } else {
                        format!("{ip}:{bport}")
                    };
                    let sel = if i + 1 == n {
                        // Last one takes whatever reached it.
                        String::new()
                    } else {
                        let p = 1.0_f64 / ((n - i) as f64);
                        format!(" -m statistic --mode random --probability {p:.11}")
                    };
                    rules.push(format!(
                        "-A {IPT_CHAIN} -d {cluster_ip} -p {proto} --dport {}{sel} -j DNAT --to-destination {dest}",
                        port.port
                    ));
                    if let Some(node_port) = port.node_port.filter(|p| *p != 0) {
                        // NodePort: xtables has its own "is this addressed
                        // to me" match, so unlike the nft fallback this
                        // needs no address enumeration at all.
                        rules.push(format!(
                            "-A {IPT_CHAIN} -m addrtype --dst-type LOCAL -p {proto} --dport {node_port}{sel} -j DNAT --to-destination {dest}"
                        ));
                    }
                }
            }
        }
    }

    if rules.is_empty() {
        return None;
    }
    // Keep the fallback's long-lived conntrack flows out of its per-event
    // statistic/DNAT rules for the same reason as the nftables chains above.
    let established = format!(
        "-A {IPT_CHAIN} -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT"
    );
    // `:CHAIN - [0:0]` replaces this chain's contents; --noflush leaves
    // every other chain in `nat` (flannel's, anyone else's) untouched.
    // Verified directly: two consecutive applies leave two rules, not four.
    Some(format!(
        "*nat\n:{IPT_CHAIN} - [0:0]\n{established}\n{}\nCOMMIT\n",
        rules.join("\n")
    ))
}

/// Installs the chain and the jumps into it. Idempotent: the jump is only
/// inserted when absent, so this never accumulates duplicates across the
/// rebuilds that happen on every Service event.
fn statistic_chain_exists(family: Family) -> bool {
    run_ok(
        ipt(family),
        &["-w", "5", "-t", "nat", "-L", IPT_CHAIN, "-n"],
    )
}

fn ensure_statistic_chain(family: Family) -> Result<()> {
    let t = ipt(family);
    // `-N` fails when the chain already exists, which is the ordinary case
    // on every rebuild after the first — so its exit status says nothing.
    // What must hold afterwards is that the chain is *there*, which is
    // what gets checked and propagated.
    let _ = run_ok(t, &["-w", "5", "-t", "nat", "-N", IPT_CHAIN]);
    if !statistic_chain_exists(family) {
        anyhow::bail!("could not create the {IPT_CHAIN} chain with {t}");
    }
    for hook in ["PREROUTING", "OUTPUT"] {
        if !run_ok(t, &["-w", "5", "-t", "nat", "-C", hook, "-j", IPT_CHAIN])
            && !run_ok(
                t,
                &["-w", "5", "-t", "nat", "-I", hook, "1", "-j", IPT_CHAIN],
            )
        {
            anyhow::bail!("could not jump from nat {hook} to {IPT_CHAIN} with {t}");
        }
    }
    Ok(())
}

/// Empties the chain without removing it, for when no Service needs the
/// fallback any more. Leaving stale DNAT rules behind would silently route
/// to pods that no longer exist.
fn flush_statistic_chain(family: Family) -> Result<()> {
    // A chain that was never created is already empty — the common case on
    // any host that has never needed the fallback, and not a failure.
    if !statistic_chain_exists(family) {
        return Ok(());
    }
    if !run_ok(ipt(family), &["-w", "5", "-t", "nat", "-F", IPT_CHAIN]) {
        anyhow::bail!("could not flush the {IPT_CHAIN} chain with {}", ipt(family));
    }
    Ok(())
}

fn apply_statistic(family: Family, ruleset: &str) -> Result<()> {
    let mut child = Command::new(ipt_restore(family))
        .args(["--noflush", "--wait", "5"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {}", ipt_restore(family)))?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(ruleset.as_bytes())
        .context("writing iptables-restore input")?;
    let out = child
        .wait_with_output()
        .context("waiting on iptables-restore")?;
    if !out.status.success() {
        anyhow::bail!(
            "{} failed: {}\n--- ruleset ---\n{ruleset}",
            ipt_restore(family),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

fn check_nft() -> Result<()> {
    let out = Command::new("nft")
        .args(["list", "tables"])
        .output()
        .context("nft not found on PATH")?;
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
    let out = child
        .wait_with_output()
        .map_err(|e| format!("waiting on nft: {e}"))?;
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
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
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

    /// `nft_check`'s counterpart for the xtables fallback — requires a real
    /// `iptables-restore` binary; skips itself otherwise, same reasoning.
    /// `--test` is `iptables-restore`'s own dry-run: parses and validates
    /// without committing, the same job `nft -c` does for the nft path.
    fn ipt_check(ruleset: &str) -> Result<(), String> {
        if Command::new("iptables-restore")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: iptables-restore not installed");
            return Ok(());
        }
        let mut child = Command::new("iptables-restore")
            .args(["--test", "--noflush"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(ruleset.as_bytes())
            .unwrap();
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else if out.stderr.is_empty() && out.stdout.is_empty() {
            eprintln!("skipping: iptables-restore --test produced no output (likely no CAP_NET_ADMIN here)");
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
                let ip = if v6 {
                    format!("fd00:42::{}", i + 5)
                } else {
                    format!("10.42.0.{}", i + 5)
                };
                (ip, 8080)
            })
            .collect()
    }

    #[test]
    fn dual_stack_all_lb_methods_pass_real_nft_syntax_check() {
        for method in [LbMethod::Random, LbMethod::RoundRobin, LbMethod::SourceHash] {
            for n in [1usize, 2, 3] {
                let svc = fake_service(vec!["10.43.0.1", "fd00::1"], None);
                let caps = Caps::all();
                let v4 = dnat_target(
                    lb_expr(&svc, method, Family::V4, caps).as_deref(),
                    &backends(n, false),
                )
                .unwrap();
                let v6 = dnat_target(
                    lb_expr(&svc, method, Family::V6, caps).as_deref(),
                    &backends(n, true),
                )
                .unwrap();

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
        let caps = Caps::all();
        assert_eq!(
            lb_expr(&svc, LbMethod::Random, Family::V4, caps).as_deref(),
            Some("jhash ip saddr")
        );
        assert_eq!(
            lb_expr(&svc, LbMethod::RoundRobin, Family::V4, caps).as_deref(),
            Some("jhash ip saddr")
        );
    }

    /// The whole point of Caps: on a kernel missing these modules the
    /// generated rules must not mention them at all, because one rejected
    /// rule takes the entire atomically-applied ruleset with it.
    #[test]
    fn a_kernel_without_numgen_or_jhash_gets_no_selector_at_all() {
        let none = Caps {
            fib: true,
            numgen: false,
            jhash: false,
            statistic_v4: false,
            statistic_v6: false,
        };
        let plain = fake_service(vec!["10.43.0.1"], None);
        let sticky = fake_service(vec!["10.43.0.1"], Some("ClientIP"));
        for method in [LbMethod::Random, LbMethod::RoundRobin, LbMethod::SourceHash] {
            assert_eq!(lb_expr(&plain, method, Family::V4, none), None);
            assert_eq!(lb_expr(&sticky, method, Family::V4, none), None);
        }
    }

    #[test]
    fn without_a_selector_multiple_backends_collapse_to_one_working_target() {
        // Not "no rule": a Service with backends must still route.
        let target = dnat_target(None, &backends(3, false)).expect("must still produce a target");
        assert_eq!(target, "10.42.0.5:8080");
        assert!(
            !target.contains("numgen"),
            "must not emit a selector this kernel lacks"
        );
        assert!(
            !target.contains("map"),
            "must not emit a verdict map this kernel lacks"
        );
        // Zero backends is still nothing to route to, selector or not.
        assert_eq!(dnat_target(None, &[]), None);
    }

    /// A Service with one ready backend, for ruleset-level tests.
    fn state_with_one_nodeport_service() -> State {
        use k8s_openapi::api::discovery::v1::{Endpoint, EndpointConditions, EndpointPort};
        let mut state = State::default();
        state
            .services
            .insert("n/s".to_string(), fake_service(vec!["10.43.0.1"], None));
        state.endpoint_slices.insert(
            "n/s-abc".to_string(),
            EndpointSlice {
                metadata: kube::core::ObjectMeta {
                    namespace: Some("n".into()),
                    name: Some("s-abc".into()),
                    labels: Some([(SVC_NAME_LABEL.to_string(), "s".to_string())].into()),
                    ..Default::default()
                },
                address_type: "IPv4".to_string(),
                endpoints: vec![Endpoint {
                    addresses: vec!["10.42.0.5".to_string()],
                    conditions: Some(EndpointConditions {
                        ready: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ports: Some(vec![EndpointPort {
                    port: Some(8080),
                    ..Default::default()
                }]),
            },
        );
        state
    }

    #[test]
    fn initial_relists_rebuild_once_after_both_snapshots_finish() {
        let mut state = State::default();

        assert!(!state.record_event(true, EventPhase::Init));
        assert!(!state.record_event(true, EventPhase::InitApply));
        assert!(!state.record_event(true, EventPhase::InitDone));
        assert!(!state.record_event(false, EventPhase::Init));
        assert!(!state.record_event(false, EventPhase::InitApply));
        assert!(state.record_event(false, EventPhase::InitDone));
        assert!(!state.dirty);

        // Live changes stay immediate after the initial snapshots are ready.
        assert!(state.record_event(true, EventPhase::Apply));

        // A reconnect batches again until that resource's new snapshot is
        // complete, so stale state is never applied one object at a time.
        assert!(!state.record_event(true, EventPhase::Init));
        assert!(!state.record_event(true, EventPhase::InitApply));
        assert!(state.record_event(true, EventPhase::InitDone));
    }

    /// `build_ruleset()`'s real output — not a hand-written stand-in script
    /// — must itself be valid nftables syntax the kernel accepts (skips
    /// itself without `nft`/CAP_NET_ADMIN, same as `nft_check`'s other
    /// callers).
    #[test]
    fn generated_nft_rules_pass_real_syntax_check() {
        let state = state_with_one_nodeport_service();
        let rs = build_ruleset(&state, IpFamily::V4, LbMethod::Random, Caps::all(), &[]);
        nft_check(&rs)
            .unwrap_or_else(|e| panic!("build_ruleset() output failed real nft syntax check: {e}"));
    }

    /// The point of the whole fallback: NodePort must keep working on a
    /// kernel with no nft_fib, which is where this was actually found.
    #[test]
    fn nodeport_still_works_without_fib_by_matching_local_addresses() {
        let state = state_with_one_nodeport_service();
        let local = vec!["192.168.1.50".to_string(), "10.0.0.1".to_string()];
        let no_fib = Caps {
            fib: false,
            numgen: true,
            jhash: true,
            statistic_v4: false,
            statistic_v6: false,
        };

        let rs = build_ruleset(&state, IpFamily::V4, LbMethod::Random, no_fib, &local);

        assert!(
            !rs.contains("fib "),
            "must not emit fib on a kernel without it:\n{rs}"
        );
        // One NodePort rule per local address, in both hooks.
        for addr in &local {
            assert!(
                rs.contains(&format!(
                    "ip daddr {addr} tcp dport 30080 dnat to 10.42.0.5:8080"
                )),
                "missing NodePort rule for {addr}:\n{rs}"
            );
        }
        // `<f> daddr` establishes the family, so the qualifier fib needed
        // must not appear on these rules.
        assert!(
            !rs.contains("dport 30080 dnat ip to"),
            "qualifier is only for the fib form:\n{rs}"
        );
        // ClusterIP routing is unaffected by the fallback.
        assert!(
            rs.contains("ip daddr 10.43.0.1 tcp dport 80 dnat to 10.42.0.5:8080"),
            "{rs}"
        );
    }

    #[test]
    fn nodeport_uses_fib_when_the_kernel_has_it() {
        let state = state_with_one_nodeport_service();
        let rs = build_ruleset(&state, IpFamily::V4, LbMethod::Random, Caps::all(), &[]);
        assert!(
            rs.contains("fib daddr type local tcp dport 30080 dnat ip to 10.42.0.5:8080"),
            "{rs}"
        );
    }

    /// With no fib AND no local addresses discoverable, NodePort is the only
    /// thing that degrades — ClusterIP must still be programmed.
    #[test]
    fn losing_nodeport_never_costs_clusterip() {
        let state = state_with_one_nodeport_service();
        let no_fib = Caps {
            fib: false,
            numgen: true,
            jhash: true,
            statistic_v4: false,
            statistic_v6: false,
        };
        let rs = build_ruleset(&state, IpFamily::V4, LbMethod::Random, no_fib, &[]);
        assert!(
            rs.contains("ip daddr 10.43.0.1 tcp dport 80 dnat to 10.42.0.5:8080"),
            "{rs}"
        );
        assert!(
            !rs.contains("30080"),
            "no local addresses means no NodePort rules:\n{rs}"
        );
    }

    /// A three-backend Service on a kernel with no nft_numgen must be
    /// programmed through the xtables chain — evenly, using kube-proxy's
    /// own 1/(N-i) scheme, with the last rule unconditional.
    #[test]
    fn statistic_fallback_spreads_evenly_and_terminates() {
        let caps = Caps {
            fib: false,
            numgen: false,
            jhash: false,
            statistic_v4: true,
            statistic_v6: true,
        };
        let mut state = state_with_one_nodeport_service();
        // Widen the single slice to three ready backends.
        let slice = state.endpoint_slices.get_mut("n/s-abc").unwrap();
        slice.endpoints[0].addresses = vec!["10.42.0.5".into()];
        for ip in ["10.42.0.6", "10.42.0.7"] {
            let mut ep = slice.endpoints[0].clone();
            ep.addresses = vec![ip.to_string()];
            slice.endpoints.push(ep);
        }

        let rs = build_statistic_ruleset(&state, IpFamily::V4, caps, Family::V4)
            .expect("three backends must produce a fallback ruleset");

        assert!(rs.starts_with("*nat\n:NOTK8S-SVC - [0:0]\n"), "{rs}");
        assert!(
            rs.contains("-A NOTK8S-SVC -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT"),
            "{rs}"
        );
        assert!(rs.trim_end().ends_with("COMMIT"), "{rs}");
        // 1/3 then 1/2 then unconditional — the scheme that actually splits
        // evenly. A flat 1/N on every rule would drop ~30% off the end.
        assert!(rs.contains("--probability 0.33333333333"), "{rs}");
        assert!(rs.contains("--probability 0.50000000000"), "{rs}");
        let last = rs
            .lines()
            .filter(|l| l.contains("10.42.0.7:8080"))
            .collect::<Vec<_>>();
        assert!(!last.is_empty(), "{rs}");
        assert!(
            last.iter().all(|l| !l.contains("statistic")),
            "the final backend must match unconditionally, or traffic falls off the end:\n{rs}"
        );
        // NodePort comes along for free here — xtables has its own local
        // match, so no address enumeration and no dependence on nft_fib.
        assert!(rs.contains("-m addrtype --dst-type LOCAL"), "{rs}");

        ipt_check(&rs).unwrap_or_else(|e| {
            panic!("build_statistic_ruleset() output failed a real iptables-restore --test: {e}")
        });
    }

    /// And the nft table must NOT also program those Services, or two DNAT
    /// rules race for the same connection.
    #[test]
    fn delegated_services_are_absent_from_the_nft_ruleset() {
        let caps = Caps {
            fib: false,
            numgen: false,
            jhash: false,
            statistic_v4: true,
            statistic_v6: true,
        };
        let mut state = state_with_one_nodeport_service();
        let slice = state.endpoint_slices.get_mut("n/s-abc").unwrap();
        let mut ep = slice.endpoints[0].clone();
        ep.addresses = vec!["10.42.0.6".into()];
        slice.endpoints.push(ep);

        let nft = build_ruleset(
            &state,
            IpFamily::V4,
            LbMethod::Random,
            caps,
            &["10.0.0.1".into()],
        );
        assert!(
            !nft.contains("10.43.0.1"),
            "delegated Service must not appear in nft:\n{nft}"
        );
        assert!(build_statistic_ruleset(&state, IpFamily::V4, caps, Family::V4).is_some());
    }

    /// A single backend needs no selection at all, so it stays in nftables
    /// even on a kernel that has the fallback available.
    #[test]
    fn one_backend_never_uses_the_fallback() {
        let caps = Caps {
            fib: false,
            numgen: false,
            jhash: false,
            statistic_v4: true,
            statistic_v6: true,
        };
        let state = state_with_one_nodeport_service();
        assert!(build_statistic_ruleset(&state, IpFamily::V4, caps, Family::V4).is_none());
        let nft = build_ruleset(
            &state,
            IpFamily::V4,
            LbMethod::Random,
            caps,
            &["10.0.0.1".into()],
        );
        assert!(
            nft.contains("ip daddr 10.43.0.1 tcp dport 80 dnat to 10.42.0.5:8080"),
            "{nft}"
        );
    }

    /// sessionAffinity must survive the statistic fallback existing. A
    /// sticky Service with several backends has to stay pinned in nftables
    /// rather than being handed to a chain that re-randomises per
    /// connection — that would trade one broken guarantee for another.
    #[test]
    fn sticky_services_are_never_handed_to_the_statistic_chain() {
        let caps = Caps {
            fib: false,
            numgen: false,
            jhash: false,
            statistic_v4: true,
            statistic_v6: true,
        };
        let mut state = state_with_one_nodeport_service();
        state.services.insert(
            "n/s".to_string(),
            fake_service(vec!["10.43.0.1"], Some("ClientIP")),
        );
        let slice = state.endpoint_slices.get_mut("n/s-abc").unwrap();
        let mut ep = slice.endpoints[0].clone();
        ep.addresses = vec!["10.42.0.6".into()];
        slice.endpoints.push(ep);

        assert!(
            build_statistic_ruleset(&state, IpFamily::V4, caps, Family::V4).is_none(),
            "a ClientIP-affinity Service must not reach the statistic chain"
        );
        // It stays in nftables, pinned to exactly one backend.
        let nft = build_ruleset(&state, IpFamily::V4, LbMethod::Random, caps, &[]);
        assert!(
            nft.contains("ip daddr 10.43.0.1 tcp dport 80 dnat to 10.42.0.5:8080"),
            "{nft}"
        );
        assert!(!nft.contains("statistic"), "{nft}");
        assert!(!nft.contains("map {"), "{nft}");
    }

    /// The fallback must respect the configured address family. Asking for
    /// v6 rules on a v4-only proxy has to produce nothing at all, not a
    /// chain full of rules for a family this node doesn't serve.
    #[test]
    fn statistic_fallback_ignores_a_family_the_proxy_does_not_serve() {
        let caps = Caps {
            fib: false,
            numgen: false,
            jhash: false,
            statistic_v4: true,
            statistic_v6: true,
        };
        let mut state = state_with_one_nodeport_service();
        let slice = state.endpoint_slices.get_mut("n/s-abc").unwrap();
        let mut ep = slice.endpoints[0].clone();
        ep.addresses = vec!["10.42.0.6".into()];
        slice.endpoints.push(ep);

        assert!(build_statistic_ruleset(&state, IpFamily::V4, caps, Family::V4).is_some());
        assert!(
            build_statistic_ruleset(&state, IpFamily::V4, caps, Family::V6).is_none(),
            "a v4-only proxy must emit no v6 rules"
        );
    }

    /// IPv6 destinations have to be bracketed, or iptables-restore rejects
    /// the whole file and — apply failures being fatal — restart-loops the
    /// proxy. The v4 path can't catch this because it has no brackets.
    #[test]
    fn statistic_fallback_brackets_ipv6_destinations() {
        use k8s_openapi::api::discovery::v1::{Endpoint, EndpointConditions, EndpointPort};
        let caps = Caps {
            fib: false,
            numgen: false,
            jhash: false,
            statistic_v4: true,
            statistic_v6: true,
        };
        let mut state = State::default();
        state
            .services
            .insert("n/s6".to_string(), fake_service(vec!["fd00::1"], None));
        state.endpoint_slices.insert(
            "n/s6-abc".to_string(),
            EndpointSlice {
                metadata: kube::core::ObjectMeta {
                    namespace: Some("n".into()),
                    name: Some("s6-abc".into()),
                    labels: Some([(SVC_NAME_LABEL.to_string(), "s6".to_string())].into()),
                    ..Default::default()
                },
                address_type: "IPv6".to_string(),
                endpoints: ["fd00::5", "fd00::6"]
                    .iter()
                    .map(|ip| Endpoint {
                        addresses: vec![ip.to_string()],
                        conditions: Some(EndpointConditions {
                            ready: Some(true),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })
                    .collect(),
                ports: Some(vec![EndpointPort {
                    port: Some(8080),
                    ..Default::default()
                }]),
            },
        );

        let rs = build_statistic_ruleset(&state, IpFamily::Dual, caps, Family::V6)
            .expect("a dual-stack proxy must emit v6 fallback rules");
        assert!(rs.contains("--to-destination [fd00::5]:8080"), "{rs}");
        assert!(rs.contains("--to-destination [fd00::6]:8080"), "{rs}");
        assert!(
            !rs.contains("--to-destination fd00::"),
            "unbracketed v6 destination:\n{rs}"
        );
        assert!(rs.contains("-d fd00::1"), "{rs}");
    }

    #[test]
    fn family_detection() {
        assert_eq!(Family::of("10.42.0.5"), Family::V4);
        assert_eq!(Family::of("fd00::5"), Family::V6);
    }
}
