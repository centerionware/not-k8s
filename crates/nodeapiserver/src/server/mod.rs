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
//! `version_compare` — `CompareKubeAwareVersionStrings`, a faithful port
//! (GA beats beta beats alpha, then major, then minor — maturity compared
//! *before* major version, a real bug this module's own tests caught in
//! an earlier draft that compared major version first).
//! `discovery` — `/api`/`/apis`/`/apis/{group}` document builders, driven
//! entirely by Group A's discovery GVK table. **Group-level only** — the
//! per-version `APIResourceList` (`/api/v1`, `/apis/{group}/{version}`)
//! needs data this crate hasn't vendored yet (the OpenAPI spec's `paths`
//! section), named honestly in that module's own doc comment rather than
//! implied by what's here. Not yet wired into `listener`'s routing.
//!
//! Status: in progress (see docs/APISERVER.md). Landed: the path grammar,
//! a real TLS listener proving the grammar and transport work together,
//! and group-level discovery document builders. **Not yet landed**: the
//! handler chain itself (authn -> authz -> APF -> admission -> REST — a
//! hard requirement on order, not a style choice, once it exists), wiring
//! discovery into the listener's actual routing, per-version resource
//! discovery, aggregated discovery v2, `/openapi/v2` + `/openapi/v3`,
//! `/version`.

pub mod path;
pub mod tls;
pub mod listener;
pub mod version_compare;
pub mod discovery;
