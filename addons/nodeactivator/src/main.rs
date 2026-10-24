use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use futures::StreamExt;
use http::{HeaderMap, Method, Request, Response, StatusCode, Uri};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnectionBuilder;
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SubjectAccessReview, SubjectAccessReviewSpec,
};
use k8s_openapi::api::core::v1::{Service, ServicePort};
use k8s_openapi::api::discovery::v1::{Endpoint, EndpointConditions, EndpointPort, EndpointSlice};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::runtime::watcher::{self, Event};
use kube::{Client, ResourceExt};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufReader, Cursor};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{RwLock, Semaphore};
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

const BACKEND_ANNOTATION: &str = "activator.notk8s.io/backend-url";
const MANAGED_LABEL: &str = "activator.notk8s.io/enabled";
const SLICE_MANAGED_BY: &str = "nodeactivator.notk8s.io";
const METRIC_NAME: &str = "activator_requests_active";
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_ACTIVE_REQUESTS: usize = 16;
const MAX_CONNECTIONS: usize = 64;

fn activator_port(service: &Service) -> Option<&ServicePort> {
    let ports = service.spec.as_ref()?.ports.as_deref()?;
    let [port] = ports else {
        return None;
    };
    (port.protocol.as_deref().unwrap_or("TCP") == "TCP").then_some(port)
}

#[derive(Clone)]
struct Route {
    service_key: String,
    backend_url: String,
}

struct State {
    routes: RwLock<HashMap<String, Route>>,
    services: RwLock<HashMap<String, Service>>,
    counters: Mutex<HashMap<String, Arc<AtomicUsize>>>,
    ready: AtomicBool,
    client: reqwest::Client,
    kube_client: Client,
    pod_ip: IpAddr,
    queue_slots: Arc<Semaphore>,
    activation_timeout: Duration,
}

impl State {
    fn counter(&self, service_key: &str) -> Arc<AtomicUsize> {
        self.counters
            .lock()
            .expect("request metric lock poisoned")
            .entry(service_key.to_string())
            .or_default()
            .clone()
    }

    async fn set_service(&self, service: &Service) {
        let name = service.name_any();
        let namespace = service.namespace().unwrap_or_default();
        let service_key = format!("{namespace}/{name}");
        self.services
            .write()
            .await
            .insert(service_key.clone(), service.clone());
        let route = service
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get(BACKEND_ANNOTATION))
            .filter(|backend| !backend.is_empty())
            .and_then(|backend_url| {
                reqwest::Url::parse(backend_url)
                    .ok()
                    .filter(|url| matches!(url.scheme(), "http" | "https"))
                    .map(|url| Route {
                        service_key: service_key.clone(),
                        backend_url: url.as_str().trim_end_matches('/').to_string(),
                    })
            });
        let selectorless = service
            .spec
            .as_ref()
            .is_some_and(|spec| spec.selector.is_none());
        let hosts = service_host(&name, &namespace);
        let mut routes = self.routes.write().await;
        routes.retain(|_, route| route.service_key != service_key);
        if let Some(route) = route.filter(|_| selectorless && activator_port(service).is_some()) {
            self.counter(&service_key);
            for host in hosts {
                routes.insert(host, route.clone());
            }
        } else {
            self.counters
                .lock()
                .expect("request metric lock poisoned")
                .remove(&service_key);
        }
        drop(routes);
        if let Err(error) = self.reconcile_endpoint_slice(service).await {
            warn!(namespace, service = %name, error = ?error, "reconciling Activator EndpointSlice failed");
        }
    }

    async fn remove_service(&self, service: &Service) {
        let name = service.name_any();
        let namespace = service.namespace().unwrap_or_default();
        let service_key = format!("{namespace}/{name}");
        self.services.write().await.remove(&service_key);
        self.routes
            .write()
            .await
            .retain(|_, route| route.service_key != service_key);
        self.counters
            .lock()
            .expect("request metric lock poisoned")
            .remove(&service_key);
        if let Err(error) = self.delete_endpoint_slice(&namespace, &name).await {
            warn!(namespace, service = %name, error = ?error, "deleting Activator EndpointSlice failed");
        }
    }

    async fn mark_ready(&self) {
        self.ready.store(true, Ordering::Release);
        let services = self
            .services
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for service in services {
            if let Err(error) = self.reconcile_endpoint_slice(&service).await {
                warn!(namespace = ?service.namespace(), service = %service.name_any(), error = ?error, "publishing ready Activator EndpointSlice failed");
            }
        }
    }

    async fn reconcile_endpoint_slice(&self, service: &Service) -> Result<()> {
        let name = service.name_any();
        let namespace = service.namespace().unwrap_or_default();
        let enabled = service
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(MANAGED_LABEL))
            .is_some_and(|value| value == "true");
        let selectorless = service
            .spec
            .as_ref()
            .is_some_and(|spec| spec.selector.is_none());
        let backend_configured = service
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get(BACKEND_ANNOTATION))
            .is_some_and(|backend| !backend.is_empty());
        if !enabled || !selectorless || !backend_configured {
            return self.delete_endpoint_slice(&namespace, &name).await;
        }
        let Some(service_port) = activator_port(service) else {
            warn!(namespace, service = %name, "Activator front Service must have exactly one TCP port");
            return self.delete_endpoint_slice(&namespace, &name).await;
        };
        let ip = self.pod_ip;
        let service_uid = service.uid().context("front Service has no UID")?;
        let slice_name = format!("nodeactivator-{name}");
        let mut labels = std::collections::BTreeMap::new();
        labels.insert("kubernetes.io/service-name".to_string(), name.clone());
        labels.insert(
            "endpointslice.kubernetes.io/managed-by".to_string(),
            SLICE_MANAGED_BY.to_string(),
        );
        let slice = EndpointSlice {
            address_type: if ip.is_ipv4() { "IPv4" } else { "IPv6" }.to_string(),
            endpoints: Some(vec![Endpoint {
                addresses: vec![ip.to_string()],
                conditions: Some(EndpointConditions {
                    ready: Some(self.ready.load(Ordering::Acquire)),
                    serving: Some(self.ready.load(Ordering::Acquire)),
                    terminating: Some(false),
                }),
                ..Default::default()
            }]),
            metadata: ObjectMeta {
                name: Some(slice_name.clone()),
                namespace: Some(namespace.clone()),
                labels: Some(labels),
                owner_references: Some(vec![OwnerReference {
                    api_version: "v1".to_string(),
                    kind: "Service".to_string(),
                    name: name.clone(),
                    uid: service_uid,
                    controller: Some(true),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            ports: Some(vec![EndpointPort {
                name: service_port.name.clone(),
                port: Some(8080),
                protocol: Some("TCP".to_string()),
                ..Default::default()
            }]),
        };
        let slices: Api<EndpointSlice> = Api::namespaced(self.kube_client.clone(), &namespace);
        match slices.get(&slice_name).await {
            Ok(existing) => {
                let managed_by = existing
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get("endpointslice.kubernetes.io/managed-by"));
                anyhow::ensure!(
                    managed_by.is_some_and(|value| value == SLICE_MANAGED_BY),
                    "refusing to take over EndpointSlice {namespace}/{slice_name} not managed by Node Activator"
                );
            }
            Err(kube::Error::Api(error)) if error.code == 404 => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("getting EndpointSlice {namespace}/{slice_name}"));
            }
        }
        slices
            .patch(
                &slice_name,
                &PatchParams::apply(SLICE_MANAGED_BY).force(),
                &Patch::Apply(&slice),
            )
            .await
            .with_context(|| format!("applying EndpointSlice {namespace}/{slice_name}"))?;
        Ok(())
    }

    async fn delete_endpoint_slice(&self, namespace: &str, service: &str) -> Result<()> {
        let slices: Api<EndpointSlice> = Api::namespaced(self.kube_client.clone(), namespace);
        let name = format!("nodeactivator-{service}");
        match slices.get(&name).await {
            Ok(slice) => {
                let managed_by = slice
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get("endpointslice.kubernetes.io/managed-by"));
                if !managed_by.is_some_and(|value| value == SLICE_MANAGED_BY) {
                    return Ok(());
                }
            }
            Err(kube::Error::Api(error)) if error.code == 404 => return Ok(()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("getting EndpointSlice {namespace}/{name}"));
            }
        }
        match slices.delete(&name, &Default::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(error)) if error.code == 404 => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("deleting EndpointSlice {namespace}/{name}"))
            }
        }
    }
}

struct ActiveRequest(Arc<AtomicUsize>);

impl ActiveRequest {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn watch_front_services(client: Client, state: Arc<State>) -> Result<()> {
    let services: Api<Service> = Api::all(client);
    let config = watcher::Config::default().labels(&format!("{MANAGED_LABEL}=true"));
    let mut stream = Box::pin(watcher::watcher(services, config));
    loop {
        match stream.next().await {
            Some(Ok(Event::Apply(service))) | Some(Ok(Event::InitApply(service))) => {
                state.set_service(&service).await;
            }
            Some(Ok(Event::Delete(service))) => state.remove_service(&service).await,
            Some(Ok(Event::Init)) => {}
            Some(Ok(Event::InitDone)) => state.mark_ready().await,
            Some(Err(error)) => warn!(error = ?error, "Service watch event failed"),
            None => return Err(anyhow!("front Service watch ended")),
        }
    }
}

fn status_response(status: StatusCode, message: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from_static(message.as_bytes())))
        .expect("static error response is valid")
}

fn host_without_port(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(http::header::HOST)?.to_str().ok()?;
    Some(
        host.rsplit_once(':')
            .map_or(host, |(host, _)| host)
            .to_ascii_lowercase(),
    )
}

fn service_host(name: &str, namespace: &str) -> [String; 2] {
    [
        format!("{name}.{namespace}.svc"),
        format!("{name}.{namespace}.svc.cluster.local"),
    ]
}

async fn proxy_request(request: Request<Incoming>, state: Arc<State>) -> Response<Full<Bytes>> {
    if request.uri().path() == "/healthz" {
        return Response::new(Full::new(Bytes::from_static(b"ok\n")));
    }
    if request.uri().path() == "/readyz" {
        return if state.ready.load(Ordering::Acquire) {
            Response::new(Full::new(Bytes::from_static(b"ready\n")))
        } else {
            status_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Service watch is initializing\n",
            )
        };
    }
    let Some(host) = host_without_port(request.headers()) else {
        return status_response(StatusCode::BAD_REQUEST, "request Host is required\n");
    };
    let route = state.routes.read().await.get(&host).cloned();
    let Some(route) = route else {
        return status_response(
            StatusCode::NOT_FOUND,
            "no activator route for this Service\n",
        );
    };
    let Ok(slot) = state.queue_slots.clone().try_acquire_owned() else {
        return status_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "activator request queue is full\n",
        );
    };
    let counter = state.counter(&route.service_key);
    let _active = ActiveRequest::new(counter);
    let _slot = slot;
    let (parts, body) = request.into_parts();
    let body = match Limited::new(body, MAX_REQUEST_BYTES).collect().await {
        Ok(body) => body.to_bytes(),
        Err(_) => {
            return status_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds 1 MiB\n",
            );
        }
    };
    let target = format!("{}{}", route.backend_url, parts.uri);
    let mut headers = parts.headers;
    if let Some(host) = headers.get(http::header::HOST).cloned() {
        headers.insert("x-forwarded-host", host);
    }
    headers.remove(http::header::HOST);
    headers.remove(http::header::CONNECTION);
    headers.remove(http::header::TRANSFER_ENCODING);
    let deadline = Instant::now() + state.activation_timeout;

    loop {
        let result = state
            .client
            .request(parts.method.clone(), &target)
            .headers(headers.clone())
            .body(body.clone())
            .send()
            .await;
        match result {
            Ok(upstream) => {
                let status = upstream.status();
                let upstream_headers = upstream.headers().clone();
                let mut response_body = Vec::new();
                let mut response_stream = upstream.bytes_stream();
                while let Some(chunk) = response_stream.next().await {
                    let chunk = match chunk {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            warn!(service = %route.service_key, error = ?error, "backend response read failed");
                            return status_response(
                                StatusCode::BAD_GATEWAY,
                                "backend response could not be read\n",
                            );
                        }
                    };
                    if response_body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                        return status_response(
                            StatusCode::BAD_GATEWAY,
                            "backend response exceeds 8 MiB\n",
                        );
                    }
                    response_body.extend_from_slice(&chunk);
                }
                let mut response = Response::builder().status(status);
                if let Some(response_headers) = response.headers_mut() {
                    for (name, value) in &upstream_headers {
                        if !matches!(
                            name.as_str(),
                            "connection" | "transfer-encoding" | "keep-alive" | "upgrade"
                        ) {
                            response_headers.insert(name.clone(), value.clone());
                        }
                    }
                }
                return response
                    .body(Full::new(Bytes::from(response_body)))
                    .unwrap_or_else(|_| {
                        status_response(StatusCode::BAD_GATEWAY, "invalid backend response\n")
                    });
            }
            Err(error) if error.is_connect() && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(error) => {
                warn!(service = %route.service_key, error = ?error, "backend did not become reachable before activation timeout");
                return status_response(
                    StatusCode::GATEWAY_TIMEOUT,
                    "backend did not become ready in time\n",
                );
            }
        }
    }
}

fn decode_query_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(hex) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                decoded.push(hex);
                index += 3;
                continue;
            }
        }
        decoded.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn requested_service(uri: &Uri) -> Option<String> {
    let query = uri.query()?;
    query.split('&').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (decode_query_component(key) == "labelSelector")
            .then(|| decode_query_component(value))
            .and_then(|selector| {
                selector.split(',').find_map(|requirement| {
                    let (key, value) = requirement
                        .split_once("==")
                        .or_else(|| requirement.split_once('='))?;
                    (key.trim() == "service").then(|| value.trim().to_string())
                })
            })
    })
}

fn metric_discovery() -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "APIResourceList",
        "groupVersion": "external.metrics.k8s.io/v1beta1",
        "resources": [{
            "name": METRIC_NAME,
            "singularName": "",
            "namespaced": true,
            "kind": "ExternalMetricValue",
            "verbs": ["get"]
        }]
    })
}

fn metric_value_list(namespace: &str, service: Option<String>, state: &State) -> Value {
    let counters = state.counters.lock().expect("request metric lock poisoned");
    let items = counters
        .iter()
        .filter(|(key, _)| {
            key.starts_with(&format!("{namespace}/"))
                && service.as_ref().is_none_or(|service| {
                    key.rsplit('/')
                        .next()
                        .is_some_and(|name| name == service.as_str())
                })
        })
        .map(|(key, counter)| {
            json!({
                "metricName": METRIC_NAME,
                "metricLabels": {"service": key.rsplit('/').next().unwrap_or_default()},
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "value": counter.load(Ordering::Relaxed).to_string()
            })
        })
        .collect::<Vec<_>>();
    json!({
        "apiVersion": "external.metrics.k8s.io/v1beta1",
        "kind": "ExternalMetricValueList",
        "metadata": {},
        "items": items
    })
}

fn json_response(status: StatusCode, value: Value) -> Response<Full<Bytes>> {
    let body = serde_json::to_vec(&value).unwrap_or_default();
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("JSON response is valid")
}

async fn metrics_request(request: Request<Incoming>, state: Arc<State>) -> Response<Full<Bytes>> {
    let path = request.uri().path();
    let prefix = "/apis/external.metrics.k8s.io/v1beta1";
    if request.method() != Method::GET {
        return status_response(StatusCode::METHOD_NOT_ALLOWED, "GET is required\n");
    }
    if path == prefix || path == format!("{prefix}/") {
        return json_response(StatusCode::OK, metric_discovery());
    }
    let Some(suffix) = path.strip_prefix(&format!("{prefix}/namespaces/")) else {
        return status_response(StatusCode::NOT_FOUND, "unknown external metrics API path\n");
    };
    let Some((namespace, metric)) = suffix.split_once('/') else {
        return status_response(
            StatusCode::NOT_FOUND,
            "metric namespace and name are required\n",
        );
    };
    if metric != METRIC_NAME {
        return status_response(StatusCode::NOT_FOUND, "unknown external metric\n");
    }
    match authorize_metric_request(request.headers(), namespace, metric, &state.kube_client).await {
        Ok(true) => {}
        Ok(false) => {
            return status_response(StatusCode::FORBIDDEN, "external metric access denied\n");
        }
        Err(error) => {
            warn!(error = ?error, "external metric authorization check failed");
            return status_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "authorization check failed\n",
            );
        }
    }
    json_response(
        StatusCode::OK,
        metric_value_list(namespace, requested_service(request.uri()), &state),
    )
}

async fn authorize_metric_request(
    headers: &HeaderMap,
    namespace: &str,
    metric: &str,
    client: &Client,
) -> Result<bool> {
    let user = headers
        .get("x-remote-user")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("aggregator did not provide X-Remote-User"))?;
    let groups = headers
        .get_all("x-remote-group")
        .iter()
        .filter_map(|value| value.to_str().ok().map(str::to_string))
        .collect::<Vec<_>>();
    let review = SubjectAccessReview {
        spec: SubjectAccessReviewSpec {
            user: Some(user.to_string()),
            groups: Some(groups),
            resource_attributes: Some(ResourceAttributes {
                group: Some("external.metrics.k8s.io".to_string()),
                version: Some("v1beta1".to_string()),
                resource: Some(metric.to_string()),
                namespace: Some(namespace.to_string()),
                verb: Some("get".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let api: Api<SubjectAccessReview> = Api::all(client.clone());
    let response = api
        .create(&PostParams::default(), &review)
        .await
        .context("creating delegated SubjectAccessReview")?;
    Ok(response
        .status
        .is_some_and(|status| status.allowed && status.denied != Some(true)))
}

async fn serve_connection(
    stream: TcpStream,
    peer: SocketAddr,
    tls: Option<Arc<RwLock<TlsAcceptor>>>,
    state: Arc<State>,
    metrics: bool,
    _slot: tokio::sync::OwnedSemaphorePermit,
) {
    if let Some(acceptor) = tls {
        let acceptor = acceptor.read().await.clone();
        match acceptor.accept(stream).await {
            Ok(stream) => serve_http_connection(stream, peer, state, metrics, _slot).await,
            Err(error) => {
                warn!(peer = %peer, error = ?error, "external metrics mTLS handshake failed");
            }
        }
    } else {
        serve_http_connection(stream, peer, state, metrics, _slot).await;
    }
}

async fn serve_http_connection<I>(
    stream: I,
    peer: SocketAddr,
    state: Arc<State>,
    metrics: bool,
    _slot: tokio::sync::OwnedSemaphorePermit,
) where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let io = TokioIo::new(stream);
    let service = service_fn(move |request| {
        let state = state.clone();
        async move {
            let response = if metrics {
                metrics_request(request, state).await
            } else {
                proxy_request(request, state).await
            };
            Ok::<_, std::convert::Infallible>(response)
        }
    });
    if let Err(error) = ConnectionBuilder::new(TokioExecutor::new())
        .serve_connection_with_upgrades(io, service)
        .await
    {
        warn!(peer = %peer, error = ?error, "HTTP connection failed");
    }
}

async fn serve(
    address: SocketAddr,
    tls: Option<Arc<RwLock<TlsAcceptor>>>,
    state: Arc<State>,
    metrics: bool,
) -> Result<()> {
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("binding listener {address}"))?;
    let connection_slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    info!(%address, tls = tls.is_some(), metrics, "nodeactivator listener started");
    loop {
        let (stream, peer) = listener
            .accept()
            .await
            .context("accepting client connection")?;
        let Ok(slot) = connection_slots.clone().try_acquire_owned() else {
            warn!(%peer, "dropping connection because the listener is full");
            drop(stream);
            continue;
        };
        tokio::spawn(serve_connection(
            stream,
            peer,
            tls.clone(),
            state.clone(),
            metrics,
            slot,
        ));
    }
}

fn required_env(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("{name} must be set"))
}

fn server_tls_acceptor(bundle: &[u8], proxy_ca_file: &str) -> Result<TlsAcceptor> {
    let mut cert_reader = BufReader::new(Cursor::new(bundle));
    let certificates = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<Vec<CertificateDer<'static>>, _>>()?;
    if certificates.is_empty() {
        return Err(anyhow!("TLS certificate file contains no certificates"));
    }
    let mut key_reader = BufReader::new(Cursor::new(bundle));
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)?
        .ok_or_else(|| anyhow!("TLS private key file contains no key"))?;
    let mut ca_reader =
        BufReader::new(fs::File::open(proxy_ca_file).context("opening proxy client CA")?);
    let client_ca = rustls_pemfile::certs(&mut ca_reader)
        .collect::<std::result::Result<Vec<CertificateDer<'static>>, _>>()?;
    let mut roots = RootCertStore::empty();
    for certificate in client_ca {
        roots.add(certificate)?;
    }
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots)).build()?;
    let config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certificates, key)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

async fn reload_server_tls(
    bundle_path: String,
    proxy_ca_file: String,
    acceptor: Arc<RwLock<TlsAcceptor>>,
    mut previous_bundle: Vec<u8>,
) {
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let bundle = match fs::read(&bundle_path) {
            Ok(bundle) if bundle != previous_bundle => bundle,
            Ok(_) => continue,
            Err(error) => {
                warn!(path = %bundle_path, error = ?error, "reading rotated PodCertificate bundle failed");
                continue;
            }
        };
        match server_tls_acceptor(&bundle, &proxy_ca_file) {
            Ok(updated) => {
                *acceptor.write().await = updated;
                previous_bundle = bundle;
                info!(path = %bundle_path, "reloaded rotated PodCertificate bundle");
            }
            Err(error) => {
                warn!(error = ?error, "loading rotated PodCertificate bundle failed; retaining previous certificate")
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let activation_timeout = env::var("ACTIVATOR_TIMEOUT_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(180);
    let client = Client::try_default()
        .await
        .context("loading in-cluster Kubernetes client")?;
    let pod_ip = required_env("ACTIVATOR_POD_IP")?
        .parse::<IpAddr>()
        .context("parsing ACTIVATOR_POD_IP")?;
    let state = Arc::new(State {
        routes: RwLock::new(HashMap::new()),
        services: RwLock::new(HashMap::new()),
        counters: Mutex::new(HashMap::new()),
        ready: AtomicBool::new(false),
        client: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(1))
            .build()
            .context("building backend HTTP client")?,
        kube_client: client.clone(),
        pod_ip,
        queue_slots: Arc::new(Semaphore::new(MAX_ACTIVE_REQUESTS)),
        activation_timeout: Duration::from_secs(activation_timeout),
    });
    let http_address =
        env::var("ACTIVATOR_HTTP_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let metrics_address =
        env::var("ACTIVATOR_METRICS_ADDR").unwrap_or_else(|_| "0.0.0.0:8443".to_string());
    let http_address = http_address
        .parse()
        .context("parsing ACTIVATOR_HTTP_ADDR")?;
    let metrics_address = metrics_address
        .parse()
        .context("parsing ACTIVATOR_METRICS_ADDR")?;
    let bundle_path = required_env("ACTIVATOR_TLS_BUNDLE")?;
    let proxy_ca_file = required_env("ACTIVATOR_PROXY_CLIENT_CA")?;
    let bundle = fs::read(&bundle_path).context("reading projected PodCertificate bundle")?;
    let tls = Arc::new(RwLock::new(server_tls_acceptor(&bundle, &proxy_ca_file)?));
    tokio::spawn(reload_server_tls(
        bundle_path,
        proxy_ca_file,
        tls.clone(),
        bundle,
    ));
    let watcher_state = state.clone();
    let watcher = tokio::spawn(async move { watch_front_services(client, watcher_state).await });
    tokio::select! {
        result = watcher => result.context("joining Service watcher")??,
        result = serve(http_address, None, state.clone(), false) => result?,
        result = serve(metrics_address, Some(tls), state, true) => result?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_hosts_match_cluster_dns_names() {
        assert_eq!(
            service_host("checkout", "shop"),
            [
                "checkout.shop.svc".to_string(),
                "checkout.shop.svc.cluster.local".to_string()
            ]
        );
    }

    #[test]
    fn host_parser_removes_port_and_normalizes_case() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::HOST, "APP.DEFAULT.SVC:8080".parse().unwrap());
        assert_eq!(
            host_without_port(&headers).as_deref(),
            Some("app.default.svc")
        );
    }

    #[test]
    fn external_metric_selector_decodes_service_label() {
        let uri: Uri = "/apis/external.metrics.k8s.io/v1beta1/namespaces/default/activator_requests_active?labelSelector=service%3Dcheckout"
            .parse()
            .unwrap();
        assert_eq!(requested_service(&uri).as_deref(), Some("checkout"));

        let uri: Uri = "/apis/external.metrics.k8s.io/v1beta1/namespaces/default/activator_requests_active?labelSelector=service%3D%3Dcheckout"
            .parse()
            .unwrap();
        assert_eq!(requested_service(&uri).as_deref(), Some("checkout"));
    }

    #[test]
    fn active_request_counter_covers_queue_lifetime() {
        let counter = Arc::new(AtomicUsize::new(0));
        {
            let _request = ActiveRequest::new(counter.clone());
            assert_eq!(counter.load(Ordering::Relaxed), 1);
        }
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }
}
