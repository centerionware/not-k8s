//! The REST/watch listener: binds `cfg.bind_addr` over TLS, accepts
//! connections, and serves them with hyper's auto (h1/h2) connection
//! builder — h2 matters here in a way it didn't for nodelet's own HTTPS
//! server (`CLAUDE.md`: client-go and kubectl negotiate h2, and watch
//! multiplexing depends on it). Structure mirrors
//! `crates/nodelet/src/server/mod.rs`'s own `run()` closely — same
//! accept-loop/TLS-handshake/spawn-per-connection shape, adapted for a
//! listener with no reason to ever be individually disabled the way
//! nodelet's exec/logs server is.
//!
//! **`GET`, `LIST`, `CREATE`, single-object `DELETE`, and `UPDATE`
//! against a real resource are now real** (`rest::get`/`rest::list`/
//! `rest::create`/`rest::delete`/`rest::update`, generic over every
//! resource this build knows about — see `rest`'s own doc comment for
//! exactly what's in and out of scope). `run()` also spawns a real
//! `cacher::registry::CacheRegistry` cache for every built-in resource in
//! the generated discovery table; CRD-defined resources are registered
//! from the live CustomResourceDefinition cache after their Established
//! CRD is discovered. `GET`/`LIST`
//! consult one whenever the request targets a registered resource.
//! `WATCH` against a resource with a
//! registered cache is real too now (`is_watch`'s own doc comment, and
//! `watch_response_body`): the cache's own retained history replays
//! first, then live events stream as they happen, real `Transfer-Encoding`
//! framing handled by hyper's own h1/h2 connection layer. A resource with
//! no registered cache still falls through to the echo stub, same as a
//! resource `GET`/`LIST` can't yet serve from a cache. `PATCH` is real
//! too now (`rest::patch_prepare`/`patch_persist`, reusing Group G's
//! already-landed `patch::json_patch`/`merge_patch`/`strategic_merge`,
//! selected by the real `Content-Type` —
//! `application/json-patch+json`/`application/merge-patch+json`/
//! `application/strategic-merge-patch+json` — with Kubernetes' default
//! strategic-merge/CRD merge-patch selection when `Content-Type` is
//! omitted and a real `415` for an unsupported explicit type), **and now runs the two Group J plugins that ever
//! apply to an `Update`-shaped write** (`namespace_lifecycle`,
//! `LimitRanger`'s own PVC validation — the split between
//! `rest::patch_prepare`/`patch_persist` exists specifically so admission
//! can see the real candidate object in between the two). `deletecollection`
//! is real too now (it lists via the same selector filtering `LIST` already
//! has, runs configured admission against each matched object, then deletes
//! each match) — `watch` is the only remaining resource verb
//! this build knows about that isn't a real generic REST dispatch — it's
//! real too, just structurally different (a streaming response, covered
//! above).
//! Client certificate authentication is real (`super::tls`'s optional
//! `client_ca`, `authn::x509::identity_from_der` on the verified peer
//! cert), surfaced in the echo response's own `user` field for
//! observability. Authorization is real too, but **opt-in and off by
//! default** (`config::Config::enforce_rbac`/`NODEAPISERVER_ENFORCE_RBAC`
//! — see that field's own doc comment for why: enabling RBAC enforcement
//! before Group O's bootstrap `ClusterRole`/`ClusterRoleBinding` set
//! exists can lock every request out with no path back in). When
//! enabled, `GET`/`LIST`/`CREATE`/`DELETE`/`UPDATE`/`WATCH` all resolve
//! the caller's real rules (`authz::resolve::rules_for` — the real
//! anonymous identity, `system:anonymous`/`system:unauthenticated`, when
//! no x509 identity was established) and deny with a real `403` unless
//! `authz::rbac::rules_allow` says yes. Group J admission (five
//! unconditional plugins as of this revision, `admission`'s own doc
//! comment has the running list) also runs, on the real write verbs only
//! — real upstream's own admission posture too, admission never gates a
//! read. The real handler chain (authentication -> authorization ->
//! priority-and-fairness -> admission -> REST, `docs/APISERVER.md`'s own
//! hard requirement) is still not fully unified into one ordered
//! pipeline — each piece above is wired in ad hoc, in the right relative
//! order for what exists today, not through one shared dispatcher yet.
//!
//! What *is* real now: `/healthz`/`/readyz`/`/livez` (`server::healthz` —
//! real upstream's own per-check response shape, `?verbose` included; see
//! that module's own doc comment for exactly which checks are ported),
//! and every non-resource discovery route
//! (`/api`, `/api/{version}`, `/apis`, `/apis/{group}`,
//! `/apis/{group}/{version}`, `/openapi/v2`, `/openapi/v3(/...)`, `/version`) is answered
//! by `server::discovery`/`openapi`/`version`'s real document builders
//! (`route_discovery`, pure and unit-tested below) rather than falling
//! into the generic echo — these are the routes `kubectl` itself calls
//! first (RESTMapper discovery) before it can even shape a request for an
//! actual resource, so wiring them is what makes the listener minimally
//! useful to a real client rather than just a path-grammar demo. `/api`
//! and `/apis` also negotiate: a client asking for aggregated discovery
//! v2 (`as=APIGroupDiscoveryList;v=v2;g=apidiscovery.k8s.io`, real
//! client-go's own aggregated discovery client) gets that shape instead of
//! the legacy `APIVersions`/`APIGroupList`, via `codec::negotiation`.
//! A discovery-shaped path this build doesn't serve (unknown
//! group/version) gets a real `404` with a minimal `Status` body — not
//! yet upstream's full `Status` type (`reason`/`details` machinery is
//! real hand-written work for a later group), but shaped close enough
//! that `client-go`'s own error-decoding path reads `code`/`reason`/
//! `message` off exactly this JSON today.

use crate::admission;
use crate::aggregator;
use crate::authz;
use crate::flowcontrol;
use crate::proxy;
use crate::config::Config;
use crate::codec::negotiation;
use crate::server::{discovery, healthz, metrics, openapi, path, rest, version};
use crate::storage::client::StorageClient;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};
include!("listener/part-1.rs");
include!("listener/part-2.rs");
include!("listener/part-3.rs");
include!("listener/part-4.rs");
include!("listener/part-5.rs");
include!("listener/part-6.rs");
include!("listener/part-7.rs");
include!("listener/part-8.rs");
include!("listener/part-9.rs");
include!("listener/part-10.rs");
include!("listener/part-11.rs");
include!("listener/part-12.rs");
include!("listener/part-13.rs");
include!("listener/part-14.rs");
include!("listener/part-15.rs");
include!("listener/part-16.rs");
include!("listener/part-17.rs");
include!("listener/part-18.rs");
include!("listener/part-19.rs");
include!("listener/part-20.rs");
include!("listener/part-21.rs");
include!("listener/part-22.rs");
include!("listener/part-23.rs");
include!("listener/part-24.rs");
include!("listener/part-25.rs");
include!("listener/part-26.rs");
include!("listener/part-27.rs");
include!("listener/part-28.rs");
include!("listener/part-29.rs");
include!("listener/part-30.rs");
include!("listener/part-31.rs");
include!("listener/part-32.rs");
include!("listener/part-33.rs");
include!("listener/part-34.rs");
include!("listener/part-35.rs");
include!("listener/part-36.rs");
include!("listener/part-37.rs");
include!("listener/part-38.rs");
include!("listener/part-39.rs");
include!("listener/part-40.rs");
include!("listener/part-41.rs");
include!("listener/part-42.rs");
include!("listener/part-43.rs");
include!("listener/part-44.rs");
