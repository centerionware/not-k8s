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
        Some(path) => match crate::authn::service_account::ReloadableAuthenticator::from_pem(
            path,
            cfg.service_account_issuer.clone(),
        ) {
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
        Some(path) => match crate::authn::bootstrap_token::ReloadableAuthenticator::from_file(path)
        {
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

    let authorization_webhook = match (
        cfg.authorization_webhook_url.as_deref(),
        cfg.authorization_webhook_config_file.as_deref(),
    ) {
        (Some(url), None) => match crate::authz::webhook::WebhookAuthorizer::new_with_cache_ttls(
            url.to_string(),
            cfg.authorization_webhook_authorized_ttl,
            cfg.authorization_webhook_unauthorized_ttl,
        ) {
            Ok(authorizer) => Some(Arc::new(authorizer)),
            Err(error) => {
                warn!(%url, error = ?error, "invalid NODEAPISERVER_AUTHORIZATION_WEBHOOK_URL; the REST/watch listener will not run");
                return;
            }
        },
        (None, Some(path)) => match crate::authz::webhook::WebhookAuthorizer::from_kubeconfig(
            path,
            cfg.authorization_webhook_authorized_ttl,
            cfg.authorization_webhook_unauthorized_ttl,
        ) {
            Ok(authorizer) => {
                info!(path = %path.display(), "nodeapiserver: configured authorization webhook from kubeconfig");
                Some(Arc::new(authorizer))
            }
            Err(error) => {
                warn!(path = %path.display(), error = ?error, "failed to load NODEAPISERVER_AUTHORIZATION_WEBHOOK_CONFIG_FILE; the REST/watch listener will not run");
                return;
            }
        },
        (None, None) => None,
        (Some(_), Some(_)) => {
            warn!(
                "authorization webhook URL and config file are mutually exclusive; the REST/watch listener will not run"
            );
            return;
        }
    };

    // Group L: aggregated API servers may use the request-header authenticator
    // and therefore require the apiserver's trusted front-proxy client
    // certificate. Load it once here; the per-APIService CA bundle still
    // controls the backend serving certificate independently.
    let aggregation_proxy_identity = match (&cfg.proxy_client_cert_file, &cfg.proxy_client_key_file)
    {
        (Some(cert), Some(key)) => {
            match crate::aggregator::client_tls::ClientIdentity::from_files(cert, key) {
                Ok(identity) => Some(Arc::new(identity)),
                Err(error) => {
                    warn!(cert = %cert.display(), key = %key.display(), error = ?error, "failed to load the aggregation proxy client identity; the REST/watch listener will not run");
                    return;
                }
            }
        }
        _ => None,
    };

    let audit_webhook = match (
        cfg.audit_webhook_url.as_deref(),
        cfg.audit_webhook_config_file.as_deref(),
    ) {
        (Some(url), None) => match crate::audit::webhook::AuditWebhook::new(url) {
            Ok(webhook) => {
                info!(%url, "nodeapiserver: configured audit webhook");
                Some(webhook)
            }
            Err(error) => {
                warn!(%url, error, "invalid NODEAPISERVER_AUDIT_WEBHOOK_URL; the REST/watch listener will not run");
                return;
            }
        },
        (None, Some(path)) => match crate::audit::webhook::AuditWebhook::from_kubeconfig(path) {
            Ok(webhook) => {
                info!(path = %path.display(), "nodeapiserver: configured audit webhook from kubeconfig");
                Some(webhook)
            }
            Err(error) => {
                warn!(path = %path.display(), error, "failed to load NODEAPISERVER_AUDIT_WEBHOOK_CONFIG_FILE; the REST/watch listener will not run");
                return;
            }
        },
        (None, None) => None,
        (Some(_), Some(_)) => {
            warn!(
                "audit webhook URL and config file are mutually exclusive; the REST/watch listener will not run"
            );
            return;
        }
    };
    let audit_sink = match cfg.audit_log_path.as_deref() {
        Some(path) => match crate::audit::sink::AuditSink::open_with_rotation(
            path,
            cfg.audit_log_max_size_bytes,
            cfg.audit_log_max_backups,
        ) {
            Ok(sink) => {
                info!(path = %path.display(), "nodeapiserver: opened audit log");
                Some(Arc::new(match audit_webhook {
                    Some(webhook) => sink.with_webhook(webhook),
                    None => sink,
                }))
            }
            Err(error) => {
                warn!(path = %path.display(), error = ?error, "failed to open NODEAPISERVER_AUDIT_LOG_PATH; the REST/watch listener will not run");
                return;
            }
        },
        None => audit_webhook
            .map(|webhook| Arc::new(crate::audit::sink::AuditSink::webhook_only(webhook))),
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

    // Build the nodestore channel lazily so a nodestore that is still
    // restarting cannot hold the API listener before it binds. Tonic
    // reconnects the channel when reflectors and request handlers make their
    // first RPC; until then, discovery and health endpoints remain available
    // while storage-backed requests report their normal backend error.
    // `StorageClient` wraps a cheap-to-clone `tonic::transport::Channel`, the
    // same "clone per use, don't share a `&mut` behind a lock" posture
    // `cacher`'s own driver takes.
    // `with_encryption` attaches Group C's config to `storage` right
    // away — before `cache_registry.spawn` below ever clones it — so
    // every clone made from this point on (including every long-running
    // background reflect loop) carries it too.
    let storage = match StorageClient::connect_lazy(&cfg) {
        Ok(c) => Some(c.with_encryption(encryption_config)),
        Err(e) => {
            warn!(error = ?e, "failed to configure nodestore client; resource requests will return 503 until configuration is fixed");
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
            cache_registry.spawn(
                s.clone(),
                resource.group,
                resource.version,
                resource.resource,
            );
        }

        // Group K: CRD-backed caches follow the CRD watch rather than
        // waiting for a client to issue the first watch against each new
        // resource. This also retires reflectors when a CRD is removed or
        // stops serving a version, so a deleted definition cannot leave a
        // stale resource cache alive in this process.
        if let Some(crd_cache) =
            cache_registry.get("apiextensions.k8s.io", "v1", "customresourcedefinitions")
        {
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
                    Ok(n) if n > 0 => info!(
                        reconciled = n,
                        "aggregator: reconciled APIService availability"
                    ),
                    Ok(_) => {}
                    Err(e) => {
                        warn!(error = ?e, "aggregator: APIService availability reconciliation pass failed")
                    }
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
    info!(%addr, storage_connected = storage.is_some(), enforce_rbac = cfg.enforce_rbac, anonymous_auth = cfg.anonymous_auth, max_request_body_bytes = cfg.max_request_body_bytes, cached_resources = crate::codegen::api_resources::API_RESOURCES.len(), "nodeapiserver: REST/watch listener up (discovery + GET/LIST/CREATE/DELETE/UPDATE/PATCH/DELETECOLLECTION/WATCH are real; unsupported paths return Kubernetes errors — see server::listener's own doc comment)");
    let enforce_rbac = cfg.enforce_rbac;
    let concurrency_limiter = Arc::new(crate::flowcontrol::limiter::ConcurrencyLimiter::new(
        cfg.apf_max_requests_inflight,
        cfg.apf_max_mutating_requests_inflight,
        cfg.apf_queue_length_limit,
    ));
    let anonymous_auth = cfg.anonymous_auth;
    let max_request_body_bytes = cfg.max_request_body_bytes;
    // Pure admission plugins are immutable after startup. Keep one ordered
    // dispatcher for all connections instead of rebuilding its trait-object
    // chain for every write request; storage-backed plugins remain in the
    // request path because they require their own I/O and failure policy.
    let pure_admission = Arc::new(
        crate::admission::chain::MutatingRegistry::with_builtins_enabled(
            &cfg.enabled_admission_plugins,
        ),
    );
    let pod_node_selector_config = match &cfg.pod_node_selector_config_file {
        Some(path) => match crate::admission::pod_node_selector::PluginConfig::from_file(path) {
            Ok(config) => Some(Arc::new(config)),
            Err(error) => {
                warn!(path = %path.display(), %error, "failed to load PodNodeSelector configuration; the REST/watch listener will not run");
                return;
            }
        },
        None => None,
    };

    // Group N: built once at startup, not per request — the TLS config
    // itself doesn't depend on which pod/node a given `pods/log` request
    // targets, only on this crate's own static configuration
    // (`NODEAPISERVER_KUBELET_CLIENT_CERT_FILE`/`_KEY_FILE`). Best-effort
    // like everything else here: a misconfigured cert/key pair falls
    // back to no client identity (the same "connects, but nodelet's own
    // TokenReview fallback path has nothing to accept" situation an
    // unset config already produces), logged rather than stopping the
    // listener.
    let kubelet_client_cert_key =
        match (&cfg.kubelet_client_cert_file, &cfg.kubelet_client_key_file) {
            (Some(cert), Some(key)) => Some((cert.as_path(), key.as_path())),
            _ => None,
        };
    let kubelet_tls = std::sync::Arc::new(
        match crate::proxy::client_tls::build_client_config(kubelet_client_cert_key) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = ?e, "failed to build the kubelet-proxy TLS client config with the configured client cert; falling back to no client identity");
                crate::proxy::client_tls::build_client_config(None)
                    .expect("a client config with no client cert must always succeed")
            }
        },
    );

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
        let pure_admission = pure_admission.clone();
        let pod_node_selector_config = pod_node_selector_config.clone();
        let kubelet_tls = kubelet_tls.clone();
        let service_account_authenticator = service_account_authenticator.clone();
        let oidc_authenticator = oidc_authenticator.clone();
        let bootstrap_token_authenticator = bootstrap_token_authenticator.clone();
        let authorization_webhook = authorization_webhook.clone();
        let aggregation_proxy_identity = aggregation_proxy_identity.clone();
        let concurrency_limiter = concurrency_limiter.clone();
        let audit_sink = audit_sink.clone();
        let audit_policy = audit_policy.clone();
        let max_request_body_bytes = max_request_body_bytes;
        tokio::spawn(async move {
            let client_ca_store = client_ca
                .as_ref()
                .map(super::tls::ReloadableClientCa::current);
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
            let identity = tls_stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|certs| certs.first())
                .and_then(|leaf| crate::authn::x509::identity_from_der(leaf.as_ref()));
            let io = TokioIo::new(tls_stream);
            let service = hyper::service::service_fn(move |req| {
                handle_with_audit(
                    req,
                    storage.clone(),
                    cache_registry.clone(),
                    pure_admission.clone(),
                    pod_node_selector_config.clone(),
                    identity.clone(),
                    bootstrap_token_authenticator.clone(),
                    service_account_authenticator.clone(),
                    oidc_authenticator.clone(),
                    authorization_webhook.clone(),
                    aggregation_proxy_identity.clone(),
                    concurrency_limiter.clone(),
                    audit_sink.clone(),
                    audit_policy.clone(),
                    anonymous_auth,
                    enforce_rbac,
                    max_request_body_bytes,
                    peer,
                    kubelet_tls.clone(),
                )
            });
            if let Err(e) = ConnBuilder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(io, service)
                .await
            {
                tracing::debug!(%peer, error = ?e, "listener: connection ended");
            }
        });
    }
}
