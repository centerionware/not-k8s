//! Group H: authentication. x509 client certs, ServiceAccount JWT
//! issuance/validation, projected/bound tokens, OIDC discovery + JWKS,
//! TokenReview, bootstrap tokens, anonymous.
//!
//! `x509` — the first real slice: derives a [`x509::Identity`] from a
//! client certificate's Subject, the same `CommonName`-as-username/
//! `Organization`-as-groups convention real upstream's own generic x509
//! authenticator uses. Wired into `server::listener` for real now:
//! `NODEAPISERVER_CLIENT_CA_FILE` (`config::Config::client_ca_file`) turns
//! on client certificate verification at the TLS layer
//! (`server::tls::load_client_ca` + `with_client_cert_verifier`, offered
//! but not required — mirroring `crates/nodelet/src/server/tls.rs`'s own
//! already-proven precedent in this workspace), and `identity_from_der`
//! turns a verified peer certificate into the `Identity` threaded through
//! to `handle`. **Authentication without authorization**: the identity is
//! real and observable (surfaced in the bring-up echo response's `user`
//! field) but **nothing checks it before serving a request yet** — there
//! is no authorization (Group I) to enforce it against, so this is
//! genuinely "who are you" without yet "are you allowed."
//!
//! Status: in progress (see docs/APISERVER.md). Everything else named
//! above (ServiceAccount JWT, OIDC, TokenReview, bootstrap tokens,
//! anonymous) is not started.

pub mod x509;
