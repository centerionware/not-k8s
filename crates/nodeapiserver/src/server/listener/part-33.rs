
/// Group M: wraps every request with a real `audit::event::build_event`
/// call, logged rather than delegated back into `handle` itself — this
/// wrapper needs nothing `handle` doesn't already compute internally
/// (method/path/query are read off `req` before it's ever consumed, and
/// `path::parse` is a pure function safe to call a second time here),
/// so it's the far less invasive place to add auditing than threading an
/// audit-context return value out through every one of `handle`'s own
/// early-return branches would have been. The sink is this crate's own
/// `tracing` output (`target: "nodeapiserver::audit"`, one JSON line per
/// request) and, when configured, an append-only file selected by
/// `NODEAPISERVER_AUDIT_LOG_PATH`; rotation and webhook delivery remain
/// separate backends. See
/// `audit::event`'s own doc comment for exactly which real `Event`
/// fields are populated and which stage/level this always uses.
async fn handle_with_audit(
    req: Request<Incoming>,
    storage: Option<StorageClient>,
    cache_registry: crate::cacher::CacheRegistry,
    identity: Option<crate::authn::x509::Identity>,
    bootstrap_token_authenticator: Option<Arc<crate::authn::bootstrap_token::ReloadableAuthenticator>>,
    service_account_authenticator: Option<Arc<crate::authn::service_account::ReloadableAuthenticator>>,
    oidc_authenticator: Option<Arc<crate::authn::oidc::Authenticator>>,
    authorization_webhook: Option<Arc<crate::authz::webhook::WebhookAuthorizer>>,
    concurrency_limiter: Arc<crate::flowcontrol::limiter::ConcurrencyLimiter>,
    audit_sink: Option<Arc<crate::audit::sink::AuditSink>>,
    audit_policy: Option<Arc<crate::audit::policy::AuditPolicy>>,
    anonymous_auth: bool,
    enforce_rbac: bool,
    peer: SocketAddr,
    kubelet_tls: std::sync::Arc<rustls::ClientConfig>,
) -> Result<Response<BoxedBody>, Infallible> {
    include!("body-61-1.rs");
    include!("body-61-2.rs");
}
