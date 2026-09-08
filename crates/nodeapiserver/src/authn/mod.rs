//! Group H: authentication. x509 client certs, ServiceAccount JWT
//! issuance/validation, projected/bound tokens, OIDC discovery + JWKS,
//! TokenReview, bootstrap tokens, anonymous.
//!
//! `bootstrap_token` — loads the standard `--token-auth-file` CSV format
//! into an in-memory bearer-token authenticator. The listener refreshes it
//! after file changes from `NODEAPISERVER_TOKEN_AUTH_FILE`, retaining the
//! last valid table during a malformed or unreadable rotation.
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
//! `service_account` — ES256 ServiceAccount JWT issuance and validation. The
//! nodebootstrap target supplies the cluster signing key; `TokenRequest`
//! mints stateless bound/unbound tokens and `TokenReview` validates them for
//! nodelet's bearer-token webhook path.
//!
//! `oidc` — optional discovery-backed OIDC bearer-token authentication. The
//! listener fetches the issuer metadata and JWKS at startup, verifies signed
//! tokens locally, and refreshes keys once when a rotated key is encountered.
//!
//! Bootstrap tokens and the compatible boolean anonymous-authentication
//! switch are wired through the listener. Structured anonymous-authentication
//! conditions and reload of the remaining authentication files remain outside
//! this slice.
//!
//! Status: the supported x509, ServiceAccount, OIDC, static-token, and
//! anonymous-authentication paths are implemented; see docs/APISERVER.md for
//! the deliberately narrower follow-up scope.

pub mod bootstrap_token;
pub mod oidc;
pub mod self_review;
pub mod service_account;
pub mod x509;
