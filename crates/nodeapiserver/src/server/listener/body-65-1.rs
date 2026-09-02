    if client_cert_identity.is_some() {
        return Ok(client_cert_identity);
    }
    let Some(header) = req.headers().get("authorization") else {
        return if anonymous_auth { Ok(None) } else { Err("anonymous authentication is disabled") };
    };
    let value = header.to_str().map_err(|_| "Authorization header is not valid UTF-8")?;
    let Some(token) = value.strip_prefix("Bearer ").filter(|token| !token.is_empty()) else {
        return Err("Authorization must use the Bearer scheme");
    };
    if let Some(authenticator) = bootstrap_token_authenticator {
        if let Some(authenticated) = authenticator.authenticate(token) {
            return Ok(Some(authenticated.identity));
        }
    }
    if let Some(authenticator) = service_account_authenticator {
        if let Some(authenticated) = authenticator.authenticate(token) {
            return Ok(Some(authenticated.identity));
        }
    }
    if let Some(authenticator) = oidc_authenticator {
        if let Some(identity) = authenticator.authenticate(token).await {
            return Ok(Some(identity));
        }
    }
    if bootstrap_token_authenticator.is_none()
        && service_account_authenticator.is_none()
        && oidc_authenticator.is_none()
    {
        return Err("bearer-token authentication is not configured");
    }
