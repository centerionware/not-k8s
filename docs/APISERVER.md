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
protobuf field-number table, the SMP/SSA schema metadata table (now
including `ref_schema` — the `$ref`'d schema a field's value is shaped
like, added for Group G's Strategic Merge Patch recursion), and the
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
parameters. `codec::table::convert_to_table` lands the generic default
`Table` converter — a faithful port of real upstream's own
`defaultTableConvertor` (`k8s.io/apiserver/pkg/registry/rest/table.go`,
fetched and read directly): exactly two columns (`Name`, `Created At`),
one row per item for a List-shaped input, `ResourceVersion`/`Continue`/
`RemainingItemCount` carried through from the List's own metadata. Named
honestly: real kube-apiserver's much larger *per-type* printer set
(Pod's `READY`/`STATUS`/`RESTARTS` columns, computed from container
statuses — hand-written Go, `pkg/printers/internalversion`) isn't
started — every resource this build serves gets the generic table today,
same as a fresh CRD does in real kube-apiserver until it earns its own
printer. Not yet done: any per-type printer, and `PartialObjectMetadata`.

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
than approximated. **Encryption-at-rest transform primitives now exist**
(`storage::encryption`): `Identity` and AES-256-GCM providers, plus the
generic `PrefixTransformers` composition every provider list (including
per-key rotation) uses — a faithful port of upstream's own
`storage/value` package (`transformer.go`'s `prefixTransformers`,
`encrypt/identity/identity.go`, `encrypt/aes/aes.go`'s `gcm` type), fetched
and read directly. Real envelope format confirmed against upstream:
`k8s:enc:aesgcm:v1:<key-name>:<nonce><ciphertext+tag>`. Named honestly:
AES-CBC, secretbox, and KMS (v1/v2) are real upstream providers this
module doesn't build (no CBC/secretbox crate in the dependency tree, no
KMS gRPC plugin protocol vendored — `ring`, used for AES-GCM, is not a
*new* dependency, already pulled in transitively by `rustls`). **Not yet
landed**: wiring any of this into `StorageClient`'s actual read/write
path, and `EncryptionConfiguration` YAML parsing (which provider(s) apply
to which resource) — this module is the transform primitives a config
loader would wire up, not the loader itself.

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
The selector matchers are now wired onto a decoded object:
`cacher::selector::object_labels`/`field_value`/`object_matches` adapt
`matches_labels`/`matches_fields` onto a `serde_json::Value` (Group F's
decoded-object shape). The two halves are deliberately asymmetric, named
honestly: `object_labels` is genuinely generic — every Kind's labels live
at `metadata.labels`, `ObjectMeta` being shared structurally — but
`field_value` is only a generic dotted-JSON-path fallback
(`"spec.nodeName"` -> pointer `/spec/nodeName`), a strict superset of what
real upstream allows; real kube-apiserver restricts which fields are even
selectable per Kind via a hand-written `SelectableFields` function per
type (`pkg/registry/*/*/strategy.go`), which isn't built here yet.
`object_matches` is now called for real, from `server::rest::list`
(Group E), filtering every item a real `LIST` request decodes.

`WatchCache`/`SharedCache` also gained a single-key `get()` (`list()`'s
own equivalent for a `GET` rather than a `LIST`, an `O(log n)` lookup
into the same `BTreeMap` `list()` already iterates) — a real gap named
here until now, since a cache with no way to answer "does this one key
exist" couldn't back a real `GET` at all.

`cacher::registry::CacheRegistry` is the last Group D piece: a
single-resource cache-registration primitive —
`CacheRegistry::spawn(storage, group, version, resource)` starts a
background `driver::reflect()` loop for one resource and hands back the
`SharedCache` it keeps live, `get()` looks one up by `(group, version,
resource)`. **Pure primitive only, not yet the full picture**: it does
not enumerate every resource this build knows about and start one for
each at boot (spawning on the order of 90 concurrent, long-running
reconnect loops against nodestore at process startup is a real
resource/ordering decision this crate hasn't made yet, not an
oversight), and nothing reads from a registered cache yet either —
`server::rest::get`/`list` still read straight from nodestore on every
call, not a registered cache (a real, valid strategy for now per
`rest`'s own doc comment, not a shortcut). **Not yet landed**: the
per-Kind `SelectableFields` allowlist, registering a cache for every
resource at boot, and wiring `rest`'s read verbs to consult a
registered cache.

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
minor)` with a regression test locking in the exact cross-case.
`server::discovery::api_resource_list(group, version)` closes the
per-version gap this doc used to name here: a new Group A parser
(`build/discovery_parse.rs`) reads every vendored spec's own `paths`
section — each verb block's `x-kubernetes-action` + its GVK extension —
grouped by `(group, version, resource)` with verbs and namespaced-ness
unioned across every path that resource appears on, with `"watch"`
synthesized whenever `"list"` is present — real bug caught by CI, not
assumed correct: the modern GET-collection route's own action is `"list"`,
never `"watch"` literally (that label only appears on the deprecated
`/watch/`-prefixed legacy route family this parser skips), but every real
REST storage supporting list also supports watching that same route.
Verbs and namespaced-ness are also unioned across every path a resource
appears on: a namespaced resource's own `/namespaces/{namespace}/...`
path and its "list across all namespaces" sibling both contribute to one
entry, not two. Correctly
tells `/api/v1/namespaces` (list `Namespace` objects) apart from
`/api/v1/namespaces/{namespace}/pods` (list `Pod`s in a namespace) by the
real path-parameter name the vendored spec uses (`{name}` vs.
`{namespace}`), not by string-matching "namespaces" as a special case.
Named, deliberate skips (the parser's own doc comment): subresources
(`pods/status`, `pods/log`, ...) and the deprecated `/watch/`-prefixed
path family. `singularName` uses real kube-apiserver's own RESTMapper
default (lowercased kind) since no per-type override table is vendored;
`shortNames`/`categories` aren't emitted at all (not present anywhere in
the vendored spec). **Now wired into the listener's actual routing**:
`server::listener`'s `route_discovery` (pure, unit-tested apart from the
async handler) dispatches all five non-resource discovery routes (`/api`,
`/api/{version}`, `/apis`, `/apis/{group}`, `/apis/{group}/{version}`) to
`server::discovery`'s real builders, with a genuine `404` (a minimal
`Status` body — `kind`/`apiVersion`/`status`/`message`/`reason`/`code`,
the exact subset `client-go`'s own `errors.NewNotFound` decoding path
reads, not the full `Status` type's `details.causes` machinery) for an
unknown group/version rather than a silent fallthrough into the
resource-request echo stub. A resource-shaped path (`/api/v1/namespaces/
default/pods`) still falls through to that stub, unchanged.
`/openapi/v3` and `/openapi/v3/<path>` are also now real, wired the same
way: a new Group A build-time step (`build/openapi_serve.rs`) embeds
every `vendor/openapi-spec/v3/*.json` file verbatim, keyed by the real
HTTP path upstream's own vendored filenames mechanically encode
(`apis__apps__v1_openapi.json` -> `apis/apps/v1`, confirmed directly from
the filenames, not guessed) — `server::openapi::root()` builds the
`{"paths": {...}}` discovery index real client-go's `openapi3` package
expects (`serverRelativeURL` + a `?hash=` cache-busting token, this
build's own content hash, not a reproduction of upstream's internal hash
algorithm — nothing in the protocol requires the two to match), and
`server::openapi::doc(path)` serves a document's bytes completely
unmodified — this crate has no OpenAPI v3 generator of its own to
diverge from what upstream actually published for the vendored ref.
`/version` is also real now (`server::version`): a `version.Info` document
(shape confirmed against upstream's own `apimachinery/pkg/version/types.go`)
built from real build-time facts — `major`/`minor` parsed from
`vendor/REF`, `gitCommit`/`gitTreeState`/`buildDate` captured by a new
`build.rs` step (`git`/`date`, each degrading to `"unknown"` rather than
failing the build if unavailable). `gitVersion` follows the
`vX.Y.Z+<suffix>` convention real distros use for a non-stock control
plane (K3s's own `v1.28.3+k3s1` is the precedent) — `+notk8s` here.
`goVersion`/`compiler` are inherently Go-specific fields with no faithful
Rust equivalent — named honestly in the module's own doc comment: this
build reports its actual `rustc` toolchain there instead of fabricating a
Go version.

Aggregated discovery v2 (`apidiscovery.k8s.io/v2`'s `APIGroupDiscoveryList`
— real shape confirmed against upstream's own
`staging/src/k8s.io/api/apidiscovery/v2/types.go`) is now real and
reachable over HTTP: `discovery::api_group_discovery_list`/
`api_v1_group_discovery_list` build it, fully data-driven from the same
Group A tables as the legacy shape, and `codec::negotiation`'s `as=`
handling was generalized from a `Table`-only boolean into a
client-requested *kind* (`Accepted::as_kind`/`as_group`/`as_version`,
with `wants_table()` as the old boolean's replacement) so
`listener::route_discovery` can pick this form over the legacy
`APIVersions`/`APIGroupList` at `/api`/`/apis` whenever a client's
`Accept` header asks for `as=APIGroupDiscoveryList;v=v2;
g=apidiscovery.k8s.io` — an exact `v2` match only, not `v2beta1` (the
pre-GA shape this crate doesn't separately model), so a client asking for
a shape this build doesn't actually build falls back to the legacy form
rather than silently getting served a possibly-wrong one.

The first real resource verbs now exist too: `server::rest::get`/`list`
— generic single-object `GET` and whole-resource `LIST` — resolve the
resource's `Kind` from Group A's discovery table (`resolve_kind`, pure,
unit-tested), build the storage key (`storage::keys::object_key`/
`list_prefix` + `prefix_range_end`), do a real `RangeRequest` against
nodestore via `StorageClient`, and decode the stored `runtime.Unknown`
envelope(s) back to JSON via `codec::protobuf` (`decode_stored_object`,
resolving the schema from the envelope's own `apiVersion`/`kind` — what
was actually written — not the request path, verified with a real
encode-then-decode round trip). `list` wraps its items in the real
`<Kind>List` shape (`PodList`, `DeploymentList`, ... — verified against
the vendored spec, not assumed: every List type is named exactly that,
never a separate hand-assigned name), `resourceVersion` from the
`Range`'s own header revision. Both generic over every resource this
build knows about, no per-type Go code, same posture every other Group
B/C/E slice has taken. `server::listener::run` connects a `StorageClient`
at startup (best-effort — a nodestore unreachable at boot degrades to
`None`, falling back to the bring-up echo stub rather than stopping the
listener from serving discovery, which needs no storage at all) and
clones it per connection (`StorageClient` wraps a cheap-to-clone
`tonic::transport::Channel`, same posture `cacher`'s own driver takes).
Named honestly, not overclaimed: reads go straight to nodestore,
bypassing `cacher::store::WatchCache` entirely (a real, valid strategy —
upstream's own quorum-read path takes exactly this shape — not a
stand-in for the cache; the cache isn't even started yet, since nothing
in `lib.rs::run()` calls `cacher::driver::reflect()`), no subresources.
`list` now filters by label/field selector for real —
`cacher::selector::object_matches` (Group D's own generic adapter,
already landed and unit-tested there) wired in unchanged, with a real
`400 BadRequest` (not a `500`) for a client-malformed selector. `list`'s
remaining gap is pagination (`continue`/`limit`).

`server::rest::create` (`POST` to a resource's collection URL) is real
too: runs Group F's already-landed `scheme::validation::validate_required`/
`validate_types` on the client's raw submitted body (required-ness is
about what the user *sent*, matching those functions' own documented
ordering), then `scheme::defaulting::apply_defaults`, sets real
`metadata.creationTimestamp`/`uid` (`uuid`, real RFC3339 via `chrono` —
both already-resolved dependencies made directly usable, not new ones),
and writes with a real create-only-if-absent `Txn`
(`Compare(ModRevision(key), Equal, 0)` — confirmed directly against
`nodestore`'s own server-side comment naming this the idiom, not
assumed) rather than a plain `Put` that could silently clobber an
existing object. Request bodies are decoded generically by negotiated
`Content-Type` (JSON/YAML — a protobuf request body would need the
target schema to decode, which needs the resource resolved first;
named honestly as a real, separate gap, not guessed at). Real,
distinct `Status` responses per outcome: `201` created, `409
AlreadyExists` (lost the create race), `422 Invalid` (validation
failures, joined into one message — real upstream's structured
`details.causes` isn't built), `400` for a missing name (no
`generateName` support) or a namespace mismatch between the body and
the URL.

`server::rest::delete` (single-object `DELETE`) is real too: one
`DeleteRange` with `prev_kv: true` so the deleted object can be returned
in the response, matching real upstream's own synchronous-delete shape.
Named honestly as the bring-up floor, not the real thing: no
`resourceVersion`/`uid` precondition checking
(`metav1.DeleteOptions.Preconditions`), no `propagationPolicy`
(Foreground/Background/Orphan — no owner-reference garbage collector
exists to orphan from or cascade through in the first place), no
finalizer handling — an unconditional delete-if-present.

`server::rest::update` (`PUT`) is real optimistic concurrency, not a
blind overwrite: reads the current object first, requires the
submitted body's own `metadata.resourceVersion` to match what's
actually stored (compared numerically against the current MVCC
revision, not as opaque strings) — a real `409 Conflict` on a mismatch,
a real `400` if the client omitted it entirely (real upstream requires
`resourceVersion` for `PUT`) — then writes with a `Txn` compared
against that same revision, so a concurrent write between the read and
this write also loses the race rather than being silently overwritten.
`metadata.creationTimestamp`/`uid` are always preserved from the
existing object regardless of what the client submitted (both
immutable after creation, matching real upstream). No create-on-update
(`AllowCreateOnUpdate`, real upstream's own opt-in a handful of types
use, isn't modeled — a `PUT` targeting a name that doesn't exist is a
real `404`, not a create).

**Authentication and authorization now gate all five real verbs**:
`authn::x509`'s verified peer identity (Group H) plus opt-in RBAC
enforcement (Group I, `NODEAPISERVER_ENFORCE_RBAC`) — see those groups'
own sections for what's real and what's deliberately still opt-in.
Admission (Group J) doesn't exist at all yet — no plugin gets a say in
any of this.

`server::watch_event::to_watch_event_json` is the first piece of real
`WATCH` support: converts one `cacher::store::WatchEvent` into the real
`metav1.WatchEvent` wire shape (`staging/src/k8s.io/apimachinery/pkg/apis/meta/v1/watch.go`,
fetched and read directly) — `{"type": "ADDED"|"MODIFIED"|"DELETED"|
"BOOKMARK", "object": {...}}`, `Added`/`Modified` decoding the real
stored object, `Bookmark` carrying just `kind`/`apiVersion`/
`resourceVersion` (matching real upstream — a bookmark object has no
other fields populated). **Pure conversion only — not yet wired into an
actual streaming HTTP response**: `server::listener` has no long-lived
chunked-response machinery yet, and nothing starts a
`cacher::registry::CacheRegistry` to read events from in the first
place. **Named, honest gap**: a `Deleted` event with no retained value
(which is every `Deleted` event today — `cacher::store::WatchEvent`'s
own doc comment says `value` is empty for `Deleted`) converts to `None`
rather than a fabricated placeholder object — real upstream's own
`WatchEvent.Object` doc comment requires "the state of the object
immediately before deletion," which this cache doesn't currently keep;
fixing that for real needs `WatchCache` itself to start retaining the
last value on delete, separate, not-yet-started work.

**Not yet landed**: `watch` end to end (the conversion above, streaming
HTTP responses, and starting `CacheRegistry` all need to come together),
`patch`/`deletecollection`, admission (Group J), the real handler chain
itself (authn -> authz -> APF -> admission -> REST — a hard requirement
on order, not a style choice, once it fully exists), `/openapi/v2`.
The throwaway e2e rig described above should land as part of this group,
not after it.

**F. Scheme: conversion, defaulting, validation** — **in progress**. The
largest handwritten chunk. `scheme::defaulting::apply_defaults(schema, value)`
lands the first slice: recursively fills a JSON object's absent fields from
Group A's `FIELD_META` table, now carrying each property's real vendored
`"default"` value (`default_json`, added alongside this module) —
unconditional defaults only (`ContainerPort.protocol` -> `"TCP"` is the
verified concrete case), matching real upstream defaulting exactly wherever
a field's default doesn't depend on another field's value or the request's
`apiVersion`; upstream's genuinely conditional defaults
(`pkg/apis/*/v1/defaults.go`'s hand-written Go) are out of scope for this
mechanism and stay separate, per-type work, named honestly in the module's
own doc comment rather than silently only-partially-implemented. An absent
object-typed field first materializes from its own structural default
(usually `{}`, which is what tells the recursion to keep going into that
schema's own fields) via `ref_schema`, then gets that schema's own defaults
applied — proven with a genuinely two-levels-deep case
(`Container.ports[].protocol`) so the cascade isn't just asserted at one
level. `scheme::validation::validate_required(schema, value)` lands the
first validation slice: recursively checks every field a schema's own
vendored `required` array names is present and non-null, returning one
`MissingField{path}` per violation with a real dotted/indexed path
(`"containers[1].name"`) — reads a new Group A table (`REQUIRED_FIELDS`, a
schema-level array flattened to (schema, field) pairs, kept separate from
`FIELD_META` since `required` has no per-property JSON node to hang off,
unlike `x-kubernetes-*`/`default`). Recurses the same way defaulting does,
via `ref_schema`. Deliberately run *before* defaulting in a real
create/update path — a field is required in the user's *input*, not
required to survive defaulting. `scheme::validation::validate_types(schema,
value)` lands the second: checks every field that *is* present has the
JSON kind (`string`/`boolean`/`number`/`integer`/`array`) the schema's own
`type` declares, reading another new Group A table (`TYPE_INFO` — kept
separate from `FIELD_META` for the same "different question, different
selectivity" reason `REQUIRED_FIELDS` is: nearly every scalar/array field
carries a `"type"`, unlike `FIELD_META`'s narrower x-kubernetes-*/default/
ref scope; a field with no `"type"` key at all, i.e. a nested single-object
field spelled via `allOf`, has no entry and is left to `ref_schema`
recursion instead, verified against `PodSpec.securityContext`). Recurses
the same way, and a whole number encoded as a JSON float (`30.0`) still
counts as `integer` — JSON has no separate integer literal syntax, so the
check is on the value's mathematical shape, not how a particular encoder
happened to lex it. Named honestly: both functions together are still
only structural (presence + kind), not the rest of real validation
(formats, enums — verified absent from the vendored specs entirely —
cross-field consistency, numeric ranges — all hand-written Go upstream, no
shortcut). Conversion only needed for genuinely multi-version groups
(admissionregistration, autoscaling, certificates, coordination,
networking, resource, storage, apiserverinternal, storagemigration) —
**not yet landed**.

**G. Patch + Server-Side Apply** — **in progress**. `patch::json_patch`/
`patch::merge_patch` wrap the `json-patch` crate for RFC 6902/7386 —
rollback-on-failure for JSON Patch (not the unsafe partial-apply variant),
recursive-merge/null-deletes-key semantics verified for JSON Merge Patch.
`patch::strategic_merge` is the hand-written k8s-specific patch kind, now
real: null deletes a key, object fields merge recursively using *their
own* `ref_schema` (not the parent's — the reason `ref_schema` was added
to Group A's codegen rather than inferred), and `patch_strategy: merge`
list fields merge by `patch_merge_key`, matched and updated in place with
non-matching elements appended, verified against the concrete
`PodSpec.containers` sample finding 5 names directly (merge-key `name`)
including a two-levels-deep recursion case
(`containers[].resources.limits`) to prove `ref_schema` resolution chains
correctly, not just parent -> immediate child. Named, deliberate gaps
(`strategic_merge`'s own doc comment): no `$patch`/`$setElementOrder`/
`$deleteFromPrimitiveList` directives — a patch that never uses them
(the overwhelming majority) behaves identically either way. **Not yet
landed**: Server-Side Apply/`managedFields` (structured-merge-diff has no
Rust crate to reuse), which will build on the same `FIELD_META`
(`ref_schema` included) this group's patch logic already reads.

**H. Authentication** — **started**. `authn::x509::identity_from_der`
derives an `Identity{name, groups, credential_id}` from a client
certificate's Subject — Common Name as username, every Organization
value as a group, real upstream's own generic x509 authenticator
convention (`authentication/request/x509/x509.go`'s
`CommonNameUserConversion`, fetched and read directly), including the
real `credential_id` extra entry (`X509SHA256=<hex sha256 of the leaf
cert's DER>`, upstream's own `user.CredentialIDKey`). Mirrors
`crates/nodelet/src/server/tls.rs::client_identity_from_der`'s
already-proven pattern in this workspace. **Now wired into the listener
for real**: `NODEAPISERVER_CLIENT_CA_FILE` (optional —
`config::Config::client_ca_file`) turns on client certificate
verification at the TLS layer (`server::tls::load_client_ca` +
`with_client_cert_verifier`, offered but not required, same posture
`nodelet`'s own `load_client_ca` already established), and the resulting
verified peer certificate is turned into an `Identity` and threaded
through to `server::listener::handle`, surfaced in the bring-up echo
response's own `user` field for real observability. **Authentication
without authorization**: nothing yet checks this identity before serving
a request — there is no authorization (Group I) to enforce it against,
so every request is still served the same way regardless of who (if
anyone) it authenticated as. Everything else named above (ServiceAccount
JWT, OIDC, TokenReview, bootstrap tokens, anonymous) is not started.

**I. Authorization** — **started**. `authz::rbac` is the RBAC
rule-matching primitive — a faithful port of real upstream's own
`VerbMatches`/`APIGroupMatches`/`ResourceMatches`/`ResourceNameMatches`/
`NonResourceURLMatches` (`pkg/apis/rbac/v1/evaluation_helpers.go`) and
`RuleAllows`/`RulesAllow` (`plugin/pkg/auth/authorizer/rbac/rbac.go`),
fetched and read directly. Covers the real wildcard semantics
(`verbs`/`apiGroups`/`resources` `"*"`, the `*/status`-style subresource
wildcard, the trailing-`*` prefix wildcard `nonResourceURLs` supports,
and empty `resourceNames` meaning "every name") and the resource vs.
non-resource request split. **Pure evaluation engine only — not yet
wired to real `Role`/`RoleBinding`/`ClusterRole`/`ClusterRoleBinding`
objects**: resolving which `PolicyRule`s apply to a subject in a
namespace needs those objects fetched from storage (real upstream's
`DefaultRuleResolver`) — separate, not-yet-started work, same "land the
primitive, wire it later" split this arc has taken throughout.
`authz::subject` is the other half `DefaultRuleResolver` combines with
rule matching: does a binding's `Subjects` list include a given
authenticated user (`pkg/registry/rbac/validation/rule.go`'s
`appliesTo`/`appliesToUser`, fetched and read directly), including the
real `ServiceAccount` `system:serviceaccount:<namespace>:<name>`
username convention (`MakeUsername`/`MatchesUsername`) and the real
namespace-defaulting rule (an unqualified `ServiceAccount` subject
defaults to its binding's own namespace — meaningless on a
`ClusterRoleBinding`, which has none, so such a subject must name one
explicitly there). `authz::resolve::rules_for(storage, user_name,
user_groups, namespace)` is the storage-backed half that closes the
loop: lists real `ClusterRoleBinding`/`RoleBinding` objects (via
`server::rest::list`, no per-type Go code, the same machinery a real
request handler uses), keeps the ones whose subjects apply, and resolves
each one's `RoleRef` to a real `Role`/`ClusterRole`'s rules (via
`server::rest::get`) — real upstream's own `DefaultRuleResolver`
(`VisitRulesFor`), ported, with the same non-fatal-per-binding-error,
purely-additive posture. **Now wired into `server::listener`, opt-in**:
`handle` calls `resolve::rules_for` + `rbac::rules_allow` to gate
`GET`/`LIST` with a real `403` on denial, gated behind
`NODEAPISERVER_ENFORCE_RBAC` (`config::Config::enforce_rbac`), **off by
default** — enabling deny-by-default RBAC before Group O's bootstrap
`ClusterRole`/`ClusterRoleBinding` set exists (the ~90 `system:` roles
named below) can lock every request out with no path back in, so this
stays opt-in until that bootstrap data exists. A request with no
established x509 identity is evaluated as the real anonymous user/group
upstream itself uses (`system:anonymous`/`system:unauthenticated`), not
silently exempted. Node authorizer, webhook authorization, and
SubjectAccessReview/SelfSubjectAccessReview are not started. PKI
primitives (`rcgen`, `p256`, `x509-parser`, `pem`) are already in-tree
from `nodecontroller`'s CSR group.

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
