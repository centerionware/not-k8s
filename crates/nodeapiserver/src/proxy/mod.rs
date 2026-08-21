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
//! **`pods/log` is now a genuine live proxy, wired into
//! `server::listener`** — `GET .../pods/{name}/log` resolves the target
//! (fetching the pod + its node), dials nodelet for real, and relays
//! its response — status, headers, streaming body (`follow=true`
//! included) — back unmodified. exec/attach/port-forward and
//! node/service proxy subresources aren't started yet, though they'd
//! reuse the same `client_tls`/`http_client` primitives (attach/exec
//! need the upgrade-splice half of `exec.rs`'s own pattern too, which
//! this module doesn't need for a plain GET).
//!
//! Status: `pods/log` done and wired; exec/attach/port-forward and
//! node/service proxy subresources not started (Group N — see
//! docs/APISERVER.md).

pub mod client_tls;
pub mod http_client;
pub mod pod_log;
