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
//! to `handle`. Authorization (Group I) can now consult this identity too
//! — `authz::resolve::rules_for`/`rbac::rules_allow`, gating `GET`/`LIST`
//! when `NODEAPISERVER_ENFORCE_RBAC` is set — but that's opt-in and off
//! by default (see `config::Config::enforce_rbac`'s own doc comment for
//! why), so by default this is still genuinely "who are you" without "are
//! you allowed."
//!
//! `self_review` — `SelfSubjectReview` (`kubectl auth whoami`), a thin
//! reflection of whatever identity `x509` (or the anonymous fallback)
//! already produced, no new authentication logic. **Wired into
//! `server::listener`** as its own `POST` branch, same virtual-resource
//! (never persisted) posture `authz::sar`'s review kinds already
//! established.
//!
//! Status: in progress (see docs/APISERVER.md). Everything else named
//! above (ServiceAccount JWT, OIDC, TokenReview, bootstrap tokens,
//! anonymous) is not started.

pub mod self_review;
pub mod x509;
