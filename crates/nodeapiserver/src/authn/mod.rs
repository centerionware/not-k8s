//! Group H: authentication. x509 client certs, ServiceAccount JWT
//! issuance/validation, projected/bound tokens, OIDC discovery + JWKS,
//! TokenReview, bootstrap tokens, anonymous.
//!
//! `x509` — the first real slice: derives a [`x509::Identity`] from a
//! client certificate's Subject, the same `CommonName`-as-username/
//! `Organization`-as-groups convention real upstream's own generic x509
//! authenticator uses. **Pure identity-extraction primitive only — not
//! yet wired into the listener**: `server::listener`'s TLS acceptor
//! doesn't request or verify client certificates at all yet (`with_no_client_auth`,
//! `server::tls`'s own doc comment), so there is no verified peer
//! certificate in scope for `server::rest`'s handlers to call this with.
//! Wiring real mTLS into the listener (`with_client_cert_verifier`,
//! mirroring `crates/nodelet/src/server/tls.rs::load_client_ca`'s
//! already-proven precedent in this workspace) and threading the
//! resulting identity through the handler chain is separate, not-yet-done
//! work — same "land the primitive, wire it later" split this crate has
//! taken repeatedly (aggregated discovery v2's builder landing before its
//! negotiated HTTP wiring is the most recent precedent).
//!
//! Status: in progress (see docs/APISERVER.md). Everything else named
//! above (ServiceAccount JWT, OIDC, TokenReview, bootstrap tokens,
//! anonymous) is not started.

pub mod x509;
