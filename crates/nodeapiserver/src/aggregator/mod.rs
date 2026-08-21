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
//! **Phase 4 partially started** — `proxy_target::resolve` is the pure
//! `spec.service` -> real dial `Target` resolution (`proxy::pod_log::
//! Target` reused directly), choosing real upstream's own
//! `ClusterIP`-dial strategy (`NewClusterIPServiceResolver`, not the
//! endpoint-resolving one) since `crates/nodeproxy` already gives this
//! build real `ClusterIP` routing to rely on — see that module's own
//! doc comment for the full reasoning. Not yet wired to a live dial
//! (`proxy::http_client`) or a real request route in `server::listener`;
//! per-`APIService` TLS trust (`spec.caBundle`/`.insecureSkipTLSVerify`)
//! isn't attempted yet either.
//!
//! **Real build-order correction, found while scoping Phases 3/4**:
//! `docs/APISERVER.md`'s own Group L section explains why Phase 4 (this
//! one) has to land before Phase 3 (discovery merge) despite the
//! numbering — the reverse of every other group's own order so far.
//!
//! See `docs/APISERVER.md`'s own Group L section (right after Group K)
//! for the full plan, including how discovery merge (Phase 3) would
//! likely reuse Group K's own `discovery::*_with_crds` shape as a third
//! merge input rather than a third parallel implementation.

pub mod availability;
pub mod proxy_target;
