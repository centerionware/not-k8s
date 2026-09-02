
/// Runs the listener forever (until the process exits). Best-effort on
/// bind/TLS failure — logs and returns rather than panicking, matching
/// every other background loop's degrade-and-continue posture in this
/// workspace (see `crates/nodelet/src/server/mod.rs::run`'s own doc
/// comment for the precedent).
pub async fn run(cfg: Config) {
    let cert_result = match (&cfg.tls_cert_file, &cfg.tls_key_file) {
        (Some(cert), Some(key)) => super::tls::load_from_pem(cert, key),
        _ => {
            let cert_dir = std::path::PathBuf::from("/var/lib/nodeapiserver/pki");
            let sans = vec![
                "localhost".to_string(),
                "127.0.0.1".to_string(),
                "kubernetes".to_string(),
                "kubernetes.default".to_string(),
            ];
            super::tls::load_or_generate(&cert_dir, &sans)
        }
    };
    let cert = match cert_result {
        Ok(c) => Arc::new(c),
        Err(e) => {
            warn!(error = ?e, "failed to load/generate the TLS certificate; the REST/watch listener will not run");
            return;
        }
    };

    // Group H: client certificate authentication is offered but not
    // required (see server::tls's own doc comment). The CA bundle is
    // reloadable, so a valid replacement applies to new connections without
    // restarting the listener. A misconfigured initial file still disables
    // client-cert auth for this run rather than stopping the listener.
    let client_ca = match &cfg.client_ca_file {
        Some(path) => match super::tls::ReloadableClientCa::from_file(path) {
            Ok(store) => Some(store),
            Err(e) => {
                warn!(path = %path.display(), error = ?e, "failed to load NODEAPISERVER_CLIENT_CA_FILE; client certificate authentication is disabled for this run");
                None
            }
        },
        None => None,
    };

    // Group H: ServiceAccount JWTs are optional for standalone development,
    // but the nodebootstrap target supplies the cluster signing key so
    // projected pod tokens and nodelet's TokenReview fallback work before
    // RBAC enforcement is enabled.
    let service_account_authenticator = match &cfg.service_account_signing_key_file {
        Some(path) => match crate::authn::service_account::ReloadableAuthenticator::from_pem(path, cfg.service_account_issuer.clone()) {
            Ok(authenticator) => Some(Arc::new(authenticator)),
            Err(e) => {
                warn!(path = %path.display(), error = ?e, "failed to load NODEAPISERVER_SERVICE_ACCOUNT_SIGNING_KEY_FILE; the REST/watch listener will not run");
                return;
            }
        },
        None => None,
    };

    // Group H: the upstream-compatible static token file is optional. A
    // malformed initial file disables the listener rather than leaving a
    // partially loaded token table in place; later malformed rotations are
    // handled by ReloadableAuthenticator, which retains the last valid table.
    let bootstrap_token_authenticator = match &cfg.bootstrap_token_file {
        Some(path) => match crate::authn::bootstrap_token::ReloadableAuthenticator::from_file(path) {
            Ok(authenticator) => Some(Arc::new(authenticator)),
            Err(error) => {
                warn!(path = %path.display(), error, "failed to load NODEAPISERVER_TOKEN_AUTH_FILE; the REST/watch listener will not run");
                return;
            }
        },
        None => None,
    };

    // Group H: OIDC is optional, but a configured issuer must complete
    // discovery and load a usable JWKS before its bearer tokens are accepted.
    // If that setup fails, keep OIDC disabled rather than accepting tokens
    // without a verified identity.
    let oidc_authenticator = match (&cfg.oidc_issuer_url, &cfg.oidc_client_id) {
        (Some(issuer_url), Some(client_id)) => {
            let ca_certificate_pem = match &cfg.oidc_ca_file {
                Some(path) => match std::fs::read(path) {
                    Ok(bytes) => Some(bytes),
                    Err(error) => {
                        warn!(path = %path.display(), error = ?error, "failed to read NODEAPISERVER_OIDC_CA_FILE; OIDC authentication is disabled for this run");
                        None
                    }
                },
                None => None,
            };
            if cfg.oidc_ca_file.is_some() && ca_certificate_pem.is_none() {
                None
            } else {
                let oidc_config = crate::authn::oidc::Config {
                    issuer_url: issuer_url.clone(),
                    client_id: client_id.clone(),
                    username_claim: cfg.oidc_username_claim.clone(),
                    username_prefix: cfg.oidc_username_prefix.clone(),
                    groups_claim: cfg.oidc_groups_claim.clone(),
                    groups_prefix: cfg.oidc_groups_prefix.clone(),
                    required_claims: cfg.oidc_required_claims.clone(),
                    signing_algs: cfg.oidc_signing_algs.clone(),
                    ca_certificate_pem,
                };
                match crate::authn::oidc::Authenticator::from_config(oidc_config).await {
                    Ok(authenticator) => Some(Arc::new(authenticator)),
                    Err(error) => {
                        warn!(issuer = %issuer_url, error = ?error, "OIDC discovery/JWKS initialization failed; OIDC authentication is disabled for this run");
                        None
                    }
                }
            }
        }
        _ => None,
    };

    let authorization_webhook = match cfg.authorization_webhook_url.clone() {
        Some(url) => match crate::authz::webhook::WebhookAuthorizer::new_with_cache_ttls(
            url.clone(),
            cfg.authorization_webhook_authorized_ttl,
            cfg.authorization_webhook_unauthorized_ttl,
        ) {
            Ok(authorizer) => Some(Arc::new(authorizer)),
            Err(error) => {
                warn!(%url, error = ?error, "invalid NODEAPISERVER_AUTHORIZATION_WEBHOOK_URL; the REST/watch listener will not run");
                return;
            }
        },
        None => None,
    };

    let audit_sink = match cfg.audit_log_path.as_deref() {
        Some(path) => match crate::audit::sink::AuditSink::open(path) {
            Ok(sink) => {
                info!(path = %path.display(), "nodeapiserver: opened audit log");
                Some(Arc::new(sink))
            }
            Err(error) => {
                warn!(path = %path.display(), error = ?error, "failed to open NODEAPISERVER_AUDIT_LOG_PATH; the REST/watch listener will not run");
                return;
            }
        },
        None => None,
    };
    let audit_policy = match cfg.audit_policy_file.as_deref() {
        Some(path) => match crate::audit::policy::AuditPolicy::from_file(path) {
            Ok(policy) => {
                info!(path = %path.display(), "nodeapiserver: loaded audit policy");
                Some(Arc::new(policy))
            }
            Err(error) => {
                warn!(path = %path.display(), error, "failed to load NODEAPISERVER_AUDIT_POLICY_FILE; the REST/watch listener will not run");
                return;
            }
        },
        None => None,
    };

    // Group C: load and validate `EncryptionConfiguration` *before*
    // connecting to nodestore — a misconfigured file is a real, loud
    // startup failure this way, and the parsed config needs to be ready
    // to attach to `storage` the moment it exists, before any clone of
    // it (the cache-registry spawn loop below, or a per-connection clone
    // in the accept loop) gets made without it.
    let encryption_config = match &cfg.encryption_config_file {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(yaml) => match crate::storage::encryption_config::parse(&yaml) {
                Ok(parsed) => {
                    info!(path = %path.display(), entries = parsed.entries.len(), "nodeapiserver: loaded EncryptionConfiguration");
                    Some(parsed)
                }
                Err(e) => {
                    warn!(path = %path.display(), error = ?e, "invalid NODEAPISERVER_ENCRYPTION_CONFIG_FILE; continuing with no encryption-at-rest");
                    None
                }
            },
            Err(e) => {
                warn!(path = %path.display(), error = ?e, "failed to read NODEAPISERVER_ENCRYPTION_CONFIG_FILE; continuing with no encryption-at-rest");
                None
            }
        },
        None => None,
    };

    // Best-effort, matching every other failure in this function: a
    // nodestore that isn't reachable yet at startup shouldn't stop the
    // listener from serving discovery (which needs no storage at all) —
    // `rest::get` degrades to the bring-up echo stub when this is `None`
    // (see its own call site's comment). Connected once here and cloned
    // per connection below: `StorageClient` wraps a cheap-to-clone
    // `tonic::transport::Channel`, the same "clone per use, don't share a
    // `&mut` behind a lock" posture `cacher`'s own driver takes.
    // `with_encryption` attaches Group C's config to `storage` right
    // away — before `cache_registry.spawn` below ever clones it — so
    // every clone made from this point on (including every long-running
    // background reflect loop) carries it too.
    let storage = match StorageClient::connect(&cfg).await {
        Ok(c) => Some(c.with_encryption(encryption_config)),
        Err(e) => {
            warn!(error = ?e, "failed to connect to nodestore at startup; resource GET requests will fall back to the bring-up echo stub until this succeeds");
            None
        }
    };

    // Group D: register one reflector for every built-in resource in the
    // generated discovery table. `StorageClient::clone()` is cheap (a
    // `tonic::transport::Channel` clone), and each reflector shares the
    // same nodestore connection pool while keeping one cache per GVR, like
    // a real informer factory.
    let cache_registry = crate::cacher::CacheRegistry::new();
    if let Some(s) = storage.as_ref() {
        for resource in crate::codegen::api_resources::API_RESOURCES {
            cache_registry.spawn(s.clone(), resource.group, resource.version, resource.resource);
        }

        // Group K: CRD-backed caches follow the CRD watch rather than
        // waiting for a client to issue the first watch against each new
        // resource. This also retires reflectors when a CRD is removed or
        // stops serving a version, so a deleted definition cannot leave a
        // stale resource cache alive in this process.
        if let Some(crd_cache) = cache_registry.get("apiextensions.k8s.io", "v1", "customresourcedefinitions") {
            let crd_storage = s.clone();
            let crd_registry = cache_registry.clone();
            tokio::spawn(async move {
                reconcile_crd_caches(crd_storage, crd_cache, crd_registry).await;
            });
        } else {
            warn!("crd cache: built-in CustomResourceDefinition cache was not registered");
        }
    }

    // Group L Phase 2: the live `APIService` availability reconciliation
    // loop (`aggregator::reconcile`'s own doc comment covers the real
    // scope) — best effort, same posture the cache-registry spawn loop
    // just above already has: no storage at startup just means this
    // loop never runs, not a reason to stop the listener. A fixed
    // interval, not watch-driven (`aggregator::reconcile`'s own real
    // work — a Service/EndpointSlice health check, a live network dial —
    // is exactly the kind of externally-changing state real upstream's
    // own controller resyncs periodically for too, not purely reactive
    // to `APIService` object mutations).
    if let Some(s) = storage.as_ref() {
        let mut reconcile_storage = s.clone();
        tokio::spawn(async move {
            loop {
                match crate::aggregator::reconcile::reconcile_once(&mut reconcile_storage).await {
                    Ok(n) if n > 0 => info!(reconciled = n, "aggregator: reconciled APIService availability"),
                    Ok(_) => {}
                    Err(e) => warn!(error = ?e, "aggregator: APIService availability reconciliation pass failed"),
                }
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        });
    }

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
    info!(%addr, storage_connected = storage.is_some(), enforce_rbac = cfg.enforce_rbac, anonymous_auth = cfg.anonymous_auth, cached_resources = crate::codegen::api_resources::API_RESOURCES.len(), "nodeapiserver: REST/watch listener up (discovery + GET/LIST/CREATE/DELETE/UPDATE/PATCH/DELETECOLLECTION/WATCH are real; unsupported paths remain bring-up stubs — see server::listener's own doc comment)");
    let enforce_rbac = cfg.enforce_rbac;
    let concurrency_limiter = Arc::new(crate::flowcontrol::limiter::ConcurrencyLimiter::new(
        cfg.apf_max_requests_inflight,
        cfg.apf_max_mutating_requests_inflight,
        cfg.apf_queue_length_limit,
    ));
    let anonymous_auth = cfg.anonymous_auth;

    // Group N: built once at startup, not per request — the TLS config
    // itself doesn't depend on which pod/node a given `pods/log` request
    // targets, only on this crate's own static configuration
    // (`NODEAPISERVER_KUBELET_CLIENT_CERT_FILE`/`_KEY_FILE`). Best-effort
    // like everything else here: a misconfigured cert/key pair falls
    // back to no client identity (the same "connects, but nodelet's own
    // TokenReview fallback path has nothing to accept" situation an
    // unset config already produces), logged rather than stopping the
    // listener.
    let kubelet_client_cert_key = match (&cfg.kubelet_client_cert_file, &cfg.kubelet_client_key_file) {
        (Some(cert), Some(key)) => Some((cert.as_path(), key.as_path())),
        _ => None,
    };
    let kubelet_tls = std::sync::Arc::new(match crate::proxy::client_tls::build_client_config(kubelet_client_cert_key) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = ?e, "failed to build the kubelet-proxy TLS client config with the configured client cert; falling back to no client identity");
            crate::proxy::client_tls::build_client_config(None).expect("a client config with no client cert must always succeed")
        }
    });

    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = ?e, "listener: accept failed");
                continue;
            }
        };
        let cert = cert.clone();
        let client_ca = client_ca.clone();
        let storage = storage.clone();
        let cache_registry = cache_registry.clone();
        let kubelet_tls = kubelet_tls.clone();
        let service_account_authenticator = service_account_authenticator.clone();
        let oidc_authenticator = oidc_authenticator.clone();
        let bootstrap_token_authenticator = bootstrap_token_authenticator.clone();
        let authorization_webhook = authorization_webhook.clone();
        let concurrency_limiter = concurrency_limiter.clone();
        let audit_sink = audit_sink.clone();
        let audit_policy = audit_policy.clone();
        tokio::spawn(async move {
            let client_ca_store = client_ca.as_ref().map(super::tls::ReloadableClientCa::current);
            let server_config = match cert.server_config(client_ca_store.as_ref()) {
                Ok(config) => config,
                Err(error) => {
                    warn!(%peer, error = ?error, "listener: failed to build the TLS server config for the connection");
                    return;
                }
            };
            let acceptor = TlsAcceptor::from(Arc::new(server_config));
            let tls_stream = match acceptor.accept(tcp).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(%peer, error = ?e, "listener: TLS handshake failed");
                    return;
                }
            };
            // Group H: if the client presented a certificate and it chains
            // to the configured CA (rustls already verified this during
            // the handshake above — `with_client_cert_verifier`'s job, not
            // this code's), extract its identity. `None` either because no
            // client-cert auth is configured at all, or because this
            // particular client didn't present one — both are the same
            // "unauthenticated by x509" outcome from here.
            let identity = tls_stream.get_ref().1.peer_certificates().and_then(|certs| certs.first()).and_then(|leaf| crate::authn::x509::identity_from_der(leaf.as_ref()));
            let io = TokioIo::new(tls_stream);
            let service = hyper::service::service_fn(move |req| handle_with_audit(req, storage.clone(), cache_registry.clone(), identity.clone(), bootstrap_token_authenticator.clone(), service_account_authenticator.clone(), oidc_authenticator.clone(), authorization_webhook.clone(), concurrency_limiter.clone(), audit_sink.clone(), audit_policy.clone(), anonymous_auth, enforce_rbac, peer, kubelet_tls.clone()));
            if let Err(e) = ConnBuilder::new(TokioExecutor::new()).serve_connection_with_upgrades(io, service).await {
                tracing::debug!(%peer, error = ?e, "listener: connection ended");
            }
        });
    }
}

/// Outcome of trying to route a path as one of the five non-resource
/// discovery endpoints. Kept distinct from a plain `Option<Value>` so the
/// caller can tell "not a discovery-shaped path at all, fall through to
/// resource handling" apart from "was discovery-shaped, but this build
/// serves no such group/version" — the latter is a real `404`, not a
/// silent fallthrough into the resource-request echo stub, which would
/// otherwise mis-describe a `/apis/totally.made.up/v1` request as some
/// kind of resource request.
enum DiscoveryRoute {
    NotApplicable,
    Found(serde_json::Value),
    /// Same as `Found`, but the bytes are already-serialized JSON (an
    /// `/openapi/v3/<path>` document, embedded verbatim at build time) —
    /// serving them directly avoids a pointless parse-then-reserialize
    /// round trip through `serde_json::Value` for a payload that can be
    /// tens of kilobytes.
    FoundRaw(&'static [u8]),
    NotFound,
}

/// `true` if `accept_header` asks for aggregated discovery v2
/// (`as=APIGroupDiscoveryList;v=v2;g=apidiscovery.k8s.io`) via
/// `codec::negotiation` — the same header real client-go's aggregated
/// discovery client sends when it wants one `/api`/`/apis` call instead of
/// the legacy `/apis` + one `/apis/{group}/{version}` per group-version.
/// Requires an exact `v2` match (not `v2beta1`, the pre-GA shape this
/// crate doesn't separately model) rather than accepting any version
/// under that group, so a client asking for a shape this build doesn't
/// actually build never silently gets served a possibly-wrong one.
fn wants_aggregated_discovery(accept_header: Option<&str>) -> bool {
    let Some(header) = accept_header else { return false };
    let Some(accepted) = negotiation::negotiate(header) else { return false };
    accepted.as_kind.as_deref() == Some("APIGroupDiscoveryList") && accepted.as_group.as_deref() == Some("apidiscovery.k8s.io") && accepted.as_version.as_deref() == Some("v2")
}

const AGGREGATED_DISCOVERY_CONTENT_TYPE: &str = "application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList";

fn discovery_content_type(parts: &[String], accept_header: Option<&str>) -> &'static str {
    if parts.len() == 1 && matches!(parts.first().map(String::as_str), Some("api") | Some("apis")) && wants_aggregated_discovery(accept_header) {
        AGGREGATED_DISCOVERY_CONTENT_TYPE
    } else {
        "application/json"
    }
}

/// Pure and unit-tested (unlike `handle`, which needs a live TLS
/// connection to exercise at all): `parts` is the already-split, prefix-
/// intact path (`["api", "v1"]`, `["apis", "apps", "v1"]`, ...) from
/// [`path::split_path`]. `accept_header` is the raw `Accept` header value,
/// if any — its only job here is picking legacy vs. aggregated discovery
/// for the two group-list routes (`/api`, `/apis`); every other route
/// ignores it entirely (it already only serves one shape).
/// `crds` — Group K's own discovery merge: every served, `Established`
/// CRD's resources, only ever non-empty for an `/apis`-prefixed path
/// (the core group at `/api` never has CRDs in it — a CRD's own
/// `spec.group` is never empty, real upstream's own CRD validation
/// requires it). `handle`'s own call site fetches this live (one `LIST`
/// of `customresourcedefinitions`) only when the path actually starts
/// with `apis`, rather than paying that cost on every single discovery
/// request — see that call site's own comment.
/// The pure decision half of Group L Phase 3's live discovery proxy: is
/// `parts` exactly a bare `/apis/{group}/{version}` path (`route_discovery`'s
/// own `NotFound` outcome for it means no local answer exists at all —
/// not statically, not via a CRD), and does `aggregated` (the same
/// pre-flight-gated live list `server::listener::handle`'s own caller
/// already fetched) claim that exact `(group, version)`? `Some` hands
/// back borrowed references into `parts`/`aggregated` themselves — no
/// cloning needed, the caller only ever uses them for one more `resolve`
/// call before either succeeding or falling through to a real `404`.
fn aggregated_discovery_group_version<'a>(parts: &'a [String], aggregated: &'a [(String, String)]) -> Option<(&'a str, &'a str)> {
    if parts.len() != 3 || parts[0] != "apis" {
        return None;
    }
    aggregated.iter().find(|(g, v)| g == &parts[1] && v == &parts[2]).map(|(g, v)| (g.as_str(), v.as_str()))
}

fn route_discovery(parts: &[String], accept_header: Option<&str>, crds: &[crate::apiextensions::registry::DiscoverableResource], aggregated: &[(String, String)]) -> DiscoveryRoute {
    let seg = |i: usize| parts.get(i).map(String::as_str);
    match (seg(0), seg(1), parts.len()) {
        (Some("api"), _, 1) if wants_aggregated_discovery(accept_header) => DiscoveryRoute::Found(discovery::api_v1_group_discovery_list_with_crds()),
        (Some("api"), _, 1) => DiscoveryRoute::Found(discovery::api_versions()),
        (Some("api"), _, 2) => match discovery::api_resource_list("", &parts[1]) {
            Some(doc) => DiscoveryRoute::Found(doc),
            None => DiscoveryRoute::NotFound,
        },
        (Some("apis"), _, 1) if wants_aggregated_discovery(accept_header) => DiscoveryRoute::Found(discovery::api_group_discovery_list_with_crds(crds, aggregated)),
        (Some("apis"), _, 1) => DiscoveryRoute::Found(discovery::api_group_list_with_crds(crds, aggregated)),
        (Some("apis"), _, 2) => match discovery::api_group_with_crds(&parts[1], crds, aggregated) {
            Some(doc) => DiscoveryRoute::Found(doc),
            None => DiscoveryRoute::NotFound,
        },
        (Some("apis"), _, 3) => match discovery::api_resource_list_with_crds(&parts[1], &parts[2], crds) {
            Some(doc) => DiscoveryRoute::Found(doc),
            None => DiscoveryRoute::NotFound,
        },
        (Some("openapi"), Some("v2"), 2) => DiscoveryRoute::Found(openapi::v2()),
        (Some("openapi"), Some("v3"), 2) => DiscoveryRoute::Found(openapi::root()),
        (Some("openapi"), Some("v3"), n) if n > 2 => match openapi::doc(&parts[2..].join("/")) {
            Some(bytes) => DiscoveryRoute::FoundRaw(bytes),
            None => DiscoveryRoute::NotFound,
        },
        (Some("version"), _, 1) => DiscoveryRoute::Found(version::info()),
        _ => DiscoveryRoute::NotApplicable,
    }
}

/// A minimal `meta/v1.Status` body for a `404` — real upstream's full
/// `Status` type (structured `details.causes`, per-reason `retryAfter`,
/// ...) isn't built yet (Group E/J territory), but `kind`/`apiVersion`/
/// `status`/`message`/`reason`/`code` is exactly what `client-go`'s own
/// `errors.NewNotFound`-decoding path (`apimachinery/pkg/api/errors`)
/// reads off an error response, so this shape is a real, not approximate,
/// subset rather than an invented one.
fn not_found_status(path_str: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("the server could not find the requested resource ({path_str})"),
        "reason": "NotFound",
        "details": {},
        "code": 404,
    })
}

/// Same minimal `Status` shape as [`not_found_status`], for the one real
/// failure mode `rest::get` can hit that isn't "not found" — a nodestore
/// request that itself errored (connection drop, decode failure on
/// malformed stored data, ...).
fn internal_error_status(path_str: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("the server encountered an internal error handling {path_str}"),
        "reason": "InternalError",
        "details": {},
        "code": 500,
    })
}

fn unauthorized_status(path_str: &str, detail: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: {detail}"),
        "reason": "Unauthorized",
        "details": {},
        "code": 401,
    })
}

/// Same minimal `Status` shape again, for a request the client itself
/// malformed (today: an unparsable `labelSelector`/`fieldSelector`) —
/// real upstream's `reason: "BadRequest"`, `code: 400`.
fn bad_request_status(path_str: &str, detail: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: {detail}"),
        "reason": "BadRequest",
        "details": {},
        "code": 400,
    })
}

/// Same minimal `Status` shape again, for an RBAC denial (`enforce_rbac`
/// only — see this module's own doc comment) — real upstream's
/// `reason: "Forbidden"`, `code: 403`.
fn forbidden_status(path_str: &str, user_name: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: User {user_name:?} does not have permission for this request (RBAC)"),
        "reason": "Forbidden",
        "details": {},
        "code": 403,
    })
}

/// Same minimal `Status` shape, for a Group J admission denial (today:
/// only `admission::namespace_lifecycle`) — real upstream's `reason:
/// "Forbidden"`, `code: 403`, same as an RBAC denial's shape but carrying
/// the plugin's own message rather than a generic "does not have
/// permission" one.
fn admission_forbidden_status(path_str: &str, detail: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: {detail}"),
        "reason": "Forbidden",
        "details": {},
        "code": 403,
    })
}

fn admission_webhook_error_response(
    path_str: &str,
    error: &admission::webhook::Error,
) -> Response<BoxedBody> {
    match error {
        admission::webhook::Error::DryRunUnsupported { detail, .. } => {
            json_response(StatusCode::BAD_REQUEST, &bad_request_status(path_str, detail))
        }
        _ => json_response(StatusCode::INTERNAL_SERVER_ERROR, &internal_error_status(path_str)),
    }
}

/// Real upstream's own shape for a proxy subresource (`pods/log`, ...)
/// whose dial to the real backend (nodelet) itself failed — `reason:
/// "" ` (upstream doesn't set one for this case either), `code: 502`,
/// distinct from [`internal_error_status`]'s `500` because the fault is
/// nodelet/the network, not this process.
fn bad_gateway_status(path_str: &str, detail: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: {detail}"),
        "reason": "",
        "details": {},
        "code": 502,
    })
}

/// Real upstream's own `ServiceUnavailable` shape — used here when an
/// aggregated `APIService`'s own pre-flight check
/// (`aggregator::availability::preflight_check`) fails: the backing
/// Service/EndpointSlice state itself is the fault, not this process nor
/// the backend's own dial (that's [`bad_gateway_status`]'s case
/// instead), matching real upstream's own `errors.NewServiceUnavailable`
/// for the identical real situation.
fn service_unavailable_status(path_str: &str, detail: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: {detail}"),
        "reason": "ServiceUnavailable",
        "details": {},
        "code": 503,
    })
}

fn too_many_requests_status(path_str: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: the API request queue is full"),
        "reason": "TooManyRequests",
        "details": {},
        "code": 429,
    })
}

/// Real upstream's own `AlreadyExists` shape for a `CREATE` that lost the
/// create-only-if-absent race — `reason: "AlreadyExists"`, `code: 409`.
fn conflict_status(path_str: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: object already exists"),
        "reason": "AlreadyExists",
        "details": {},
        "code": 409,
    })
}

fn precondition_failed_status(path_str: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: delete precondition failed"),
        "reason": "Conflict",
        "details": {},
        "code": 409,
    })
}

/// Real upstream's own `Invalid` shape for a `CREATE` that failed
/// `scheme::validation` — `reason: "Invalid"`, `code: 422`. Real
/// upstream's full `Status.details.causes` (one structured entry per
/// violation) isn't built — `message` joins every violation into one
/// human-readable string instead, same "real subset, not the full type"
/// posture every other `Status` builder in this module already takes.
fn invalid_status(path_str: &str, violations: &[String]) -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str} is invalid: {}", violations.join("; ")),
        "reason": "Invalid",
        "details": {},
        "code": 422,
    })
}

/// Real upstream's own `user.Anonymous`/`user.AllUnauthenticated`
/// constants — what a request with no established identity is treated
/// as for authorization purposes (RBAC then denies it unless some policy
/// explicitly grants access to `system:anonymous`/`system:unauthenticated`,
/// same as real upstream).
const ANONYMOUS_USERNAME: &str = "system:anonymous";
const UNAUTHENTICATED_GROUP: &str = "system:unauthenticated";

/// Group J: persists `ResourceQuota.status.used` after a successful pod/
/// PVC/service `CREATE`, or the generic object-count evaluator's own
/// `count/<resource>` fallback — real upstream's own
/// `quotaAccessor.UpdateQuotaStatus`
/// (`plugin/pkg/admission/resourcequota/apis/resourcequota/...`),
/// scoped to whichever evaluator's own `usage_after_*_create` the caller
/// already computed. A bounded retry (3 attempts) on a real optimistic-
/// concurrency `Conflict` from `rest::update_status` re-reads the quota
/// and merges again, same "retry on lost race" posture every other write
/// path in this crate already uses. **Read-modify-write, not
/// overwrite**: only the keys the calling evaluator itself tracks are
/// replaced in the quota's existing `status.used` map — every
/// `ResourceQuota` evaluator this crate has now persists its own
/// `status.used` this way, so the read-modify-write is what keeps them
/// from clobbering each other's keys, not a "some evaluator doesn't
/// persist yet" gap. Every failure (quota vanished, storage error, retries
/// exhausted) is logged and dropped — a status write is bookkeeping, not the
/// admission decision itself, which has already succeeded by the time
/// this runs.
async fn persist_quota_usage_updates(client: &mut StorageClient, namespace: &str, updates: Vec<(String, std::collections::BTreeMap<String, crate::scheme::quantity::Quantity>)>, path_str: &str) {
    for (quota_name, new_usage) in updates {
        for _attempt in 0..3 {
            let current = match rest::get(client, None, "", "v1", "resourcequotas", Some(namespace), &quota_name).await {
                Ok(rest::GetOutcome::Found(q)) => q,
                Ok(rest::GetOutcome::ObjectNotFound) | Ok(rest::GetOutcome::UnknownResource) => break,
                Err(e) => {
                    warn!(path = %path_str, %quota_name, error = ?e, "admission: reading ResourceQuota to persist status.used failed");
                    break;
                }
            };
            let mut merged: std::collections::BTreeMap<String, crate::scheme::quantity::Quantity> = current
                .pointer("/status/used")
                .and_then(serde_json::Value::as_object)
                .map(|m| m.iter().filter_map(|(k, v)| v.as_str().and_then(|s| crate::scheme::quantity::Quantity::parse(s).ok()).map(|q| (k.clone(), q))).collect())
                .unwrap_or_default();
            for (k, v) in &new_usage {
                merged.insert(k.clone(), *v);
            }
            let mut status_body = current.clone();
            status_body["status"]["used"] = serde_json::Value::Object(merged.iter().map(|(k, v)| (k.clone(), serde_json::Value::String(v.to_string()))).collect());

            match rest::update_status(client, "", "v1", "resourcequotas", Some(namespace), &quota_name, &status_body, false).await {
                Ok(rest::UpdateOutcome::Updated(_)) => break,
                Ok(rest::UpdateOutcome::Conflict) => continue,
                Ok(_) => break,
                Err(e) => {
                    warn!(path = %path_str, %quota_name, error = ?e, "admission: persisting ResourceQuota.status.used failed");
                    break;
                }
            }
        }
    }
}
