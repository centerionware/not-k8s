//! Group E: the listener, handler chain (authn -> authz -> APF -> admission
//! -> REST), path grammar, discovery, OpenAPI endpoints.
//!
//! `path` — the REST path grammar (`RequestInfo`): a faithful port of
//! upstream's own `RequestInfoFactory.NewRequestInfo`, pure and fully
//! unit-tested against upstream's own documented example paths.
//!
//! Status: in progress (see docs/APISERVER.md). Landed: the path grammar.
//! **Not yet landed**: the actual hyper/h2/rustls listener, the handler
//! chain itself (today nothing calls `path::parse` — there is no request
//! to call it on), discovery (`/api`, `/apis`, aggregated discovery v2),
//! `/openapi/v2`+`/openapi/v3`, `/version`. The handler-chain order
//! (authentication -> authorization -> priority-and-fairness -> admission
//! -> REST) is a hard requirement once it exists, not a style choice
//! (`docs/APISERVER.md`'s own "honest engineering problem" section).

pub mod path;
