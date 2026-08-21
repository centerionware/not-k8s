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
//! **Phase 2 partially done** — `availability` is the real availability
//! controller's *decision logic* (`local`'s own always-`Available`
//! posture, `remote`'s own real pre-flight chain over an already-fetched
//! Service/`EndpointSlice`, both a faithful port of
//! `github.com/kubernetes/kube-aggregator`'s own two controllers, fetched
//! and read directly) — pure, no I/O, not yet wired to a live
//! reconciliation loop that actually watches `APIService`/Service/
//! EndpointSlice objects and writes the resulting condition back
//! (that's the remaining Phase 2 work), and the actual discovery-
//! endpoint dial `remote`'s own controller does *after* pre-flight
//! passes isn't attempted here either (naturally Phase 4's job, the same
//! `proxy::http_client` primitive the eventual reverse proxy needs too).
//! **Phase 4 done — a genuine live reverse proxy, wired into
//! `server::listener::handle`.** `route::resolve` finds the one stored,
//! non-local `APIService` (if any) claiming a request's `(group,
//! version)` (a bounded `LIST`, same cardinality assumption
//! `apiextensions::registry::resolve_in` already makes for CRDs);
//! `server::listener::aggregate_proxy` is the dispatch glue — fetches the
//! backing Service/`EndpointSlice`s, runs the exact same
//! `availability::preflight_check` fresh on every request (this build
//! still has no reconciliation loop caching the resulting condition —
//! Phase 2's remaining gap, below — so this is a real, honest substitute:
//! slower than reading an already-computed condition, never wrong),
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
//! honestly), streaming upgrade support (SPDY/websocket — the same real,
//! separate gap Group N's exec/attach still has), and discovery merge
//! (Phase 3, below — an aggregated group's own `/apis/{group}/{version}`
//! document isn't proxied yet, so `kubectl` won't discover it exists even
//! though a direct resource request to it now actually works).
//!
//! **Phase 2's remaining gap**: no live reconciliation loop yet that
//! actually watches `APIService`/Service/EndpointSlice objects and writes
//! `status.conditions` back — `aggregate_proxy` runs the same pre-flight
//! logic per-request instead (see above), a real working substitute, not
//! a stub, but real upstream's own `status.conditions` on a fetched
//! `APIService` document still won't reflect live availability the way a
//! real cluster's `kubectl get apiservices` output would.
//!
//! **Real build-order correction, found while scoping Phases 3/4**:
//! `docs/APISERVER.md`'s own Group L section explains why Phase 4 (this
//! one) has to land before Phase 3 (discovery merge) despite the
//! numbering — the reverse of every other group's own order so far;
//! Phase 4 landing first is exactly what happened.
//!
//! See `docs/APISERVER.md`'s own Group L section (right after Group K)
//! for the full plan, including how discovery merge (Phase 3) would
//! likely reuse Group K's own `discovery::*_with_crds` shape as a third
//! merge input rather than a third parallel implementation.

pub mod availability;
pub mod client_tls;
pub mod route;
pub mod proxy_target;
