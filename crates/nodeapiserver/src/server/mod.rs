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
//! `/apis/{group}/{version}`, plus `/healthz`) and **still a bring-up stub
//! for actual resource requests** (`get`/`list`/`create`/... against a real
//! resource just echoes the parsed `RequestInfo`) — see that module's own
//! doc comment.
//! `version_compare` — `CompareKubeAwareVersionStrings`, a faithful port
//! (GA beats beta beats alpha, then major, then minor — maturity compared
//! *before* major version, a real bug this module's own tests caught in
//! an earlier draft that compared major version first).
//! `discovery` — `/api`/`/apis`/`/apis/{group}` group-level document
//! builders plus `api_resource_list()` for the per-version
//! `APIResourceList` (`/api/v1`, `/apis/{group}/{version}`), driven
//! entirely by Group A's discovery tables. Wired into `listener`'s actual
//! routing now, not just a pure builder.
//!
//! Status: in progress (see docs/APISERVER.md). Landed: the path grammar,
//! a real TLS listener proving the grammar and transport work together,
//! and every discovery document (group-level and per-version) actually
//! reachable over HTTP, including a real `404` for an unknown group/version
//! rather than a silent fallthrough. **Not yet landed**: the handler chain
//! itself for actual resource requests (authn -> authz -> APF -> admission
//! -> REST — a hard requirement on order, not a style choice, once it
//! exists), aggregated discovery v2, `/openapi/v2` + `/openapi/v3`,
//! `/version`.

pub mod path;
pub mod tls;
pub mod listener;
pub mod version_compare;
pub mod discovery;
