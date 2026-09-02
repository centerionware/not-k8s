
async fn handle(
    req: Request<Incoming>,
    mut storage: Option<StorageClient>,
    cache_registry: crate::cacher::CacheRegistry,
    identity: Option<crate::authn::x509::Identity>,
    service_account_authenticator: Option<Arc<crate::authn::service_account::ReloadableAuthenticator>>,
    enforce_rbac: bool,
    authorization_webhook_allowed: bool,
    kubelet_tls: std::sync::Arc<rustls::ClientConfig>,
) -> Result<Response<BoxedBody>, Infallible> {
    include!("body-66-1.rs");
    include!("body-66-2.rs");
    include!("body-66-3.rs");
    include!("body-66-4.rs");
    include!("body-66-5.rs");
}
