async fn handle(
    req: Request<Incoming>,
    mut storage: Option<StorageClient>,
    cache_registry: crate::cacher::CacheRegistry,
    pure_admission: Arc<crate::admission::chain::MutatingRegistry>,
    pod_node_selector_config: Option<Arc<crate::admission::pod_node_selector::PluginConfig>>,
    identity: Option<crate::authn::x509::Identity>,
    service_account_authenticator: Option<Arc<crate::authn::service_account::ReloadableAuthenticator>>,
    enforce_rbac: bool,
    authorization_webhook_allowed: bool,
    aggregation_proxy_identity: Option<Arc<crate::aggregator::client_tls::ClientIdentity>>,
    kubelet_tls: std::sync::Arc<rustls::ClientConfig>,
) -> Result<Response<BoxedBody>, Infallible> {
    include!("handle/prelude.rs");
    include!("handle/subresources.rs");
    include!("handle/patch.rs");
    include!("handle/status.rs");
    include!("handle/delete_collection.rs");
    include!("handle/reviews.rs");
    include!("handle/tokens.rs");
    include!("handle/scale.rs");
    include!("handle/aggregate.rs");
    include!("handle/crud.rs");
    include!("handle/watch.rs");
}
