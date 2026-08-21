//! The actual live proxy dial: given a [`crate::proxy::pod_log::Target`]
//! and a [`rustls::ClientConfig`] (`client_tls::build_client_config`),
//! opens a real TLS connection to nodelet's kubelet-style server and
//! relays the response back unmodified.
//!
//! Reuses `crates/nodelet/src/server/exec.rs`'s own proven low-level
//! pattern — a raw TCP dial + `hyper::client::conn::http1::handshake`
//! over the connection's IO, rather than `hyper-util`'s higher-level
//! pooled client — just with a TLS layer wrapped around the TCP stream
//! first, since nodelet's own server here is a remote host over mTLS,
//! not a local plaintext socket the way that module's own upstream-CRI
//! proxy target is. No connection reuse/pooling: `pods/log` is a
//! one-shot GET (or a long-lived streamed response for `follow=true`,
//! but still exactly one request on the connection either way), so
//! pooling would add real complexity for no real benefit here.

use crate::proxy::pod_log::Target;
use crate::server::listener::{BoxError, BoxedBody};
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper::{Request, Response, Uri};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use rustls::ClientConfig;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{host} is not a valid TLS server name")]
    InvalidServerName { host: String },
    #[error("connecting to {host}:{port}: {source}")]
    Connect { host: String, port: u16, source: std::io::Error },
    #[error("TLS handshake with {host}:{port} failed: {source}")]
    Tls { host: String, port: u16, source: std::io::Error },
    #[error("HTTP handshake with nodelet failed: {0}")]
    HttpHandshake(#[source] hyper::Error),
    #[error("building the request to nodelet: {0}")]
    BuildRequest(#[from] http::Error),
    #[error("nodelet request failed: {0}")]
    Request(#[source] hyper::Error),
}

/// Dials `target` over a real TLS connection (`client_config` decides
/// server-cert-verification posture and whether a client certificate is
/// presented — see `client_tls`'s own doc comment), issues one `GET`
/// with `target.query` forwarded verbatim, and hands back nodelet's own
/// response — status, headers, and (crucially, for `follow=true`) a
/// still-streaming body — completely unmodified. The caller
/// (`server::listener`) is responsible for anything client-facing this
/// build wants to add on top (it doesn't add anything today: this is a
/// transparent proxy, matching real upstream's own `pods/log` handler,
/// which is also just a transparent reverse proxy to kubelet).
pub async fn fetch(target: &Target, client_config: Arc<ClientConfig>) -> Result<Response<BoxedBody>, Error> {
    let server_name = ServerName::try_from(target.host.clone()).map_err(|_| Error::InvalidServerName { host: target.host.clone() })?;

    let tcp = TcpStream::connect((target.host.as_str(), target.port)).await.map_err(|source| Error::Connect { host: target.host.clone(), port: target.port, source })?;
    let connector = TlsConnector::from(client_config);
    let tls_stream = connector.connect(server_name, tcp).await.map_err(|source| Error::Tls { host: target.host.clone(), port: target.port, source })?;
    let io = TokioIo::new(tls_stream);

    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.map_err(Error::HttpHandshake)?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!(error = ?e, "proxy: nodelet connection ended");
        }
    });

    let uri_str = if target.query.is_empty() { target.path.clone() } else { format!("{}?{}", target.path, target.query) };
    let uri: Uri = uri_str.parse().map_err(|e| Error::BuildRequest(http::Error::from(e)))?;
    let req = Request::builder().method("GET").uri(uri).header(hyper::header::HOST, &target.host).body(Empty::<Bytes>::new().boxed())?;

    let resp = sender.send_request(req).await.map_err(Error::Request)?;
    Ok(resp.map(|incoming| incoming.map_err(|e| Box::new(e) as BoxError).boxed()))
}
