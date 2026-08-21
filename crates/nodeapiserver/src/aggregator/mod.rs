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
//! See `docs/APISERVER.md`'s own Group L section (right after Group K)
//! for the full plan: why this workspace is unusually well-positioned
//! for the reverse-proxy half already (a real, live Service/
//! EndpointSlice watch plus `crates/nodeproxy` in the same repo, and
//! `proxy::http_client`/`proxy::client_tls` — Group N's own
//! already-landed dial-and-relay primitives), and how discovery merge
//! (Phase 3) would likely reuse Group K's own `discovery::*_with_crds`
//! shape as a third merge input rather than a third parallel
//! implementation.

pub mod availability;
