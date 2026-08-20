//! Group E: the listener, handler chain (authn -> authz -> APF -> admission
//! -> REST), path grammar, discovery, OpenAPI endpoints.
//!
//! `path` — the REST path grammar (`RequestInfo`): a faithful port of
//! upstream's own `RequestInfoFactory.NewRequestInfo`, pure and fully
//! unit-tested against upstream's own documented example paths.
//! `tls` — self-signed server certificate for the listener (not the
//! cluster's real PKI — see that module's own doc comment).
//! `listener` — the real hyper + h2 + rustls listener. **Its request
//! handler is a real dispatch for every non-resource discovery route**
//! (`/api`, `/api/{version}`, `/apis`, `/apis/{group}`,
//! `/apis/{group}/{version}`, plus `/healthz`) **and for single-object
//! `GET`** (`rest::get`, against real nodestore data) — **every other
//! resource verb** (`list`/`watch`/`create`/`update`/`patch`/`delete`)
//! **is still a bring-up stub** that just echoes the parsed `RequestInfo`
//! — see that module's own doc comment.
//! `rest` — the first real, generic REST verb: single-object `GET`,
//! resolving a resource's Kind from Group A's discovery table, reading
//! straight from nodestore (bypassing the watch cache — named honestly as
//! a real, valid read strategy, not a shortcut; see that module's own doc
//! comment for exactly what's in and out of scope). No authn/authz/
//! admission yet — every request reaching it is currently treated as
//! allowed.
//! `version_compare` — `CompareKubeAwareVersionStrings`, a faithful port
//! (GA beats beta beats alpha, then major, then minor — maturity compared
//! *before* major version, a real bug this module's own tests caught in
//! an earlier draft that compared major version first).
//! `discovery` — `/api`/`/apis`/`/apis/{group}` group-level document
//! builders plus `api_resource_list()` for the per-version
//! `APIResourceList` (`/api/v1`, `/apis/{group}/{version}`), driven
//! entirely by Group A's discovery tables. Wired into `listener`'s actual
//! routing now, not just a pure builder.
//! `openapi` — `/openapi/v3` and `/openapi/v3/<path>`, serving every
//! vendored OpenAPI v3 document verbatim (Group A's `codegen::openapi_v3_docs`),
//! wired into `listener`'s routing alongside the other discovery routes.
//! `version` — `/version`, a `version.Info` document built from real
//! build-time facts (vendored release, git commit/tree state, build
//! date) — see that module's own doc comment for exactly what's real and
//! what's necessarily approximate (`goVersion`/`compiler` for a Rust
//! binary).
//!
//! Status: in progress (see docs/APISERVER.md). Landed: the path grammar,
//! a real TLS listener proving the grammar and transport work together,
//! every group/version discovery document actually reachable over HTTP
//! (including a real `404` for an unknown group/version rather than a
//! silent fallthrough), `/openapi/v3`, `/version`, aggregated discovery
//! v2 (negotiated), and a real single-object `GET`. **Not yet landed**:
//! the rest of the verb set, the real handler chain (authn -> authz ->
//! APF -> admission -> REST — a hard requirement on order, not a style
//! choice, once it exists), `/openapi/v2`.

pub mod path;
pub mod tls;
pub mod listener;
pub mod version_compare;
pub mod discovery;
pub mod openapi;
pub mod version;
pub mod rest;
