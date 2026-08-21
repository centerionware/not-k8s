//! `APIService` objects, the availability controller, reverse proxying
//! to aggregated API servers, discovery merge.
//!
//! Status: **design pass done, no code yet** — see `docs/APISERVER.md`'s
//! own Group L section (right after Group K) for the real plan, grounded
//! directly in `k8s.io/kube-aggregator`'s own `pkg/apis/apiregistration/
//! v1/types.go` + `pkg/apiserver/handler_proxy.go`: what an `APIService`
//! really is, why this workspace is unusually well-positioned for the
//! reverse-proxy half already (a real, live Service/EndpointSlice watch
//! plus `crates/nodeproxy` in the same repo, and `proxy::http_client`/
//! `proxy::client_tls` — Group N's own already-landed dial-and-relay
//! primitives, architecturally the same shape an `APIService` proxy
//! needs), the real availability controller, and how discovery merge
//! would likely reuse Group K's own `discovery::*_with_crds` shape as a
//! third merge input rather than a third parallel implementation.
