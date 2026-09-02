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

