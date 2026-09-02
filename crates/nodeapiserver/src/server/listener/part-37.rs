
async fn authenticate_request(
    req: &Request<Incoming>,
    client_cert_identity: Option<crate::authn::x509::Identity>,
    bootstrap_token_authenticator: Option<&crate::authn::bootstrap_token::ReloadableAuthenticator>,
    service_account_authenticator: Option<&crate::authn::service_account::ReloadableAuthenticator>,
    oidc_authenticator: Option<&crate::authn::oidc::Authenticator>,
    anonymous_auth: bool,
) -> std::result::Result<Option<crate::authn::x509::Identity>, &'static str> {
    include!("body-65-1.rs");
    include!("body-65-2.rs");
}
