//! `/exec`, `/attach`, `/portForward` — proxied to the CRI runtime's own
//! streaming server (containerd runs one internally; CRI's Exec/Attach/
//! PortForward RPCs return a one-shot URL to it, not the stream itself).
//! nodelet doesn't implement the SPDY/WebSocket streaming protocol at all
//! — it dials that URL itself with the client's original upgrade request,
//! mirrors whatever response the target gives back to the original client,
//! and once both sides have upgraded, splices the two raw connections
//! together byte-for-byte (`tokio::io::copy_bidirectional`). Real kubelet
//! does the same "proxy" pattern (as opposed to "redirect": the target URL
//! is normally `127.0.0.1:<port>`, unreachable to a remote kubectl client
//! directly).

use super::routes::query_values;
use super::{BoxedBody, ServerState};
use http_body_util::{BodyExt, Empty};
use hyper::body::{Bytes, Incoming};
use hyper::upgrade::Upgraded;
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use std::sync::Arc;
use tracing::warn;

fn empty_body() -> BoxedBody {
    Empty::<Bytes>::new().map_err(|never: std::convert::Infallible| match never {}).boxed()
}

pub async fn handle_exec(
    state: Arc<ServerState>,
    req: Request<Incoming>,
    namespace: &str,
    pod: &str,
    container: &str,
    query: &[(String, String)],
) -> Response<BoxedBody> {
    let cmd: Vec<String> = query_values(query, "command").into_iter().map(|s| s.to_string()).collect();
    if cmd.is_empty() {
        return super::text_response(StatusCode::BAD_REQUEST, "missing 'command' query parameter");
    }
    let stdin = super::routes::query_flag(query, "input");
    let stdout = super::routes::query_flag(query, "output");
    let stderr = super::routes::query_flag(query, "error");
    let tty = super::routes::query_flag(query, "tty");

    let target = state.runtime.exec_url(namespace, pod, container, &cmd, stdin, stdout, stderr, tty).await;
    proxy_to_target(req, target).await
}

pub async fn handle_attach(
    state: Arc<ServerState>,
    req: Request<Incoming>,
    namespace: &str,
    pod: &str,
    container: &str,
    query: &[(String, String)],
) -> Response<BoxedBody> {
    let stdin = super::routes::query_flag(query, "input");
    let stdout = super::routes::query_flag(query, "output");
    let stderr = super::routes::query_flag(query, "error");
    let tty = super::routes::query_flag(query, "tty");

    let target = state.runtime.attach_url(namespace, pod, container, stdin, stdout, stderr, tty).await;
    proxy_to_target(req, target).await
}

pub async fn handle_port_forward(
    state: Arc<ServerState>,
    req: Request<Incoming>,
    namespace: &str,
    pod: &str,
    query: &[(String, String)],
) -> Response<BoxedBody> {
    let ports = super::routes::query_values(query, "port")
        .into_iter()
        .filter_map(|value| value.parse::<i32>().ok())
        .collect::<Vec<_>>();
    let target = state.runtime.port_forward_url(namespace, pod, &ports).await;
    proxy_to_target(req, target).await
}

async fn proxy_to_target(req: Request<Incoming>, target: anyhow::Result<String>) -> Response<BoxedBody> {
    let target_url = match target {
        Ok(url) => url,
        Err(e) => {
            warn!(error = ?e, "server: failed to get a streaming URL from the runtime");
            return super::text_response(StatusCode::NOT_FOUND, format!("{e:#}"));
        }
    };
    match proxy_upgrade(req, &target_url).await {
        Ok(resp) => resp,
        Err(e) => {
            warn!(error = ?e, target = %target_url, "server: streaming proxy failed");
            super::text_response(StatusCode::BAD_GATEWAY, format!("streaming proxy failed: {e:#}"))
        }
    }
}

/// Dial `target_url` (always the CRI runtime's own local streaming
/// server — containerd's default is `127.0.0.1:<random-port>`), replay the
/// client's upgrade request there, mirror the target's response back to
/// the client, then splice the two upgraded connections together.
async fn proxy_upgrade(mut req: Request<Incoming>, target_url: &str) -> anyhow::Result<Response<BoxedBody>> {
    let target_uri: Uri = target_url.parse()?;
    let host = target_uri.host().ok_or_else(|| anyhow::anyhow!("streaming URL has no host: {target_url}"))?;
    let port = target_uri.port_u16().unwrap_or(80);

    let tcp = tokio::net::TcpStream::connect((host, port)).await?;
    let io = TokioIo::new(tcp);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
    tokio::spawn(async move {
        if let Err(e) = conn.with_upgrades().await {
            tracing::debug!(error = ?e, "server: streaming proxy upstream connection ended");
        }
    });

    let mut outgoing = Request::builder().method(req.method().clone()).uri(target_uri.clone());
    for (name, value) in req.headers() {
        // Host must match the target we're actually dialing, not the
        // original client-facing hostname.
        if name == hyper::header::HOST {
            continue;
        }
        outgoing = outgoing.header(name, value);
    }
    outgoing = outgoing.header(hyper::header::HOST, host);
    let outgoing = outgoing.body(empty_body())?;

    // Must grab the client-side upgrade future before the request is
    // consumed by anything else — hyper resolves it once *our* response
    // (built below) is flushed back to the client.
    let client_upgrade = hyper::upgrade::on(&mut req);

    let mut target_resp = sender.send_request(outgoing).await?;
    if target_resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        let status = target_resp.status();
        return Ok(super::text_response(status, "upstream streaming server did not switch protocols"));
    }

    let mut resp_builder = Response::builder().status(target_resp.status());
    for (name, value) in target_resp.headers() {
        resp_builder = resp_builder.header(name, value);
    }
    let target_upgrade = hyper::upgrade::on(&mut target_resp);

    tokio::spawn(async move {
        let (client_upgraded, target_upgraded) = match tokio::try_join!(client_upgrade, target_upgrade) {
            Ok(pair) => pair,
            Err(e) => {
                warn!(error = ?e, "server: upgrade handshake failed on one side of the proxy");
                return;
            }
        };
        splice(client_upgraded, target_upgraded).await;
    });

    Ok(resp_builder.body(empty_body())?)
}

async fn splice(client: Upgraded, target: Upgraded) {
    let mut client_io = TokioIo::new(client);
    let mut target_io = TokioIo::new(target);
    if let Err(e) = tokio::io::copy_bidirectional(&mut client_io, &mut target_io).await {
        tracing::debug!(error = ?e, "server: streaming proxy connection ended");
    }
}
