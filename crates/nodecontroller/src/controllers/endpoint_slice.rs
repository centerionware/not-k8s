//! endpointslice-controller (Group B, service routing): watches Service +
//! Pod, produces EndpointSlices. Pure event — a Pod either matches a
//! Service's selector or it doesn't, nothing to poll.
//!
//! `nodeproxy`'s `svc.rs` only *consumes* EndpointSlices (Service +
//! EndpointSlice watch, no periodic resync) — this is the piece that
//! *produces* them. Without it, disabling k3s's real controller-manager
//! leaves nodeproxy with nothing to watch and Services stop routing
//! entirely, the single highest-impact gap in the whole crate (see
//! docs/CONTROLLER_MANAGER.md, Group B).
//!
//! # Scope of this slice
//!
//! Implements the modern `discovery.k8s.io/v1` `EndpointSlice` path only —
//! the one `nodeproxy` and every current Kubernetes version actually uses.
//! **Not implemented**: the legacy `v1.Endpoints` object
//! (`endpoints-controller`) — nothing in this project reads it, so it's
//! not worth a second, parallel object-production path yet; and
//! `endpointslice-mirroring-controller` (mirrors a *user-created*
//! `Endpoints` object into EndpointSlices) — a niche path for callers that
//! still write legacy Endpoints by hand. Both are named explicitly, not
//! silently dropped.
//!
//! **One EndpointSlice per Service**, not upstream's size-limited,
//! multi-slice-per-Service scheme (upstream splits at 100 endpoints per
//! slice by default). A single slice is spec-legal at any size and simpler
//! to keep correct; splitting for scale is additive, not a rework, if a
//! real deployment ever needs it — this project's own target is ordinary
//! multi-node clusters, not hyperscale.
//!
//! **No owner-reference cascade delete**: `garbage-collector-controller`
//! (Group D) isn't implemented yet, so a Service delete explicitly deletes
//! its own EndpointSlice here rather than relying on Kubernetes' owner-ref
//! GC to do it — the `ownerReferences` entry is still written (for the day
//! GC exists, and so `kubectl` shows the real relationship), just not
//! relied upon yet.

use anyhow::{Context, Result};
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Pod, Service, ServicePort};
use k8s_openapi::api::discovery::v1::{Endpoint, EndpointConditions, EndpointPort, EndpointSlice};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::{BTreeMap, HashMap};

const SERVICE_NAME_LABEL: &str = "kubernetes.io/service-name";
const MANAGED_BY_LABEL: &str = "endpointslice.kubernetes.io/managed-by";
const MANAGED_BY_VALUE: &str = "nodecontroller";

/// Does `selector` match `labels`? Every key in `selector` must be present
/// in `labels` with an equal value. An empty/absent selector means "this
/// Service isn't ours to manage" (externally-managed endpoints) — the
/// caller is expected to have already filtered those out, not this
/// function, so an empty selector deliberately matches nothing here rather
/// than everything.
pub fn selector_matches(selector: &BTreeMap<String, String>, labels: &BTreeMap<String, String>) -> bool {
    if selector.is_empty() {
        return false;
    }
    selector.iter().all(|(k, v)| labels.get(k) == Some(v))
}

/// A Pod's endpoint conditions, upstream's own rule: `ready` mirrors the
/// Pod's own `Ready` condition and nothing else; `serving` equals `ready`
/// for a live (non-deleting) Pod (this project doesn't yet implement
/// `publishNotReadyAddresses` — every Service is treated as if it were
/// unset, upstream's default); `terminating` is "has a deletionTimestamp".
pub fn endpoint_conditions(pod_ready: bool, deleting: bool) -> EndpointConditions {
    EndpointConditions {
        ready: Some(pod_ready && !deleting),
        serving: Some(pod_ready),
        terminating: Some(deleting),
    }
}

fn pod_ready(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .into_iter()
        .flatten()
        .any(|c| c.type_ == "Ready" && c.status == "True")
}

/// Resolves one `ServicePort.targetPort` against a representative Pod's
/// container ports. `None` (unset) means "same as `port`" per the API's own
/// documented default. A named `targetPort` is looked up by name across
/// every container's `ports` list — the first match wins, matching
/// upstream's own tie-break (name uniqueness across a Pod's containers is
/// the caller's responsibility, not something either controller enforces).
/// Falls back to `service_port` itself if a named port genuinely isn't
/// found, rather than dropping the endpoint — a Service is more useful
/// pointed at *a* plausible port than not routed at all.
fn resolve_target_port(target_port: Option<&IntOrString>, service_port: i32, sample_pod: Option<&Pod>) -> i32 {
    match target_port {
        None => service_port,
        Some(IntOrString::Int(n)) => *n,
        Some(IntOrString::String(name)) => sample_pod
            .and_then(|p| p.spec.as_ref())
            .into_iter()
            .flat_map(|s| &s.containers)
            .filter_map(|c| c.ports.as_ref())
            .flatten()
            .find(|cp| cp.name.as_deref() == Some(name.as_str()))
            .map(|cp| cp.container_port)
            .unwrap_or(service_port),
    }
}

/// Builds the `ports` list for an EndpointSlice from a Service's own
/// `spec.ports`, resolved against one representative Pod (any matching Pod
/// — this assumes every matching Pod exposes the same named ports at the
/// same numbers, the common case; a Service whose Pods disagree gets the
/// first Pod's resolution for all of them, a known simplification against
/// upstream's per-Pod-group splitting).
pub fn build_ports(service_ports: &[ServicePort], sample_pod: Option<&Pod>) -> Vec<EndpointPort> {
    service_ports
        .iter()
        .map(|sp| EndpointPort {
            name: sp.name.clone(),
            port: Some(resolve_target_port(sp.target_port.as_ref(), sp.port, sample_pod)),
            protocol: Some(sp.protocol.clone().unwrap_or_else(|| "TCP".to_string())),
            app_protocol: sp.app_protocol.clone(),
        })
        .collect()
}

/// Builds one `Endpoint` per matching, IP-assigned Pod. Pods with no
/// `status.podIP` yet (not yet scheduled/started) are the caller's job to
/// have already filtered out — this only knows how to describe a Pod that
/// has one.
pub fn build_endpoint(pod: &Pod, pod_ip: &str) -> Endpoint {
    let deleting = pod.metadata.deletion_timestamp.is_some();
    Endpoint {
        addresses: vec![pod_ip.to_string()],
        conditions: Some(endpoint_conditions(pod_ready(pod), deleting)),
        node_name: pod.spec.as_ref().and_then(|s| s.node_name.clone()),
        target_ref: Some(k8s_openapi::api::core::v1::ObjectReference {
            kind: Some("Pod".to_string()),
            name: Some(pod.name_any()),
            namespace: pod.namespace(),
            uid: pod.uid(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn slice_name(service_name: &str) -> String {
    format!("{service_name}-nc")
}

/// Whether this Service is ours to manage: has a real, non-empty pod
/// selector. `ExternalName` Services and Services with externally-managed
/// endpoints (a hand-written `Endpoints`/`EndpointSlice`, no `selector`)
/// are deliberately left alone.
pub fn is_managed(service: &Service) -> bool {
    service
        .spec
        .as_ref()
        .and_then(|s| s.selector.as_ref())
        .is_some_and(|sel| !sel.is_empty())
}

async fn reconcile_service(client: &Client, namespace: &str, name: &str, pod_cache: &HashMap<String, Pod>) {
    let svc_api: Api<Service> = Api::namespaced(client.clone(), namespace);
    let slice_api: Api<EndpointSlice> = Api::namespaced(client.clone(), namespace);

    let service = match svc_api.get_opt(name).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            // Gone — remove any slice we own for it (no GC controller to
            // do this via ownerReferences yet, see this module's header).
            let _ = slice_api.delete(&slice_name(name), &Default::default()).await;
            return;
        }
        Err(e) => {
            tracing::warn!(namespace = %namespace, service = %name, error = ?e, "failed to read Service for endpointslice reconcile");
            return;
        }
    };

    if !is_managed(&service) {
        // Not (or no longer) ours — if we previously created a slice for
        // it (e.g. its selector was just cleared), drop it.
        let _ = slice_api.delete(&slice_name(name), &Default::default()).await;
        return;
    }
    let selector = service.spec.as_ref().and_then(|s| s.selector.clone()).unwrap_or_default();
    let service_ports = service.spec.as_ref().and_then(|s| s.ports.clone()).unwrap_or_default();

    let matching: Vec<&Pod> = pod_cache
        .values()
        .filter(|p| p.namespace().as_deref() == Some(namespace))
        .filter(|p| selector_matches(&selector, p.metadata.labels.as_ref().unwrap_or(&BTreeMap::new())))
        .collect();

    let sample_pod = matching.first().copied();
    let ports = build_ports(&service_ports, sample_pod);
    let endpoints: Vec<Endpoint> = matching
        .iter()
        .filter_map(|p| p.status.as_ref().and_then(|s| s.pod_ip.as_ref()).map(|ip| build_endpoint(p, ip)))
        .collect();

    let mut labels = BTreeMap::new();
    labels.insert(SERVICE_NAME_LABEL.to_string(), name.to_string());
    labels.insert(MANAGED_BY_LABEL.to_string(), MANAGED_BY_VALUE.to_string());

    let owner = OwnerReference {
        api_version: "v1".to_string(),
        kind: "Service".to_string(),
        name: name.to_string(),
        uid: service.uid().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
        ..Default::default()
    };

    let slice = EndpointSlice {
        address_type: "IPv4".to_string(),
        endpoints,
        ports: Some(ports),
        metadata: ObjectMeta {
            name: Some(slice_name(name)),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
    };

    // TEMPORARY diagnostic — chasing a real, reproducible failure where
    // the EndpointSlice never carries the matching Pod's address; narrow
    // back down once the cause is confirmed from a live run (see
    // node_lifecycle.rs's own diagnostic-logging precedent, which found
    // two real bugs this same way).
    tracing::info!(
        namespace = %namespace,
        service = %name,
        pod_cache_size = pod_cache.len(),
        matching_pods = matching.len(),
        endpoints = slice.endpoints.len(),
        "reconciling EndpointSlice"
    );

    // Server-side apply: idempotent create-or-update, no read-modify-write
    // race with a concurrent reconcile of the same Service (the same
    // pattern nodelet's own node registration uses).
    if let Err(e) = slice_api
        .patch(&slice_name(name), &PatchParams::apply(MANAGED_BY_VALUE).force(), &Patch::Apply(&slice))
        .await
    {
        tracing::warn!(namespace = %namespace, service = %name, error = ?e, "failed to apply EndpointSlice");
    } else {
        tracing::info!(namespace = %namespace, service = %name, "applied EndpointSlice");
    }
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let mut services: HashMap<(String, String), ()> = HashMap::new();
    let mut pods: HashMap<String, Pod> = HashMap::new();

    // Seed both caches from what's already there before watching, same
    // "the objects are the durable record" rule every other controller in
    // this crate uses.
    let svc_api: Api<Service> = Api::all(client.clone());
    let pod_api: Api<Pod> = Api::all(client.clone());
    let existing_pods = pod_api.list(&Default::default()).await.context("listing Pods to seed endpointslice-controller")?;
    for p in existing_pods.items {
        pods.insert(pod_key(&p), p);
    }
    let existing_services = svc_api.list(&Default::default()).await.context("listing Services to seed endpointslice-controller")?;
    for s in &existing_services.items {
        if !is_managed(s) {
            continue;
        }
        let ns = s.namespace().unwrap_or_default();
        let name = s.name_any();
        services.insert((ns.clone(), name.clone()), ());
        reconcile_service(&client, &ns, &name, &pods).await;
    }

    let mut svc_stream = crate::watch::watch_services(&client);
    let mut pod_stream = crate::watch::watch_pods(&client);

    loop {
        tokio::select! {
            ev = svc_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(svc))) | Some(Ok(Event::InitApply(svc))) => {
                        let ns = svc.namespace().unwrap_or_default();
                        let name = svc.name_any();
                        services.insert((ns.clone(), name.clone()), ());
                        reconcile_service(&client, &ns, &name, &pods).await;
                    }
                    Some(Ok(Event::Delete(svc))) => {
                        let ns = svc.namespace().unwrap_or_default();
                        let name = svc.name_any();
                        services.remove(&(ns.clone(), name.clone()));
                        reconcile_service(&client, &ns, &name, &pods).await;
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "service watch error in endpointslice-controller"),
                    None => return Ok(()),
                }
            }
            ev = pod_stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(pod))) | Some(Ok(Event::InitApply(pod))) => {
                        let ns = pod.namespace().unwrap_or_default();
                        let labels = pod.metadata.labels.clone().unwrap_or_default();
                        pods.insert(pod_key(&pod), pod);
                        reconcile_affected_services(&client, &ns, &labels, &services, &pods).await;
                    }
                    Some(Ok(Event::Delete(pod))) => {
                        let ns = pod.namespace().unwrap_or_default();
                        let labels = pod.metadata.labels.clone().unwrap_or_default();
                        pods.remove(&pod_key(&pod));
                        reconcile_affected_services(&client, &ns, &labels, &services, &pods).await;
                    }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "pod watch error in endpointslice-controller"),
                    None => return Ok(()),
                }
            }
        }
    }
}

fn pod_key(pod: &Pod) -> String {
    format!("{}/{}", pod.namespace().unwrap_or_default(), pod.name_any())
}

/// Re-reconciles every managed Service in `namespace` — a Pod event doesn't
/// know which Service(s) it belongs to (that's a selector match, not a
/// field on the Pod), so this recomputes every candidate in the same
/// namespace against the current cache. Simple and correct; scoped to one
/// namespace's Service count, not the whole cluster's.
async fn reconcile_affected_services(
    client: &Client,
    namespace: &str,
    _pod_labels: &BTreeMap<String, String>,
    services: &HashMap<(String, String), ()>,
    pods: &HashMap<String, Pod>,
) {
    for (ns, name) in services.keys() {
        if ns == namespace {
            reconcile_service(client, ns, name, pods).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{Container, PodSpec, PodStatus};

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn an_empty_selector_matches_nothing() {
        assert!(!selector_matches(&BTreeMap::new(), &labels(&[("app", "web")])));
    }

    #[test]
    fn a_selector_matches_a_superset_of_labels() {
        let selector = labels(&[("app", "web")]);
        let pod_labels = labels(&[("app", "web"), ("tier", "frontend")]);
        assert!(selector_matches(&selector, &pod_labels));
    }

    #[test]
    fn a_selector_does_not_match_a_wrong_value() {
        let selector = labels(&[("app", "web")]);
        let pod_labels = labels(&[("app", "other")]);
        assert!(!selector_matches(&selector, &pod_labels));
    }

    #[test]
    fn a_selector_does_not_match_a_missing_key() {
        let selector = labels(&[("app", "web"), ("tier", "frontend")]);
        let pod_labels = labels(&[("app", "web")]);
        assert!(!selector_matches(&selector, &pod_labels));
    }

    #[test]
    fn a_ready_non_deleting_pod_is_ready_and_serving_not_terminating() {
        let c = endpoint_conditions(true, false);
        assert_eq!(c.ready, Some(true));
        assert_eq!(c.serving, Some(true));
        assert_eq!(c.terminating, Some(false));
    }

    #[test]
    fn a_not_ready_pod_is_not_ready_or_serving() {
        let c = endpoint_conditions(false, false);
        assert_eq!(c.ready, Some(false));
        assert_eq!(c.serving, Some(false));
    }

    #[test]
    fn a_deleting_pod_is_terminating_and_not_ready_even_if_it_was() {
        let c = endpoint_conditions(true, true);
        assert_eq!(c.ready, Some(false)); // ready requires both healthy AND not deleting
        assert_eq!(c.serving, Some(true)); // still serving while it drains
        assert_eq!(c.terminating, Some(true));
    }

    #[test]
    fn target_port_unset_falls_back_to_the_service_port() {
        assert_eq!(resolve_target_port(None, 80, None), 80);
    }

    #[test]
    fn target_port_numeric_is_used_directly() {
        assert_eq!(resolve_target_port(Some(&IntOrString::Int(8080)), 80, None), 8080);
    }

    fn pod_with_container_port(name: &str, port: i32) -> Pod {
        Pod {
            spec: Some(PodSpec {
                containers: vec![Container {
                    ports: Some(vec![k8s_openapi::api::core::v1::ContainerPort {
                        name: Some(name.to_string()),
                        container_port: port,
                        ..Default::default()
                    }]),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn target_port_named_resolves_against_the_sample_pod() {
        let pod = pod_with_container_port("http", 8080);
        assert_eq!(resolve_target_port(Some(&IntOrString::String("http".to_string())), 80, Some(&pod)), 8080);
    }

    #[test]
    fn target_port_named_but_not_found_falls_back_to_the_service_port() {
        let pod = pod_with_container_port("grpc", 9000);
        assert_eq!(resolve_target_port(Some(&IntOrString::String("http".to_string())), 80, Some(&pod)), 80);
    }

    #[test]
    fn a_service_with_no_selector_is_not_managed() {
        let svc = Service::default();
        assert!(!is_managed(&svc));
    }

    #[test]
    fn a_service_with_a_selector_is_managed() {
        let svc = Service {
            spec: Some(k8s_openapi::api::core::v1::ServiceSpec {
                selector: Some(labels(&[("app", "web")])),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(is_managed(&svc));
    }

    #[test]
    fn build_endpoint_carries_the_pod_ip_and_node_name() {
        let mut pod = pod_with_container_port("http", 8080);
        pod.spec.as_mut().unwrap().node_name = Some("node-a".to_string());
        pod.status = Some(PodStatus { pod_ip: Some("10.42.0.5".to_string()), ..Default::default() });
        let ep = build_endpoint(&pod, "10.42.0.5");
        assert_eq!(ep.addresses, vec!["10.42.0.5".to_string()]);
        assert_eq!(ep.node_name.as_deref(), Some("node-a"));
    }
}
