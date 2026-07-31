//! A kubelet-style HTTPS server: `/containerLogs` (backs `kubectl logs`)
//! and `/exec`, `/attach`, `/portForward` (back `kubectl exec`/`attach`/
//! `port-forward`). `cri` feature only — meaningless without real
//! containers. See docs/GAP_CLOSURE.md for what's simplified here relative
//! to real kubelet (notably: `AlwaysAllow` authorization once a bearer
//! token passes `TokenReview`, matching kubelet's own historical default,
//! not a from-scratch RBAC/SubjectAccessReview implementation).

pub mod auth;
pub mod exec;
pub mod logs;
pub mod prom_metrics;
pub mod routes;
pub mod stats;
pub mod tls;

use crate::config::Config;
use crate::runtime::PodRuntime;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use kube::Client;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

#[derive(Clone)]
pub struct ServerState {
    pub client: Client,
    pub runtime: Arc<dyn PodRuntime>,
    pub node_name: String,
}

/// Run the server forever (until the process exits) on `cfg.server_port`.
/// Best-effort: a failure to bind or generate a TLS cert is logged and the
/// server simply doesn't run — nodelet's core pod-management loop must not
/// depend on this succeeding (matches how every other background loop in
/// main.rs degrades: warn and move on, never take the whole agent down).
pub async fn run(client: Client, runtime: Arc<dyn PodRuntime>, cfg: Config) {
    if !cfg.server_enabled {
        info!("kubelet-style HTTP(S) server disabled (NODELET_SERVER_ENABLED=false)");
        return;
    }

    let cert = match tls::load_or_generate(&cfg.server_cert_dir, &cfg.node_name) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = ?e, "failed to load/generate a TLS certificate; exec/logs/attach/port-forward server will not run");
            return;
        }
    };
    let acceptor = TlsAcceptor::from(Arc::new(cert.server_config()));

    let addr: SocketAddr = match format!("0.0.0.0:{}", cfg.server_port).parse() {
        Ok(a) => a,
        Err(e) => {
            warn!(error = ?e, "invalid NODELET_SERVER_PORT");
            return;
        }
    };
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            warn!(%addr, error = ?e, "failed to bind kubelet-style server port; exec/logs/attach/port-forward will not work");
            return;
        }
    };
    info!(%addr, "kubelet-style HTTP(S) server listening (containerLogs/exec/attach/portForward)");

    let state = Arc::new(ServerState { client, runtime, node_name: cfg.node_name.clone() });

    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = ?e, "server: accept failed");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(tcp).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(%peer, error = ?e, "server: TLS handshake failed");
                    return;
                }
            };
            let io = TokioIo::new(tls_stream);
            let service = hyper::service::service_fn(move |req| routes::handle(state.clone(), req));
            if let Err(e) = ConnBuilder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(io, service)
                .await
            {
                // Connection resets/aborts from a client going away mid-exec
                // are routine, not actionable — debug-level, not a warning.
                tracing::debug!(%peer, error = ?e, "server: connection ended");
            }
        });
    }
}

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type BoxedBody = http_body_util::combinators::BoxBody<hyper::body::Bytes, BoxError>;

/// Build a plain-text error/status response — used by every handler for
/// anything that isn't the happy path (auth failure, pod/container not
/// found, upstream CRI error, ...).
pub(crate) fn text_response(status: hyper::StatusCode, msg: impl Into<String>) -> hyper::Response<BoxedBody> {
    hyper::Response::builder()
        .status(status)
        .header("Content-Type", "text/plain")
        .body(body_from_bytes(msg.into().into_bytes()))
        .unwrap()
}

/// Wrap a fully-materialized byte buffer as a `BoxedBody` — the common case
/// for every handler that isn't streaming (`logs::handle_container_logs`'s
/// `follow` mode is the one exception).
pub(crate) fn body_from_bytes(bytes: Vec<u8>) -> BoxedBody {
    use http_body_util::{BodyExt, Full};
    Full::new(hyper::body::Bytes::from(bytes)).map_err(|never: std::convert::Infallible| match never {}).boxed()
}
