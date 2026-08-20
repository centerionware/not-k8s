# nodeapiserver — design

kube-apiserver's job: the one thing every other component in this project
talks to — REST + watch over every built-in and CRD-defined resource, backed
by `nodestore`. This is the last k3s component. Full research and the
delivery-group rationale live in `docs/APISERVER_PLAN.md` (static — read once,
not maintained); this document is the **live, status-tracked** counterpart,
in `docs/CONTROLLER_MANAGER.md`'s shape: state the end goal precisely, name
the honest engineering problem, group the work, keep each group's status
marker current as it lands. Source of truth for progress is this file, not
the plan file.

Status legend: **not started**, **in progress**, **done** (e2e-verified, per
group where the throwaway rig described below can reach it), **deferred**.

Branching: `nodeapiserver` is a long-lived integration branch (pushed to
`origin/nodeapiserver`). Every group below is its own sub-branch and PR
**into `nodeapiserver`**, not `main` — see `APISERVER_PLAN.md`'s
"Verification" section. `main` takes this work only once, in one PR, once a
cluster boots with **no k3s installed** and the full unfiltered
`test-e2e.sh` suite passes.

## The end goal, stated precisely

Full behavioural parity with real kube-apiserver against this project's own
scope (Groups A-N), plus the four cluster-bootstrap responsibilities k3s
currently absorbs around it (Group O: PKI, RBAC bootstrap policy, the
`kubernetes` default Service, CoreDNS/flannel addon deployment) — because
this plan replaces k3s outright, not just the binary inside it.

**The honest engineering problem, named plainly**: this is not a "watch loop
plus some REST handlers" component like `nodelet`/`nodeproxy`/`nodescheduler`
turned out to be. It is the single largest component in this project by a
wide margin — `APISERVER_PLAN.md`'s measured estimate is 110-130k lines of
Rust, larger than every other component in this repo combined. Two things
make that tractable rather than a rewrite-Kubernetes-in-Rust fantasy:

1. **`nodestore` already is the hard, stateful, correctness-critical half.**
   The full etcd v3 gRPC surface, MVCC revisions, raft replication — all done,
   e2e-gated, independent of this component. `nodeapiserver` is a client of
   it, same as any other etcd-backed apiserver would be.
2. **`k8s-openapi` already is the entire type layer**, and the vendored
   OpenAPI v3 specs (finding 5) already carry the strategic-merge-patch and
   server-side-apply metadata upstream's own generators (`conversion-gen`,
   `defaulter-gen`, struct tags) produce. The actual new engineering is the
   *behavior* those artifacts don't give for free: the handler chain, the
   watch cache, patch/SSA logic, authn/authz/admission, and CEL's missing
   cost-budget/extension-library/type-checking pieces (finding 7).

No component here gets a shortcut on correctness because it's hard — a
resource that "mostly" round-trips through protobuf, or a patch strategy
that's approximately strategic-merge, breaks every existing component that
already depends on real Kubernetes semantics (`nodelet`, `nodescheduler`,
`nodecontroller`'s SSA-based EndpointSlice writes, `nodeproxy`'s EndpointSlice
watch). This must be a real apiserver, not a subset that happens to satisfy
this project's own five other components.

## Getting signal before the cutover

Per `APISERVER_PLAN.md`: a throwaway-rig e2e case, in the shape of
`deploy/lib/test/cases/datastore.sh` (which drives real `grpcurl` against a
throwaway `nodestore`), boots a scratch `nodeapiserver` + `nodestore` pair and
drives it with real `kubectl`/`curl`. This starts returning verdicts as soon
as Groups B, C and E produce one working resource, well before k3s is
actually removed from the deploy path. Track its file name here once it
exists.

## Delivery groups

Ordered by dependency (Group A unblocks everything), not by value. Each
status line is updated as its own PR merges into `nodeapiserver`.

**Phase 0 — prerequisites** — **in progress**. Bump `k8s-openapi` v1_33 →
v1_34 workspace-wide (additive per `APISERVER_PLAN.md` finding 10 — zero
field removals across 572 shared structs). Migrate `crates/nodescheduler`
from `cel-interpreter = "0.10"` to `cel = "0.14"` (the crate was renamed and
reworked — `Val` trait, `Env` overload resolution — so
`dynamic_resources.rs`'s compile/execute path needs real changes, not a
version bump; `dynamic_resources_tests.rs` is the safety net).

**A. Vendoring + build-time codegen** — **done**. `crates/nodeapiserver`
exists; `vendor/refresh.sh` vendored `release-1.34`'s 64 openapi-spec v3
files + 72 `generated.proto` files, recording the ref in `vendor/REF`.
`build.rs` (`build/proto_parse.rs`, `build/openapi_parse.rs`) emits the
protobuf field-number table, the SMP/SSA schema metadata table, and the
discovery GVK map; `src/codegen.rs` builds runtime indexes over them plus
the proto-style<->openapi-style message-name resolver (`resolve_message_ref`)
Group B's codec depends on. Not wired into `deploy/lib/components.sh` or
`notk8s`'s `APPLETS` table yet — correctly Group O's job, per this doc's own
delivery-group split, since there is no listener until Group E.

**B. Wire formats** — **in progress**. `codec::protobuf` is a generic
protobuf encode/decode over `serde_json::Value`, driven entirely by Group
A's field table (`codec::wire` has the raw varint/tag primitives) — no
prost-generated struct universe, per finding 6. Handles all six scalar
types actually present in the vendored set (bool/bytes/double/int32/int64/
string — verified empty for anything else via a codegen-table-driven test,
not just grepped once and assumed), nested messages, repeated (unpacked —
verified spec-correct for proto2, not just simpler), `map<K, V>`, and the
`k8s\x00` + `runtime.Unknown` envelope (`wrap_unknown`/`unwrap_unknown`).
`codec::json`/`codec::yaml` are thin wrappers; `codec::negotiation` parses
`Accept`/`Content-Type` including `kubectl get`'s `as=Table;g=...;v=...`
parameters. Not yet done: `Table` server-side printing itself (only its
negotiation parameters are parsed so far) and `PartialObjectMetadata`.

**C. Storage over nodestore** — **in progress**. `storage::client::StorageClient`
is a real etcd v3 gRPC client to nodestore (`Range`/`Put`/`DeleteRange`/`Txn`/
`Watch`/`Lease*` — `Watch` and `LeaseKeepAlive` both real bidirectional
streams, plus a `prefix_range_end()` helper matching etcd's own client
convention), same mutual-TLS posture as nodestore's own client API,
compiled from a synced copy of nodestore's own vendored protos
(`proto/sync-from-nodestore.sh`) — client-only, not linking the
`nodestore` crate. Method names and the bidi-streaming shape confirmed
directly against `crates/nodestore/src/server/mod.rs`'s own working
implementation of the same generated types, not assumed from the .proto
alone (worth flagging: nodestore's own code reads `LeaseGrantRequest.ttl`/
`.id`, lowercased from the .proto's `TTL`/`ID` — prost snake_cases
regardless of the source proto's own casing convention). `storage::keys`
has the full key layout, override table included:
`/registry/<prefix>/<ns>/<name>` where `<prefix>` is the resource's own
lowercased name unless `(group, resource)` is one of the six real
overrides in `SpecialDefaultResourcePrefixes` — found at
`pkg/kubeapiserver/default_storage_factory_builder.go` via GitHub code
search once `pkg/controlplane/instance.go` (this doc's own earlier,
now-stale guess at the location) turned out not to define it in
`release-1.34` anymore. Genuinely small (six entries: `replicationcontrollers`
-> `controllers`, `endpoints` -> `services/endpoints`, `nodes` -> `minions`,
`services` -> `services/specs`, and `ingresses` -> `ingress` in both
`extensions` and `networking.k8s.io`), vendored as a literal table rather
than approximated. **Not yet landed**: encryption-at-rest providers.

**D. Watch cache** — **in progress**. `cacher::store::WatchCache` is the
cache core: apply/list/watch_from, bookmarks, RV=0 reads, and consistent
reads (`wait_for_revision`, a free function operating on a cloneable
`watch::Receiver` rather than a method on the exclusively-owned cache — a
`&self` method there would conflict with the driver loop's `&mut self`
`apply()`, caught before it shipped rather than after). A watcher whose
`start_revision` has fallen out of the retained history window gets
`Error::TooOld`, the same "relist required" signal real
etcd/kube-apiserver/client-go informers all key off. `cacher::store::SharedCache`
wraps a `WatchCache` in `Arc<std::sync::RwLock<..>>` — the same aliasing
problem `wait_for_revision` hit shows up again for `list()`/`watch_from()`
once a driver loop and reader tasks need concurrent access to one cache,
so this is what real callers actually hold, verified with a genuinely
concurrent `tokio::spawn` reader+writer test, not just a sequential one.
Pure and synchronous underneath, unit-tested against synthetic events
with no live storage needed. `cacher::driver` wires a real `StorageClient`
to it: `list()` seeds the cache from a `Range` snapshot + its header
revision, `watch_from_revision()` opens a `Watch` from `revision + 1` (not
`revision` — avoids redelivering the event the snapshot already
reflects), `apply_watch_response`/`apply_watch_response_shared` decode
`mvccpb::Event`s into `WatchCache::apply`/`SharedCache::apply` calls
(`Added` vs `Modified` distinguished by `kv.version == 1`, matching
`mvccpb::Event`'s own doc comment; an empty-events response with a newer
header revision becomes a `Bookmark`), and `reflect()` is the reconnect
loop — LIST, WATCH, and on any failure or stream end, relist and try
again forever, the same "never give up" posture a real `client-go`
Reflector takes. Decode logic is pure and unit-tested against constructed
`mvccpb`/`etcdserverpb` values; the async orchestration around real
`StorageClient` calls needs live infrastructure to test further.
`reflect()` also generates bookmarks on `bookmark_interval` by explicitly
sending a `WatchProgressRequest` — confirmed by reading
`crates/nodestore/src/server/watch.rs` that nodestore's own server answers
that request on demand but never generates a progress notification
unprompted, so the client side has to ask periodically; `tokio::select!`
between the response stream and the bookmark timer, not a second task.
`cacher::selector` is a faithful port of upstream's label (`labels/selector.go`)
and field (`fields/selector.go`) selector grammars — including `>`/`<`
(numeric-only Gt/Lt), which the label grammar's own doc-comment BNF omits
but the real lexer supports (confirmed against the lexer's token table, not
the possibly-stale comment). Caught and fixed a real parsing bug before it
shipped: a naive fixed-keyword-priority scan (check `" in "` before `"="`)
mis-parses `key=value in here` as a set-based `in` requirement, because
`" in "` occurs inside the *value* after the real `=`; a leftmost-match
scan (longest operator wins ties at the same position) fixes it, with a
regression test locking in exactly that case. Deliberately decoupled from
`WatchCache`'s raw bytes — takes a label map or a field-lookup closure —
so it doesn't have to wait on Group F's object-decoding decisions.
**Not yet landed**: wiring the selector matchers into an actual LIST call
over cached items, which needs Group F's object model to know what a
cached entry's labels/fields even are.

**E. Generic server + handler chain + REST endpoints** — **in progress**.
`server::path` is the REST path grammar — a faithful, line-by-line port of
upstream's own `RequestInfoFactory.NewRequestInfo`
(`staging/src/k8s.io/apiserver/pkg/endpoints/request/requestinfo.go`), not
a reimplementation from a handful of example paths: every branch traces to
a specific line there, and the tests are upstream's own documented example
paths from that function's doc comment. Caught one real thing by tracing
the algorithm rather than assuming: `RequestInfo.Namespace` gets
positionally populated from a `namespaces/{X}/...` path even when the
request turns out to be for the cluster-scoped `Namespace` object itself
(`/api/v1/namespaces/default/status`) — a genuine, harmless upstream quirk
a naive reimplementation would likely have "corrected" away, which would
have been a real behavioral divergence. Full verb set incl.
`deletecollection`, subresources, and field/label selector capture for
`list`/`watch`/`deletecollection` are covered. `server::listener` is a real
hyper + h2 + rustls TLS listener (`server::tls` self-signs a cert on first
start — explicitly **not** the cluster's real PKI, that's Group O's job,
same layering nodelet's own HTTPS server already uses) that proves the
listener/TLS/path-grammar stack works end to end — `nodeapiserver::run()`
now actually binds and serves. **Its request handler is a bring-up stub**,
answering `/healthz` and echoing the parsed `RequestInfo` as JSON, not the
real REST dispatch. `server::discovery` builds the `/api` (`APIVersions`)
and `/apis`/`/apis/{group}` (`APIGroupList`/`APIGroup`) documents from
Group A's discovery GVK table — real shapes confirmed against the
vendored OpenAPI v3 specs, preferred-version selection via
`server::version_compare`'s faithful port of
`CompareKubeAwareVersionStrings`. That port caught a real bug in its own
first draft: a `(major, type, minor)` tuple order would let a higher major
version at a *lower* maturity outrank a lower major version at GA
(`v2alpha1` beating `v1`) — the opposite of upstream's documented
"GA/alpha/beta first, then major and minor," fixed to `(type, major,
minor)` with a regression test locking in the exact cross-case. Discovery
is **group-level only** — the per-version `APIResourceList` needs the
OpenAPI spec's `paths` section, which Group A hasn't vendored/parsed yet,
named honestly rather than implied — and **not yet wired into the
listener's routing**. **Not yet landed**: the handler chain itself, wiring
discovery into real routing, per-version resource discovery, aggregated
discovery v2, `/openapi/v2` + `/openapi/v3`, `/version`. Handler-chain order
(authentication → authorization → priority-and-fairness → admission → REST)
is a hard requirement, not a style choice. The throwaway e2e rig described
above should land as part of this group, not after it.

**F. Scheme: conversion, defaulting, validation** — **not started**. The
largest handwritten chunk. Conversion only needed for genuinely multi-version
groups (admissionregistration, autoscaling, certificates, coordination,
networking, resource, storage, apiserverinternal, storagemigration).
Validation is per-field business logic with no shortcut.

**G. Patch + Server-Side Apply** — **not started**. `json-patch` for RFC
6902/7386; hand-written strategic merge patch; hand-written
structured-merge-diff + `managedFields` (no Rust crate exists), both driven
by Group A's metadata table.

**H. Authentication** — **not started**. x509 client certs, ServiceAccount
JWT issuance/validation, projected/bound tokens, OIDC discovery + JWKS,
TokenReview, bootstrap tokens, anonymous.

**I. Authorization** — **not started**. RBAC, Node authorizer, webhook,
SubjectAccessReview/SelfSubjectAccessReview. PKI primitives (`rcgen`, `p256`,
`x509-parser`, `pem`) are already in-tree from `nodecontroller`'s CSR group.

**J. Admission** — **not started**. Built-in plugin chain
(NamespaceLifecycle, ServiceAccount, DefaultStorageClass, ResourceQuota,
LimitRanger, PodSecurity, DefaultTolerationSeconds, …), mutating/validating
webhooks, ValidatingAdmissionPolicy/MutatingAdmissionPolicy on CEL. **Build
the CEL cost budget before wiring any CEL-driven admission path** — an
unbudgeted CEL evaluator in the request path is a denial-of-service surface.

**K. CRDs (apiextensions)** — **not started**. Dynamic storage
registration, structural schemas, pruning, defaulting,
`x-kubernetes-validations` CEL with type-checking, conversion webhooks,
`established`/`namesAccepted` condition machinery.

**L. Aggregation layer** — **not started**. `APIService` objects,
`ServiceResolver`, reverse proxying, discovery merge, availability
conditions.

**M. APF, audit, observability** — **not started**. FlowSchema/
PriorityLevelConfiguration queueing; policy-driven audit; `/metrics`;
`/healthz`/`/readyz`/`/livez` with per-check verbose output
(`deploy/setup-control-plane.sh` already polls `/readyz?verbose`).

**N. Streaming and proxy subresources** — **not started**. exec/attach/
port-forward/log spliced through to `nodelet:10250`, reusing
`crates/nodelet/src/server/exec.rs`'s raw-upgrade-splice pattern (proven in
production here already — no SPDY/WebSocket crate needed on either end);
node/service/pod proxy subresources.

**O. Cluster bootstrap — the k3s replacement half** — **not started**.
Cluster PKI generation (CA, serving cert, SA signing keypair, per-component
client certs, kubeconfig emission), the ~90 `system:` ClusterRoles/Bindings
from upstream's `bootstrappolicy`, the `kubernetes` default Service +
endpoint reconciler, CoreDNS + flannel manifests moved into `deploy/`. Then
rewrite `deploy/setup-control-plane.sh` to stop installing k3s entirely, plus
the `components.sh` row + `notk8s` applet (`components.sh:6` and
`deploy/measure.sh:98` already name `nodeapiserver` in anticipation).

## Final acceptance

A cluster bootstrapped by `./deploy/bootstrap-source.sh --with-cri` with **no
k3s installed at all**, passing the full unfiltered `test-e2e.sh` suite
(~142 tests) including the real CSI and DRA reference drivers from
`deploy/lib/e2e-full-setup.sh`. Only then does `nodeapiserver` merge to
`main`, per `CLAUDE.md`'s merge protocol and the "no partial multi-phase
work" standing rule — satisfied here by merging into this integration branch
group-by-group and reserving `main` for the completed arc.
