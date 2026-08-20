//! The REST/watch listener: binds `cfg.bind_addr` over TLS, accepts
//! connections, and serves them with hyper's auto (h1/h2) connection
//! builder — h2 matters here in a way it didn't for nodelet's own HTTPS
//! server (`CLAUDE.md`: client-go and kubectl negotiate h2, and watch
//! multiplexing depends on it). Structure mirrors
//! `crates/nodelet/src/server/mod.rs`'s own `run()` closely — same
//! accept-loop/TLS-handshake/spawn-per-connection shape, adapted for a
//! listener with no reason to ever be individually disabled the way
//! nodelet's exec/logs server is.
//!
//! **The request handler here is a bring-up stub, not the REST dispatch.**
//! It answers `/healthz` and otherwise echoes the parsed
//! [`crate::server::path::RequestInfo`] as JSON — enough to prove the
//! listener, TLS, and path grammar all work together end to end, which is
//! exactly what this milestone is for. The real handler chain
//! (authentication -> authorization -> priority-and-fairness -> admission
//! -> REST, `docs/APISERVER.md`'s own hard requirement) replaces this once
//! Groups H-J exist to fill it in.

use crate::config::Config;
use crate::server::path;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type BoxedBody = http_body_util::combinators::BoxBody<hyper::body::Bytes, BoxError>;

fn body_from_bytes(bytes: Vec<u8>) -> BoxedBody {
    use http_body_util::{BodyExt, Full};
    Full::new(hyper::body::Bytes::from(bytes)).map_err(|never: std::convert::Infallible| match never {}).boxed()
}

fn json_response(status: StatusCode, value: &serde_json::Value) -> Response<BoxedBody> {
    let bytes = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder().status(status).header("Content-Type", "application/json").body(body_from_bytes(bytes)).unwrap()
}

/// Runs the listener forever (until the process exits). Best-effort on
/// bind/TLS failure — logs and returns rather than panicking, matching
/// every other background loop's degrade-and-continue posture in this
/// workspace (see `crates/nodelet/src/server/mod.rs::run`'s own doc
/// comment for the precedent).
pub async fn run(cfg: Config) {
    let cert_dir = std::path::PathBuf::from("/var/lib/nodeapiserver/pki");
    let sans = vec!["localhost".to_string(), "127.0.0.1".to_string(), "kubernetes".to_string(), "kubernetes.default".to_string()];
    let cert = match super::tls::load_or_generate(&cert_dir, &sans) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = ?e, "failed to load/generate a TLS certificate; the REST/watch listener will not run");
            return;
        }
    };
    let server_config = match cert.server_config() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = ?e, "failed to build TLS server config; the REST/watch listener will not run");
            return;
        }
    };
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let addr: SocketAddr = match cfg.bind_addr.parse() {
        Ok(a) => a,
        Err(e) => {
            warn!(bind_addr = %cfg.bind_addr, error = ?e, "invalid NODEAPISERVER_BIND_ADDR");
            return;
        }
    };
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            warn!(%addr, error = ?e, "failed to bind the REST/watch listener port");
            return;
        }
    };
    info!(%addr, "nodeapiserver: REST/watch listener up (bring-up handler only — see server::listener's own doc comment)");

    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = ?e, "listener: accept failed");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(tcp).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(%peer, error = ?e, "listener: TLS handshake failed");
                    return;
                }
            };
            let io = TokioIo::new(tls_stream);
            let service = hyper::service::service_fn(handle);
            if let Err(e) = ConnBuilder::new(TokioExecutor::new()).serve_connection(io, service).await {
                tracing::debug!(%peer, error = ?e, "listener: connection ended");
            }
        });
    }
}

async fn handle(req: Request<Incoming>) -> Result<Response<BoxedBody>, Infallible> {
    let method = req.method().as_str().to_string();
    let path_str = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();

    if path_str == "/healthz" {
        return Ok(Response::builder().status(StatusCode::OK).header("Content-Type", "text/plain").body(body_from_bytes(b"ok".to_vec())).unwrap());
    }

    let info = path::parse(&method, &path_str, &query);
    let value = serde_json::json!({
        "isResourceRequest": info.is_resource_request,
        "verb": info.verb,
        "apiPrefix": info.api_prefix,
        "apiGroup": info.api_group,
        "apiVersion": info.api_version,
        "namespace": info.namespace,
        "resource": info.resource,
        "subresource": info.subresource,
        "name": info.name,
    });
    Ok(json_response(StatusCode::OK, &value))
}
