//! Group N: streaming and proxy subresources — exec/attach/port-forward/
//! log spliced through to `nodelet:10250`
//! (`crates/nodelet/src/server/exec.rs`'s proven raw-upgrade-splice
//! pattern), node/service/pod proxy subresources.
//!
//! `pod_log` — real `pods/log` target resolution (`LogLocation`), a
//! faithful port of real upstream's own
//! `pkg/registry/core/pod/strategy.go` + the node connection-info
//! resolution `pkg/kubelet/client/kubelet_client.go` performs.
//!
//! `client_tls` — the `rustls::ClientConfig` this build dials nodelet
//! with: real upstream's own insecure-by-default posture when no
//! kubelet CA is configured, plus an optional client certificate
//! (`NODEAPISERVER_KUBELET_CLIENT_CERT_FILE`/`_KEY_FILE`) that lets
//! nodelet's own x509-client-cert auth path authenticate the connection
//! directly, no `TokenReview` round trip needed.
//!
//! `http_client` — the actual dial: a real TLS connection + one `GET`,
//! reusing `crates/nodelet/src/server/exec.rs`'s own proven low-level
//! `hyper::client::conn::http1::handshake` pattern.
//!
//! **`pods/log` is a genuine live proxy, and pod connection subresources are
//! now upgraded through the same path, wired into `server::listener`** —
//! `GET .../pods/{name}/log` resolves the target (fetching the pod + its
//! node), while `exec`, `attach`, and `portforward` translate their query
//! parameters to nodelet's kubelet-style routes. `http_client::upgrade`
//! forwards the upgrade headers and splices both upgraded connections after
//! the `101` response. Node and Service proxy subresources use the same
//! listener-level relay, while uncommon transport compatibility remains
//! tracked in Group N (`docs/APISERVER.md`).

pub mod client_tls;
pub mod http_client;
pub mod node_proxy;
pub mod pod_log;
pub mod pod_stream;
pub mod service_proxy;
