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
//! `/apis/{group}/{version}`, plus `/healthz`/`/readyz`/`/livez` —
//! `healthz`, real upstream's own per-check response shape) **and for
//! `GET`/`LIST`/
//! `CREATE`/`DELETE`/`UPDATE`/`PATCH`/`DELETECOLLECTION`** (`rest::get`/
//! `rest::list`/`rest::create`/`rest::delete`/`rest::update`/`rest::patch`/
//! `rest::delete_collection`, against real nodestore data), **and a real
//! streaming response for `WATCH`** against a registered cache — every
//! resource verb this build knows about is now real, no bring-up stub
//! left — see that module's own doc comment.
//! `rest` — the real, generic REST verbs so far: `GET`/`LIST`/`CREATE`/
//! `DELETE`/`UPDATE`/`PATCH`/`DELETECOLLECTION`, resolving a resource's
//! Kind from Group A's discovery table. `GET`/`LIST` consult a synchronized
//! watch cache when the listener provides one, falling back to nodestore
//! for misses, unsynchronized caches, paginated reads, and callers without
//! a cache. `LIST` filters by label/field selector for real
//! (`cacher::selector::object_matches`, Group D's own generic adapter,
//! wired in unchanged). `CREATE` runs Group F's
//! `scheme::validation`/`defaulting`, sets real `creationTimestamp`/`uid`,
//! and writes with a real create-only-if-absent `Txn`. `DELETE` reads and
//! validates optional `resourceVersion`/`uid` preconditions, then uses an
//! MVCC-guarded delete/termination transaction; finalizers are honored,
//! while propagation remains out of scope. `CREATE`/`UPDATE` also support
//! `dryRun=All` without persisting. `UPDATE` is
//! real optimistic concurrency (reads current, requires the submitted
//! `resourceVersion` to match, writes with a `Txn` compared against that
//! same revision — a real `Conflict` on a mismatch or a lost race, not a
//! silent overwrite), preserving `creationTimestamp`/`uid` from the
//! existing object regardless of what the client submitted. Admission and
//! authorization are applied by `listener` before the REST operation.
//! Built-in workload `scale` subresources are also real: their virtual
//! `autoscaling/v1 Scale` object is translated to the parent's
//! `spec.replicas`.
//! `version_compare` — `CompareKubeAwareVersionStrings`, a faithful port
//! (GA beats beta beats alpha, then major, then minor — maturity compared
//! *before* major version, a real bug this module's own tests caught in
//! an earlier draft that compared major version first).
//! `discovery` — `/api`/`/apis`/`/apis/{group}` group-level document
//! builders plus `api_resource_list()` for the per-version
//! `APIResourceList` (`/api/v1`, `/apis/{group}/{version}`), driven
//! entirely by Group A's discovery tables. Wired into `listener`'s actual
//! routing now, not just a pure builder.
//! `openapi` — `/openapi/v2`, plus `/openapi/v3` and
//! `/openapi/v3/<path>`, serving a derived Swagger v2 document and every
//! vendored OpenAPI v3 document verbatim (Group A's `codegen::openapi_v3_docs`),
//! wired into `listener`'s routing alongside the other discovery routes.
//! `version` — `/version`, a `version.Info` document built from real
//! build-time facts (vendored release, git commit/tree state, build
//! date) — see that module's own doc comment for exactly what's real and
//! what's necessarily approximate (`goVersion`/`compiler` for a Rust
//! binary).
//! `watch_event` — converts one `cacher::store::WatchEvent` into the real
//! `metav1.WatchEvent` wire shape (`{"type": ..., "object": ...}`) used by
//! the live `WATCH` response stream, including the cache's retained
//! pre-delete object value.
//!
//! Status: in progress (see docs/APISERVER.md). Landed: the path grammar,
//! a real TLS listener proving the grammar and transport work together,
//! every group/version discovery document actually reachable over HTTP
//! (including a real `404` for an unknown group/version rather than a
//! silent fallthrough), `/openapi/v3`, `/version`, aggregated discovery
//! v2 (negotiated), and real `GET`/`LIST`/`CREATE`/`DELETE`/`UPDATE`/
//! `PATCH`/`DELETECOLLECTION`, gated by opt-in RBAC (Groups H/I) — `PATCH`
//! now runs the two Group J plugins that ever apply to an `Update`-shaped
//! write (`namespace_lifecycle`, `LimitRanger`'s own PVC validation);
//! `DELETECOLLECTION` also runs configured admission per selected object.
//! The core Pod `ephemeralcontainers` GET/PUT/PATCH subresource is also
//! real, with upstream's append-only existing-container strategy.
//! `watch` is also real now (a real streaming HTTP response against a
//! registered cache — this paragraph was stale about that; see
//! `listener`'s own doc comment for the full, current picture). Every
//! resource verb this build knows about is now real. **Not yet
//! landed**: the real handler chain (authn -> authz -> APF -> admission
//! -> REST — a hard requirement on order, not a style choice, once it
//! exists).

pub mod path;
pub mod tls;
pub mod listener;
pub mod version_compare;
pub mod discovery;
/// `/healthz`/`/readyz`/`/livez`'s real per-check response shape, ported
/// from upstream's own `k8s.io/apiserver/pkg/server/healthz` (see that
/// module's own doc comment for exactly which checks and which parts of
/// the wire format are and aren't ported).
pub mod healthz;
/// `/metrics` — a scoped port of real upstream's own
/// `apiserver_request_total` counter, rendered as real Prometheus text
/// exposition format (see that module's own doc comment for exactly
/// which labels are and aren't tracked).
pub mod metrics;
pub mod openapi;
pub mod version;
pub mod rest;
pub mod watch_event;
