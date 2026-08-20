//! Group E: the listener, handler chain (authn -> authz -> APF -> admission
//! -> REST), path grammar, discovery, OpenAPI endpoints.
//!
//! `path` — the REST path grammar (`RequestInfo`): a faithful port of
//! upstream's own `RequestInfoFactory.NewRequestInfo`, pure and fully
//! unit-tested against upstream's own documented example paths.
//! `tls` — self-signed server certificate for the listener (not the
//! cluster's real PKI — see that module's own doc comment).
//! `listener` — the real hyper + h2 + rustls listener. **Its request
//! handler is a bring-up stub**, not the real REST dispatch — see that
//! module's own doc comment.
//!
//! Status: in progress (see docs/APISERVER.md). Landed: the path grammar,
//! and a real TLS listener proving the grammar and the transport work
//! together end to end. **Not yet landed**: the handler chain itself
//! (authn -> authz -> APF -> admission -> REST — a hard requirement on
//! order, not a style choice, once it exists), discovery (`/api`, `/apis`,
//! aggregated discovery v2), `/openapi/v2` + `/openapi/v3`, `/version`.

pub mod path;
pub mod tls;
pub mod listener;
