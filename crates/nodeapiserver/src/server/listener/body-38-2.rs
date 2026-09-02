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
