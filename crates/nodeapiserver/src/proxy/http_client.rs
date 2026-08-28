//! The actual live proxy dial: given a [`crate::proxy::pod_log::Target`]
//! and a [`rustls::ClientConfig`] (`client_tls::build_client_config`),
//! opens a connection to nodelet's kubelet-style server or a plaintext
//! Service ClusterIP and relays the response back unmodified.
//!
//! Reuses `crates/nodelet/src/server/exec.rs`'s own proven low-level
//! pattern — a raw TCP dial + `hyper::client::conn::http1::handshake`
//! over the connection's IO, rather than `hyper-util`'s higher-level
//! pooled client — wrapping the TCP stream in TLS for nodelet targets and
//! using it directly for Service targets. No connection reuse/pooling: this is a
//! one-shot request (or a long-lived streamed response for `follow=true`,
//! but still exactly one request on the connection either way), so
//! pooling would add real complexity for no real benefit here.

use crate::proxy::pod_log::Target;
use crate::server::listener::{BoxError, BoxedBody};
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Bytes;
use hyper::{Request, Response, StatusCode, Uri};
use hyper::body::Incoming;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use rustls::ClientConfig;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
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
    #[error("unsupported proxy target scheme {0:?}")]
    InvalidScheme(String),
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
    let mut sender = dial(target, client_config).await?;
    let uri = build_uri(target)?;
    let req = Request::builder().method("GET").uri(uri).header(hyper::header::HOST, &target.host).body(Empty::<Bytes>::new().boxed())?;
    let resp = sender.send_request(req).await.map_err(Error::Request)?;
    Ok(resp.map(|incoming| incoming.map_err(|e| Box::new(e) as BoxError).boxed()))
}

/// The generalized sibling [`fetch`] doesn't need: any method, a real
/// request body, and the caller's own already-filtered header set
/// forwarded verbatim — `aggregator`'s reverse proxy needs all three
/// (an aggregated backend is a real transparent proxy for the whole
/// group-version, not one fixed GET-only endpoint the way `pods/log`
/// is), while `fetch` stays exactly as it was for that one caller.
/// `headers` excludes `Host` (set explicitly from `target.host`, same as
/// `fetch`) and any hop-by-hop header — the caller's job, not this
/// function's, since what counts as hop-by-hop is a request-parsing
/// concern, not a dialing one.
pub async fn relay(target: &Target, client_config: Arc<ClientConfig>, method: &str, headers: &[(String, String)], body: Vec<u8>) -> Result<Response<BoxedBody>, Error> {
    let mut sender = dial(target, client_config).await?;
    let uri = build_uri(target)?;
    let mut builder = Request::builder().method(method).uri(uri).header(hyper::header::HOST, &target.host);
    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let req = builder.body(Full::new(Bytes::from(body)).boxed())?;
    let resp = sender.send_request(req).await.map_err(Error::Request)?;
    Ok(resp.map(|incoming| incoming.map_err(|e| Box::new(e) as BoxError).boxed()))
}

/// Proxy a Kubernetes exec/attach/port-forward request through nodelet's
/// kubelet-style HTTPS endpoint. These APIs are connection upgrades rather
/// than ordinary request/response bodies: the target must receive the
/// client's upgrade headers, and both upgraded byte streams must remain
/// connected after the `101 Switching Protocols` response is returned.
pub async fn upgrade(mut req: Request<Incoming>, target: &Target, client_config: Arc<ClientConfig>) -> Result<Response<BoxedBody>, Error> {
    let mut sender = dial_upgrade(target, client_config).await?;
    let target_uri = build_uri(target)?;
    let client_upgrade = hyper::upgrade::on(&mut req);
    let (parts, body) = req.into_parts();

    let mut builder = Request::builder().method(parts.method).uri(target_uri).header(hyper::header::HOST, &target.host);
    for (name, value) in &parts.headers {
        if name != hyper::header::HOST {
            builder = builder.header(name, value);
        }
    }
    let outgoing = builder.body(body.map_err(|e| Box::new(e) as BoxError).boxed())?;
    let mut target_response = sender.send_request(outgoing).await.map_err(Error::Request)?;

    if target_response.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Ok(target_response.map(|incoming| incoming.map_err(|e| Box::new(e) as BoxError).boxed()));
    }

    let target_upgrade = hyper::upgrade::on(&mut target_response);
    let mut response = Response::builder().status(target_response.status());
    for (name, value) in target_response.headers() {
        response = response.header(name, value);
    }
    let response = response.body(Empty::<Bytes>::new().map_err(|never: std::convert::Infallible| match never {}).boxed())?;

    tokio::spawn(async move {
        match tokio::try_join!(client_upgrade, target_upgrade) {
            Ok((client, target)) => splice(client, target).await,
            Err(error) => tracing::debug!(?error, "proxy: streaming upgrade failed on one side"),
        }
    });
    Ok(response)
}

fn build_uri(target: &Target) -> Result<Uri, Error> {
    let uri_str = if target.query.is_empty() { target.path.clone() } else { format!("{}?{}", target.path, target.query) };
    uri_str.parse().map_err(|e| Error::BuildRequest(http::Error::from(e)))
}

/// The real TCP+TLS dial and HTTP/1.1 handshake shared by [`fetch`] and
/// [`relay`] — the only part of either that's actually specific to
/// "connect to this one target," everything else about a request
/// (method/headers/body) is the caller's own concern.
type DialBody = http_body_util::combinators::BoxBody<Bytes, std::convert::Infallible>;

async fn dial(target: &Target, client_config: Arc<ClientConfig>) -> Result<hyper::client::conn::http1::SendRequest<DialBody>, Error> {
    let tcp = TcpStream::connect((target.host.as_str(), target.port)).await.map_err(|source| Error::Connect { host: target.host.clone(), port: target.port, source })?;
    let io: Box<dyn ProxyIo> = match target.scheme {
        "http" => Box::new(tcp),
        "https" => {
            let server_name = ServerName::try_from(target.host.clone()).map_err(|_| Error::InvalidServerName { host: target.host.clone() })?;
            let connector = TlsConnector::from(client_config);
            Box::new(connector.connect(server_name, tcp).await.map_err(|source| Error::Tls { host: target.host.clone(), port: target.port, source })?)
        }
        other => return Err(Error::InvalidScheme(other.to_string())),
    };
    let io = TokioIo::new(io);

    let (sender, conn) = hyper::client::conn::http1::handshake(io).await.map_err(Error::HttpHandshake)?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!(error = ?e, "proxy: connection ended");
        }
    });
    Ok(sender)
}

type UpgradeBody = http_body_util::combinators::BoxBody<Bytes, BoxError>;

async fn dial_upgrade(target: &Target, client_config: Arc<ClientConfig>) -> Result<hyper::client::conn::http1::SendRequest<UpgradeBody>, Error> {
    let server_name = ServerName::try_from(target.host.clone()).map_err(|_| Error::InvalidServerName { host: target.host.clone() })?;
    let tcp = TcpStream::connect((target.host.as_str(), target.port)).await.map_err(|source| Error::Connect { host: target.host.clone(), port: target.port, source })?;
    let connector = TlsConnector::from(client_config);
    let tls_stream = connector.connect(server_name, tcp).await.map_err(|source| Error::Tls { host: target.host.clone(), port: target.port, source })?;
    let io = TokioIo::new(tls_stream);
    let (sender, conn) = hyper::client::conn::http1::handshake(io).await.map_err(Error::HttpHandshake)?;
    tokio::spawn(async move {
        if let Err(error) = conn.with_upgrades().await {
            tracing::debug!(?error, "proxy: upgraded connection ended");
        }
    });
    Ok(sender)
}

async fn splice(client: Upgraded, target: Upgraded) {
    let mut client = TokioIo::new(client);
    let mut target = TokioIo::new(target);
    if let Err(error) = tokio::io::copy_bidirectional(&mut client, &mut target).await {
        tracing::debug!(?error, "proxy: upgraded connection ended");
    }
}

trait ProxyIo: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> ProxyIo for T {}
