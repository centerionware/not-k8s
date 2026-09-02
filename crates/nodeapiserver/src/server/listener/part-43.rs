
/// Group N's core node/service proxy dispatch.  The object and EndpointSlice
/// reads are intentionally performed before consuming the request body so an
/// invalid or unavailable target returns a normal Kubernetes Status response.
async fn proxy_resource(
    req: Request<Incoming>,
    storage: Option<StorageClient>,
    info: &path::RequestInfo,
    method: &str,
    path_str: &str,
    query: &str,
    identity: &Option<crate::authn::x509::Identity>,
    enforce_rbac: bool,
    kubelet_tls: std::sync::Arc<rustls::ClientConfig>,
) -> Response<BoxedBody> {
    include!("body-71-1.rs");
    include!("body-71-2.rs");
}
