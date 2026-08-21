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
//! See `docs/APISERVER.md`'s own Group L section (right after Group K)
//! for the real plan for everything else: why this workspace is
//! unusually well-positioned for the reverse-proxy half already (a
//! real, live Service/EndpointSlice watch plus `crates/nodeproxy` in the
//! same repo, and `proxy::http_client`/`proxy::client_tls` — Group N's
//! own already-landed dial-and-relay primitives, architecturally the
//! same shape an `APIService` proxy needs), the real availability
//! controller (Phase 2), and how discovery merge (Phase 3) would likely
//! reuse Group K's own `discovery::*_with_crds` shape as a third merge
//! input rather than a third parallel implementation. Phase 4 (the
//! actual reverse proxy) is the only phase with no code at all yet.
