//! A real, live round trip proving Group L Phase 4's reverse proxy chain
//! end to end: `aggregator::route::resolve` finding a real stored
//! `APIService` by `(group, version)`, `aggregator::availability::
//! preflight_check` passing against a real Service + ready
//! `EndpointSlice`, `aggregator::proxy_target::resolve` picking the real
//! dial target, `aggregator::client_tls::build_client_config` building a
//! real per-backend TLS trust from a real `spec.caBundle`, and
//! `proxy::http_client::relay` actually dialing and relaying a request/
//! response over a real TLS connection — the one part of this chain no
//! prior test exercised (`tests/apiservice_roundtrip.rs` only proved
//! `APIService` itself is a working resource; nothing before this test
//! ever dialed a real backend). The "backend" here is a small real
//! `rustls`/`hyper` HTTPS server this test spins up itself, standing in
//! for a real aggregated API server (metrics-server, ...) — same
//! "verified against real infrastructure, not assumed" standard every
//! other `tests/*_roundtrip.rs` file in this crate already holds itself
//! to; `crates/nodeproxy`'s own real `ClusterIP` DNAT (what a live
//! cluster would use to route to this same target) is a separate,
//! already-proven concern this test has no reason to stand up itself —
//! it dials `127.0.0.1` directly instead of a real `ClusterIP`, proving
//! the TLS/relay mechanics rather than cluster networking.

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::server::conn::http1 as server_http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use nodeapiserver::aggregator::{availability, client_tls, proxy_target, route};
use nodeapiserver::config::Config;
use nodeapiserver::proxy::http_client;
use nodeapiserver::server::rest;
use nodeapiserver::storage::client::StorageClient;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

/// Mirrors `tests/crd_roundtrip.rs`'s own `find_nodestore_binary` /
/// `spawn_nodestore` exactly — see that file's doc comments for why each
/// test file owns its own copy rather than sharing one across files.
fn find_nodestore_binary() -> Option<PathBuf> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent()?.parent()?.to_path_buf();
    let candidates = ["bin/nodestore", "target/release/nodestore", "target/debug/nodestore"];
    for candidate in candidates {
        let path = repo_root.join(candidate);
        if path.is_file() {
            return Some(path);
        }
    }
    if !repo_root.join("crates/nodestore").is_dir() {
        return None;
    }
    eprintln!("no nodestore binary found at any of {candidates:?} -- building one now (cargo build -p nodestore)");
    let status = std::process::Command::new("cargo").args(["build", "-p", "nodestore"]).current_dir(&repo_root).status().ok()?;
    if !status.success() {
        return None;
    }
    let built = repo_root.join("target/debug/nodestore");
    built.is_file().then_some(built)
}

async fn spawn_nodestore(nodestore_bin: &std::path::Path, port: u16) -> (tokio::process::Child, tempfile::TempDir, StorageClient) {
    let data_dir = tempfile::tempdir().expect("creating a scratch nodestore data dir");
    let listen = format!("127.0.0.1:{port}");

    let mut child = tokio::process::Command::new(nodestore_bin)
        .env("NODESTORE_LISTEN", &listen)
        .env("NODESTORE_DATA_DIR", data_dir.path())
        .env("RUST_LOG", "warn")
        .kill_on_drop(true)
        .spawn()
        .expect("spawning the real nodestore binary");

    let pki_dir = data_dir.path().join("pki/client");
    let cert = pki_dir.join("client.crt");
    let key = pki_dir.join("client.key");
    let ca = pki_dir.join("ca.crt");

    let mut cfg = Config::default();
    cfg.nodestore_endpoint = format!("https://{listen}");
    cfg.nodestore_cert_file = Some(cert.clone());
    cfg.nodestore_key_file = Some(key.clone());
    cfg.nodestore_ca_file = Some(ca.clone());

    let mut storage = None;
    for _ in 0..100 {
        if let Some(status) = child.try_wait().expect("checking whether nodestore is still running") {
            panic!("nodestore exited during startup with {status:?}");
        }
        if cert.is_file() && key.is_file() && ca.is_file() {
            if let Ok(client) = StorageClient::connect(&cfg).await {
                storage = Some(client);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let storage = storage.expect("nodestore never became reachable within 20s");
    (child, data_dir, storage)
}

/// A real HTTPS server standing in for an aggregated backend
/// (metrics-server, ...) — real `rcgen` self-signed cert, real
/// `rustls`/`hyper` handshake, echoes the request path back as the
/// response body so the test can prove the *exact* path/query this
/// build's own dispatch resolved actually reached the backend unchanged.
async fn spawn_https_echo_server() -> (std::net::SocketAddr, Vec<u8>) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let key_pair = rcgen::KeyPair::generate().expect("generating a key pair");
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).expect("generating a self-signed cert");
    let cert_der = cert.cert.der().clone();
    let cert_pem = cert.cert.pem();
    let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(key_pair.serialize_der()));

    let server_config = rustls::ServerConfig::builder().with_no_client_auth().with_single_cert(vec![cert_der], key_der).expect("building the server TLS config");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binding the echo server");
    let addr = listener.local_addr().expect("reading the bound address");

    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else { return };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls_stream) = acceptor.accept(tcp).await else { return };
                let io = TokioIo::new(tls_stream);
                let service = service_fn(|req: Request<hyper::body::Incoming>| async move {
                    let echoed = format!("{} {}", req.method(), req.uri());
                    Ok::<_, std::convert::Infallible>(Response::new(Full::new(Bytes::from(echoed)).boxed()))
                });
                let _ = server_http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });

    (addr, cert_pem.into_bytes())
}

#[tokio::test]
async fn an_aggregated_apiservice_resolves_and_relays_through_a_real_tls_backend() {
    let Some(nodestore_bin) = find_nodestore_binary() else {
        eprintln!("SKIPPED: no nodestore binary available and building one on demand failed");
        return;
    };
    let (mut child, _data_dir, mut storage) = spawn_nodestore(&nodestore_bin, 23811).await;
    let (backend_addr, backend_ca_pem) = spawn_https_echo_server().await;

    let api_service = json!({
        "apiVersion": "apiregistration.k8s.io/v1",
        "kind": "APIService",
        "metadata": {"name": "v1beta1.metrics.k8s.io"},
        "spec": {
            "group": "metrics.k8s.io",
            "version": "v1beta1",
            "service": {"namespace": "kube-system", "name": "metrics-server", "port": 443},
            "groupPriorityMinimum": 100,
            "versionPriority": 100,
            "caBundle": base64_encode(&backend_ca_pem),
        },
    });
    rest::create(&mut storage, "apiregistration.k8s.io", "v1", "apiservices", None, &api_service).await.expect("creating the APIService");

    let service = json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {"name": "metrics-server", "namespace": "kube-system"},
        // A real dial target: this build's own dispatch would resolve
        // `spec.clusterIP` to dial, so this test points it at the real
        // echo server's own loopback address rather than a synthetic
        // ClusterIP -- see this file's own doc comment for why that's
        // still a faithful test of the mechanics that matter here.
        "spec": {"type": "ClusterIP", "clusterIP": backend_addr.ip().to_string(), "ports": [{"name": "https", "port": 443, "targetPort": backend_addr.port() as i64}]},
    });
    rest::create(&mut storage, "", "v1", "services", Some("kube-system"), &service).await.expect("creating the backing Service");

    let endpoint_slice = json!({
        "apiVersion": "discovery.k8s.io/v1",
        "kind": "EndpointSlice",
        "metadata": {"name": "metrics-server-abc12", "namespace": "kube-system", "labels": {"kubernetes.io/service-name": "metrics-server"}},
        "addressType": "IPv4",
        "endpoints": [{"addresses": [backend_addr.ip().to_string()], "conditions": {"ready": true}}],
        "ports": [{"name": "https", "port": backend_addr.port() as i64}],
    });
    rest::create(&mut storage, "discovery.k8s.io", "v1", "endpointslices", Some("kube-system"), &endpoint_slice).await.expect("creating the EndpointSlice");

    // Step 1: `route::resolve` finds the real stored APIService by its
    // own `(group, version)` -- the exact lookup `server::listener`'s own
    // dispatch branch performs before ever considering this a local
    // request.
    let resolved = route::resolve(&mut storage, "metrics.k8s.io", "v1beta1").await.expect("resolving the APIService").expect("a matching, non-local APIService must be found");
    assert_eq!(resolved["spec"]["service"]["name"], "metrics-server");
    assert!(route::resolve(&mut storage, "metrics.k8s.io", "v1").await.expect("resolving a non-matching version").is_none(), "a different version must not match");

    // Step 2: the real pre-flight chain against the real Service/
    // EndpointSlice this build just created -- must pass, since the
    // backend really is up and really does have a ready endpoint on the
    // matching port name.
    let service_doc = match rest::get(&mut storage, None, "", "v1", "services", Some("kube-system"), "metrics-server").await.expect("fetching the service") {
        rest::GetOutcome::Found(v) => v,
        other => panic!("expected Found, got {other:?}"),
    };
    let slices = match rest::list(&mut storage, None, "discovery.k8s.io", "v1", "endpointslices", Some("kube-system"), "kubernetes.io/service-name=metrics-server", "", 0, "").await.expect("listing endpointslices") {
        rest::ListOutcome::Found(list) => list["items"].as_array().cloned().unwrap_or_default(),
        other => panic!("expected Found, got {other:?}"),
    };
    assert_eq!(slices.len(), 1);
    availability::preflight_check("kube-system", "metrics-server", 443, Some(&service_doc), &slices).expect("pre-flight must pass against a real, ready backend");

    // Step 3: resolve the real dial target and build this backend's own
    // real TLS trust from its own real `spec.caBundle`.
    let target = proxy_target::resolve(&resolved, &service_doc, "/apis/metrics.k8s.io/v1beta1/nodes", "").expect("resolving the proxy target");
    assert_eq!(target.host, backend_addr.ip().to_string());
    let ca_bundle = base64_decode(resolved["spec"]["caBundle"].as_str().unwrap());
    let client_config = client_tls::build_client_config(Some(&ca_bundle), false).expect("building the real per-APIService TLS client config");

    // Step 4: the actual live dial -- a real TLS handshake verified
    // against the real `caBundle` above (not `insecureSkipTLSVerify`),
    // then a real HTTP/1.1 request relayed to the real echo server and
    // its real response relayed back.
    let mut real_target = target;
    real_target.port = backend_addr.port();
    let resp = http_client::relay(&real_target, Arc::new(client_config), "GET", &[], Vec::new()).await.expect("dialing the real backend over real TLS must succeed");
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.expect("reading the relayed response body").to_bytes();
    assert_eq!(body, Bytes::from_static(b"GET /apis/metrics.k8s.io/v1beta1/nodes"), "the backend must have received the exact path this build resolved");

    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(s: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).expect("valid base64")
}
