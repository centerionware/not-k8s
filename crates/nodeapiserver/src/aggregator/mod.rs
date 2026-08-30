//! `APIService` objects, the availability controller, reverse proxying
//! to aggregated API servers, discovery merge.
//!
//! Status: **Phase 1 done** — `APIService` is a real, working
//! generic-REST resource (`tests/apiservice_roundtrip.rs` proves
//! create/get/list/update/delete end to end against a real `nodestore`,
//! zero new application code needed once its own compiled schema
//! existed; `vendor/refresh.sh`'s proto-fetch glob was missing
//! `k8s.io/kube-aggregator` entirely, the real reason it didn't already
//! work — fixed by vendoring that package's `generated.proto` directly).
//! **Phase 2 done** — `availability` is the real availability
//! controller's *decision logic* (`local`'s own always-`Available`
//! posture, `remote`'s own real pre-flight chain over an already-fetched
//! Service/`EndpointSlice`, both a faithful port of
//! `github.com/kubernetes/kube-aggregator`'s own two controllers, fetched
//! and read directly); `reconcile::reconcile_once` is the live loop that
//! actually runs it — lists every stored `APIService`, runs pre-flight
//! plus (once that passes) a real discovery-endpoint dial
//! (`proxy::http_client::fetch`, a named, honest single-dial
//! simplification of upstream's own 5-concurrent-probe check — see
//! `reconcile`'s own doc comment), and writes the resulting `Available`
//! condition to `status.conditions` via `rest::update_status`. Spawned
//! as a periodic background task from `server::listener::run` (best
//! effort, same posture the cache-registry spawn loop already has).
//! Its written condition is now consulted too
//! (`availability::cached_available`): `route::discoverable_group_versions`
//! trusts a decisive cached answer outright (zero extra I/O either way);
//! `server::listener::aggregate_proxy` short-circuits straight to `503`
//! on a cached `Available: False` (skipping the Service/`EndpointSlice`
//! fetch), but still runs the full fresh check on `True`/unknown, since
//! it needs the backing Service fetched regardless to resolve the dial
//! target — this only ever saves the *negative*-path I/O, not the
//! positive one.
//! **Phase 4 done — a genuine live reverse proxy, wired into
//! `server::listener::handle`.** `route::resolve` finds the one stored,
//! non-local `APIService` (if any) claiming a request's `(group,
//! version)` (a bounded `LIST`, same cardinality assumption
//! `apiextensions::registry::resolve_in` already makes for CRDs);
//! `server::listener::aggregate_proxy` is the dispatch glue — fetches the
//! backing Service/`EndpointSlice`s, runs the exact same
//! `availability::preflight_check` fresh on every request when a positive
//! cached condition still needs backing Service data to resolve the dial
//! target (a deliberate freshness check),
//! resolves the dial target (`proxy_target::resolve`), builds this
//! backend's own TLS trust (`client_tls::build_client_config` — real
//! `spec.caBundle`/`.insecureSkipTLSVerify` semantics, `webpki-roots` for
//! real upstream's own "system trust roots" default when neither is
//! set), and relays the whole request — method, headers, body — through
//! `proxy::http_client::relay` (the method/header/body-forwarding sibling
//! `fetch` needed, since an aggregated backend is a real transparent
//! proxy for arbitrary verbs, not `pods/log`'s one fixed GET). **Not
//! attempted**: this build presenting its own client identity to the
//! backend (real upstream's own front-proxy `X-Remote-User`/`--proxy-
//! client-cert-file` chain — `client_tls`'s own doc comment names this
//! honestly), and streaming upgrade support (SPDY/websocket — the same
//! real, separate gap Group N's exec/attach still has).
//!
//! **Phase 3 done, including live resource enumeration.**
//! `aggregator::route::discoverable_group_versions` lists every stored,
//! non-local `APIService`, runs the same real pre-flight check
//! `aggregate_proxy` runs before a dial (fresh every call, since the
//! backing endpoint may change independently), and returns the
//! `(group, version)` pairs that currently pass; `server::discovery::
//! merged_group_version_map` takes this as a third merge input alongside
//! the static table and Group K's CRD-sourced one, wired into `/apis`/
//! `/apis/{group}` (both discovery shapes — legacy and `apidiscovery.
//! k8s.io/v2`). **`/apis/{group}/{version}`'s own `APIResourceList` is
//! now a real live proxied fetch too** — `server::listener::handle`
//! catches a `route_discovery` `NotFound` for exactly that path shape,
//! checks whether the `(group, version)` is one of the pre-flight-passing
//! aggregated pairs already fetched for this request, and if so dials
//! the backend's own `/apis/{group}/{version}` through the same
//! `aggregate_proxy` Phase 4 already built — real upstream's own
//! `checkAPIService`-adjacent posture, reusing the identical dial
//! machinery rather than a second implementation. Net effect: `kubectl
//! api-resources`/`kubectl get <aggregated-resource>` now both work
//! against a real aggregated backend, the same as `kubectl
//! api-versions` already did.
//!
//! **Real build-order correction, found while scoping Phases 3/4**:
//! `docs/APISERVER.md`'s own Group L section explains why Phase 4 (this
//! one) has to land before Phase 3 (discovery merge) despite the
//! numbering — the reverse of every other group's own order so far;
//! Phase 4 landing first is exactly what happened.
//!
//! See `docs/APISERVER.md`'s own Group L section (right after Group K)
//! for the full current behavior, including how discovery merge (Phase 3)
//! reuses Group K's `discovery::*_with_crds` shape as a third merge input
//! and obtains aggregated resources from the backend at request time.

pub mod availability;
pub mod client_tls;
pub mod reconcile;
pub mod route;
pub mod proxy_target;
