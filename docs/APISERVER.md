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
etcd/kube-apiserver/client-go informers all key off. `apply()`'s own
`Deleted` handling retains the key's last-known value before removing it
from `items` — real upstream's own `WatchEvent.Object` semantics for a
delete ("the state of the object immediately before deletion"), closing
a gap this doc used to name in Group E's `to_watch_event_json` section:
the caller (`cacher::driver`) always passes an empty value for a
`Deleted` event (nodestore's own watch stream is never asked for
`prev_kv`), so the cache — the only place that still has the pre-delete
value — is what has to remember it. `cacher::store::SharedCache`
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
oversight). `server::rest::get` now *can* consult a `SharedCache` if
handed one (`cache.get(key)`, a hit skips nodestore entirely; a miss
always falls through to a real `Range` rather than trusting the cache's
absence of an entry to mean "not found," since a not-yet-synced or
never-registered cache is indistinguishable from a genuinely empty one
using only what `SharedCache` exposes today — a pure latency win on the
hit path, never a correctness risk on the miss path). `server::listener::run`
now proves this end to end for one real resource: it spawns a
`CacheRegistry` cache for `namespaces` (core group — the same resource
Group F's first verified name-format rule already targets) and `GET`
consults it whenever a request actually targets `namespaces`; every
other resource's `GET`/`LIST`, and every `create`/`update`/`delete`
regardless of resource, still reads/writes straight to nodestore. **One
concrete case, not a general policy** at the time — enumerating *every*
resource this build knows about and starting a cache for each at boot is
still a real, separate, not-yet-made decision (how many at once, in what
order, whether to wait for sync before serving traffic).

**Follow-up**: `server::listener::run` now spawns a bounded, reasoned
list of resources instead of just `namespaces` —
`BOOT_CACHED_RESOURCES` (`namespaces`, `pods`, `services`, `secrets`,
`configmaps`, `endpoints`, `nodes`: the core-group resources a real
cluster's own kubelets/kube-proxy/controllers read most heavily), and
`GET`/`LIST` consult one whenever a request targets any resource in that
list (`cache_registry.get(group, version, resource)` replaced the
`namespaces`-only special case). This is still a deliberately bounded
subset, not the general "every resource at boot" policy — that remains
a real, separate, not-yet-made decision. **Not yet landed**: the
per-Kind `SelectableFields` allowlist, and registering a cache for every
resource at boot.

**Follow-up**: `server::rest::list` now also consults a cache when
handed one, closing the gap the paragraph above used to name — but it
can't reuse `get`'s "a miss always falls through" trick, since an empty
`list()` result is itself a fully valid `200` answer, not a signal to
fall through the way a `get` miss is. So `SharedCache` gained a real
`has_synced()` flag (real `client-go` `HasSynced()`, ported): a fresh
cache starts unsynced, and the first completed `replace()` — the
reconnect loop's own first `LIST` — marks it permanently synced, never
un-set by a later relist. This is a genuine flag, not "`revision() > 0`":
a resource with zero live objects still completes a real `LIST` at
revision `0` (an empty store has never advanced its revision), which
would be indistinguishable from "not synced yet" if synced-ness were
inferred from the revision alone. `list` checks `has_synced()` before
trusting the cache; an unsynced cache falls through to nodestore exactly
as `cache: None` would. `server::listener` passes the same
`namespaces` proof-of-concept cache to `list` that it already passed to
`get` — no new resource gained caching from this change, just closed
the `get`/`list` asymmetry for the one resource that already had one.

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

**Authentication, authorization, and admission now all gate the real
write verbs**: `authn::x509`'s verified peer identity (Group H), opt-in
RBAC enforcement (Group I, `NODEAPISERVER_ENFORCE_RBAC`), and five
unconditional Group J admission plugins — see those groups' own sections
for what's real and what's deliberately still opt-in/not-yet-ported.

`server::watch_event::to_watch_event_json` is the first piece of real
`WATCH` support: converts one `cacher::store::WatchEvent` into the real
`metav1.WatchEvent` wire shape (`staging/src/k8s.io/apimachinery/pkg/apis/meta/v1/watch.go`,
fetched and read directly) — `{"type": "ADDED"|"MODIFIED"|"DELETED"|
"BOOKMARK", "object": {...}}`, `Added`/`Modified` decoding the real
stored object, `Bookmark` carrying just `kind`/`apiVersion`/
`resourceVersion` (matching real upstream — a bookmark object has no
other fields populated), `Deleted` carrying the real last-known object
state (real upstream's own `WatchEvent.Object` doc comment: "the state
of the object immediately before deletion" — `cacher::store::WatchCache`
now retains exactly that on delete, closing what was previously named
here as a real gap; see that module's own doc comment).

**Milestone: `WATCH` is real end to end**, for any resource in
`server::listener`'s own `BOOT_CACHED_RESOURCES`: a `GET
.../pods?watch=true` (or the `/api/v1/watch/pods` legacy path form) now
gets a genuine streaming response — `cache.watch_from(resourceVersion)`'s
own retained-history replay first, then every live event as it happens,
each encoded by `to_watch_event_json` and framed as a newline-terminated
JSON document (`watch_response_body`, an `http_body_util::StreamBody`
over a `tokio_stream` combining the replay `Vec` with a
`BroadcastStream` of the cache's live event channel). No `Transfer-Encoding`
header is set explicitly — hyper's own h1/h2 connection layer already
frames a body with no known length correctly for whichever protocol was
negotiated (chunked for h1, native framing for h2, where
`Transfer-Encoding` is actually forbidden by the HTTP/2 spec, so setting
it by hand would have been wrong for an h2 connection). A
`resourceVersion` older than the cache's retained history window gets a
real `410 Gone` (`errors.NewResourceExpired`'s own shape — the signal
every real `client-go` informer relists on) rather than silently serving
a gap. A resource outside `BOOT_CACHED_RESOURCES` (no registered cache)
falls through to the bring-up echo stub, same posture as `GET`/`LIST`
already had for an uncached resource. **`WATCH` is now RBAC-gated too**
(`enforce_rbac`, resolved against a fresh cheap `storage.clone()` since
`watch` otherwise needs no storage connection at all) — fails closed
(`500`) if enforcement is on but no storage connection exists to resolve
rules against, rather than silently degrading to "allow." Group J
admission deliberately does **not** gate `watch` — matching real
upstream's own posture (admission never runs on a read, whatever the
verb), not a gap.

**`PATCH` is real too now** (`rest::patch`, reusing Group G's already-landed
`patch::json_patch`/`merge_patch`/`strategic_merge`): the real `Content-Type`
selects the patch kind (`application/json-patch+json`/
`application/merge-patch+json`/`application/strategic-merge-patch+json` —
`rest::patch_kind_for_content_type`, a real `415` for anything else,
Server-Side Apply's own `application/apply-patch+yaml` deliberately not
recognized, matching Group G's own "not yet landed" note), applied to
the object this same call itself reads, then persisted through the same
optimistic-concurrency `Txn`-compared-against-`ModRevision` tail
`rest::update` already used (factored out as `persist_update`) — no
client-submitted `resourceVersion` needed, unlike `PUT`, since the
object being patched *is* the one just read. **Named, honest gap**:
`PATCH` doesn't run through Group J admission yet — the mutating/
validating plugin chain in `server::listener` is wired specifically
against a pre-built `body_value` the way `CREATE`/`UPDATE` supply it,
and `PATCH`'s own final object only exists once `rest::patch` has
already applied the patch and persisted it, past the point admission
would need to run to still be able to reject the write; closing this
needs `rest::patch` itself split into an apply-then-validate-then-persist
shape the way `create`/`update` already are.

**Not yet landed**: `deletecollection`, the rest of admission
(Group J's own section has the
running plugin list), the real handler chain fully unified into one
ordered dispatcher (authn -> authz -> APF -> admission -> REST — a hard
requirement on order, not a style choice, once it fully exists; today
each piece is wired in ad hoc, in the right relative order, not through
one shared pipeline), `/openapi/v2`. The throwaway e2e rig described
above should land as part of this group, not after it.

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
shortcut).

The first format checks now exist: `scheme::name_format::is_dns1123_label`/
`is_dns1123_subdomain`/`is_dns1035_label` — faithful character-class
ports of real upstream's own regex-based name validators
(`apimachinery/pkg/util/validation/validation.go`'s `IsDNS1123Label`/
`IsDNS1123Subdomain`/`IsDNS1035Label`, fetched and read directly; this
crate has no regex dependency, and each pattern is simple enough to
check in one pass without one). Real upstream itself keeps "which
validator applies to which resource" as hand-maintained-per-type Go
(`ValidateNamespaceName = NameIsDNSLabel`, `ValidateServiceAccountName
= NameIsDNSSubdomain`, confirmed directly — there is no vendored table
for this, the same "verified genuinely absent" finding `validate_types`
already recorded for enum constraints), so `server::rest`'s own
`name_format_violations` wires in only the resources this crate has
actually verified a real rule for, each confirmed directly against
`pkg/apis/core/validation/validation.go` (release-1.34) and cross-checked
against the vendored `api__v1_openapi.json` `paths` table to confirm it's
really core-group: `namespaces` -> `is_dns1123_label`
(`ValidateNamespaceName = NameIsDNSLabel`); `serviceaccounts`, `pods`,
`replicationcontrollers`, `nodes`, `limitranges`, `resourcequotas`,
`secrets`, `endpoints`, `persistentvolumes`, `configmaps` ->
`is_dns1123_subdomain` (each a literal `var Validate<Kind>Name =
apimachineryvalidation.NameIsDNSSubdomain`); `services` ->
`is_dns1035_label` (`ValidateServiceName = NameIsDNS1035Label`, real
upstream's own default — a `RelaxedServiceNameValidation` alpha feature
gate can relax this to `NameIsDNSLabel`, but this crate has no
feature-gate machinery so it always applies the gate's default-off
behavior). Four more non-core resources are now wired too, each
group-verified against the vendored spec's own `paths` table and
cross-checked against the real per-type `Validate<Kind>` function that
applies the rule to that type's own `ObjectMeta` (not merely a
same-named var used elsewhere for a referenced-field check —
`ValidateClassName`, for one, is also used to validate the
`storageClassName` field referenced from PV/PVC, a different check
entirely from validating a `StorageClass` object's own name):
`scheduling.k8s.io/priorityclasses` -> `is_dns1123_subdomain`
(`ValidatePriorityClass`, `pkg/apis/scheduling/validation/validation.go`
— real upstream also forbids a `system-`-prefixed name unless it's one
of a fixed predefined set, NOT ported here, only the DNS-subdomain
shape); `resource.k8s.io/resourceclaims` and
`resource.k8s.io/resourceclaimtemplates` -> `is_dns1123_subdomain`
(`ValidateResourceClaim`/`ValidateResourceClaimTemplate`,
`pkg/apis/resource/validation/validation.go`); `storage.k8s.io/storageclasses`
-> `is_dns1123_subdomain` (`ValidateStorageClass`,
`pkg/apis/storage/validation/validation.go`). Twelve more resources
across six more non-core groups landed the same way (real
`Validate<Kind>[Create]` function confirmed to apply the var to that
type's own `ObjectMeta`, real group confirmed against that group's own
vendored spec `paths` table): `apps/v1`'s `controllerrevisions`,
`daemonsets`, `deployments`, `replicasets`
(`pkg/apis/apps/validation/validation.go`); `networking.k8s.io/v1`'s
`ingresses`, `ingressclasses`, `servicecidrs`
(`pkg/apis/networking/validation/validation.go`);
`discovery.k8s.io/v1`'s `endpointslices`
(`pkg/apis/discovery/validation/validation.go`);
`flowcontrol.apiserver.k8s.io/v1`'s `flowschemas`,
`prioritylevelconfigurations`
(`pkg/apis/flowcontrol/validation/validation.go`); `node.k8s.io/v1`'s
`runtimeclasses` and `coordination.k8s.io/v1`'s `leases` (both inline
`NameIsDNSSubdomain` directly rather than through a named var — same
rule, confirmed the same way). `name_format_violations` now covers 28
resources total. Every other resource is left unchecked rather than
guessing at a rule for it, gating both `create` and `update`. Extending
this to more resources is real, separate follow-up work, one verified
entry at a time (the function's own doc comment says so explicitly).

Conversion only needed for genuinely multi-version groups
(admissionregistration, autoscaling, certificates, coordination,
networking, resource, storage, apiserverinternal, storagemigration) —
**not yet landed**.

`scheme::quantity::Quantity` parses real upstream's own resource-quantity
string format (`100m`, `1.5Gi`, `1e3`, …) — a faithful port of the real
grammar (`staging/.../api/resource/quantity.go`'s own doc comment, quoted
verbatim in this module's own doc comment), **honestly not
byte-for-byte**: real upstream falls back to an arbitrary-precision
decimal for any value that would overflow `int64`; this port instead
holds every value as an exact `i128` milli-unit count, lossless for any
magnitude a real Kubernetes resource request/limit/quota has ever
practically used, and returns an error rather than silently losing
precision on the (unrealistic) values that would overflow that. Built as
the prerequisite `plugin/pkg/admission/limitranger`/`resourcequota` both
need for real min/max/ratio comparisons — not yet wired to either.

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
(the overwhelming majority) behaves identically either way. **All three
are now wired into a real `PATCH` verb** (Group E's own section has the
detail: `server::rest::patch`, selected by real `Content-Type`, real
optimistic concurrency, admission not yet run on it). **Not yet
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

**J. Admission** — **started**. `admission::namespace_lifecycle` is a
faithful port of real upstream's own `NamespaceLifecycle` plugin
(`staging/src/k8s.io/apiserver/pkg/admission/plugin/namespace/lifecycle/admission.go`,
fetched and read directly): forbids deleting the three immortal namespaces
(`default`/`kube-system`/`kube-public` — upstream's own literal
registration args), forbids `CREATE` into a namespace whose `status.phase`
is `Terminating`, and returns a real `404` (not a `403`) for `CREATE`/
`UPDATE` into a namespace that doesn't exist at all — matching upstream's
own "return the live lookup's NotFound error unwrapped" behavior. Named
honestly simplified: upstream's version also carries an informer-cache
staleness workaround (a 50ms wait + an LRU "force live lookup" list) this
plugin has no equivalent need for, since it always resolves the namespace
straight from storage (`server::rest::get`) rather than from a cache that
could be stale. **Wired into `server::listener`, unconditionally** — unlike
Group I's RBAC, this plugin needs no operator-provisioned bootstrap data,
so there's no "could lock every request out" risk to gate behind a config
flag; it runs on every `CREATE`/`UPDATE`/`DELETE` today.

`admission::default_toleration_seconds` is this crate's first **mutating**
plugin — a faithful port of real upstream's own `DefaultTolerationSeconds`
(`plugin/pkg/admission/defaulttolerationseconds/admission.go`, fetched and
read directly): every `Pod` `CREATE`/`UPDATE` (core group, no subresource)
that doesn't already carry its own toleration for the
`node.kubernetes.io/not-ready`/`node.kubernetes.io/unreachable` `NoExecute`
taints gets one appended, `tolerationSeconds: 300` — upstream's own
default (this crate has no admission-plugin flag surface yet, so only the
default value is ported, named honestly rather than hard-coded as if it
were the only value upstream supports). "Already tolerates" uses
upstream's own real matching rule: a toleration whose `key` is the taint's
key (or empty — wildcard) *and* whose `effect` is `NoExecute` (or empty)
counts, regardless of `operator`/`tolerationSeconds`. A pure `Value ->
Value` transform, no I/O needed (unlike `namespace_lifecycle`, nothing
about this decision depends on other cluster state) — runs on the decoded
request body in `server::listener` before it reaches
`rest::create`/`update`, so the appended tolerations are part of what
actually gets validated and persisted. Also wired unconditionally, same
no-lockout-risk reasoning as `namespace_lifecycle`.

`admission::service_account` is both mutating and validating, `CREATE`-only
— a faithful port of real upstream's own `ServiceAccount` plugin
(`plugin/pkg/admission/serviceaccount/admission.go`, fetched and read
directly): defaults `spec.serviceAccountName` to `"default"` when unset (a
mirror pod is never mutated — its spec is left alone and instead validated
against three real restrictions: may not reference a `ServiceAccount`, a
`Secret`, or a projected `ServiceAccountToken` volume source, all ported
from upstream's own mirror-pod `Validate` branch), requires the referenced
`ServiceAccount` to exist (forbidden if not), auto-mounts a projected
`kube-api-access-*` token volume into every container lacking its own
mount at `/var/run/secrets/kubernetes.io/serviceaccount` unless the pod or
its `ServiceAccount` opts out (`shouldAutomount`, ported exactly — pod's
own preference wins, then the `ServiceAccount`'s, defaulting `true`), and
copies the `ServiceAccount`'s `imagePullSecrets` onto the pod when it
specifies none of its own. Split the same pure-decision/real-I/O-step way
as `namespace_lifecycle`. Named honestly not ported:
`LimitSecretReferences`/`enforceMountableSecrets` (upstream's own default
is `false` unless an operator annotates the `ServiceAccount`
`kubernetes.io/enforce-mountable-secrets: "true"` — a real but
off-by-default check most real clusters never exercise) and the
`ephemeralcontainers` subresource validation path (this crate doesn't
serve any subresource yet).

`admission::default_storage_class` is mutating, `CREATE`-only — a faithful
port of real upstream's own `DefaultStorageClass` plugin
(`plugin/pkg/admission/storage/storageclass/setdefault/admission.go`,
fetched and read directly): a `PersistentVolumeClaim` that doesn't already
specify a class (upstream's own real `PersistentVolumeClaimHasClass` —
either the beta `volume.beta.kubernetes.io/storage-class` annotation or a
non-null `spec.storageClassName` counts) gets `spec.storageClassName` set
to whichever `StorageClass` carries a default annotation
(`storageclass.kubernetes.io/is-default-class` or the beta spelling, value
`"true"`), newest by `creationTimestamp` first, name-ascending as the
tie-break — both upstream's real selection rule and its real tie-break,
ported exactly, not approximated. No-ops if no class is marked default,
same as upstream. The one real I/O step (`server::rest::list` over every
`StorageClass`) always runs on a `PersistentVolumeClaim` `CREATE` today,
even when the PVC already has a class — a real, named inefficiency
(`mutate` itself no-ops in that case, but only after the list already
happened), not silently optimized around with a duplicate has-class check.

`admission::limit_ranger` is mutating (pods, `CREATE` only) + validating
(pods and `PersistentVolumeClaim`s) — a faithful-but-scoped port of real
upstream's own `LimitRanger` plugin
(`plugin/pkg/admission/limitranger/admission.go`, fetched and read
directly): container-level (`LimitRange.spec.limits[].type ==
"Container"`) min/max/ratio enforcement across `containers` and
`initContainers`, container-level defaulting (a container missing a
request/limit for a resource the `LimitRange` carries a
`default`/`defaultRequest` for gets it filled in, and the pod is
annotated `kubernetes.io/limit-ranger` describing what was set — real
upstream's own annotation key and message format, ported exactly),
pod-level (`LimitTypePod`) aggregate min/max/ratio enforcement against
the pod-wide total (`pod_requests`/`pod_limits`, real upstream's own
`podRequests`/`podLimits` aggregation ported exactly — sums ordinary
containers, folds in a restartable init container/"sidecar"
[`restartPolicy: Always`] cumulatively into both the pod-wide total and
a running sidecar subtotal since it keeps running alongside the main
containers, and takes the *max* across ordinary sequential init
containers' own need plus that running sidecar subtotal, since ordinary
init containers never run concurrently with each other), and
`PersistentVolumeClaim`-level (`LimitTypePersistentVolumeClaim`) min/max
enforcement on `spec.resources.requests` (PVCs are validated, never
defaulted — storage is a required part of the spec, matching upstream).
Built on `scheme::quantity::Quantity` for real comparisons — its own
`i128` exactness also means this port skips upstream's own
`MaxMilliValue` overflow-avoidance dance entirely, and `Quantity` gained
real `+`/`max` for the pod-level aggregation's own summing/maxing.
**Not yet ported, named honestly**: the real `PodLevelResources`
feature-gate override (pod-level `spec.resources` overriding the
aggregated total for CPU/memory — an alpha feature with no feature-gate
machinery in this crate to model, so the aggregated per-container total
is always used). Same pure-decision/real-I/O-step split as every other
Group J plugin (`server::rest::list` over `LimitRange` in the target
namespace is the one I/O step).

`admission::pod_security` is validating, `CREATE`-only — a
faithful-but-scoped port of real upstream's own Pod Security Standards
plugin (`staging/src/k8s.io/pod-security-admission/policy/*.go`, fetched
and read directly): enforces whichever level a namespace's real
`pod-security.kubernetes.io/enforce` label requests
(`baseline`/`restricted`; an absent or unrecognized label means
`privileged` — upstream's own "no restriction" default). **All twelve
real `baseline`-level checks are now ported** (each the current,
latest-`MinimumVersion` variant only — this always enforces whatever the
newest upstream variant of each check requires, no version-pinned check
history modeled): `privileged`, `hostNamespaces`
(`hostNetwork`/`hostPID`/`hostIPC`), `hostPorts`, `hostPathVolumes`,
`capabilities_baseline` (the real default-capability allowlist),
`seccompProfile_baseline` (the 1.19+ field form only — the pre-1.19
alpha-annotation form is real but long obsolete, skipped rather than
silently glossed over), `sysctls` (the 1.32+ allowed set, the widest
upstream has defined), `procMount` (the `hostUsers: false` relaxation is
ported too, unconditionally rather than behind the feature gate this
crate doesn't model), `hostProbesAndHostLifecycle` (upstream's own
newest baseline check, 1.34+), `windowsHostProcess`, `appArmorProfile`
(both the deprecated annotation form and the real field form), and
`seLinuxOptions` (the 1.31+ allowed-type set). **All six real
`restricted`-level checks are ported too**: `runAsNonRoot` (real
upstream's own three-way pod/container logic), `runAsUser` (forbids
`runAsUser=0`), `allowPrivilegeEscalation` (Windows-exempt, matching
upstream's own 1.25+ variant), `capabilities_restricted` (must drop
`ALL`, may only add `NET_BIND_SERVICE`; Windows-exempt too),
`seccompProfile_restricted` (same three-way logic as `runAsNonRoot`,
Windows-exempt), `restrictedVolumes` (the real inline-volume-source
allowlist). Real upstream's own `OverrideCheckIDs` is ported too: at
`Restricted`, `hostPathVolumes`/`capabilities_baseline`/
`seccompProfile_baseline` are suppressed in favor of their
strictly-stronger restricted equivalents, so a violation isn't reported
twice for the same root cause. Same pure-decision/real-I/O-step split as
every other Group J plugin (`server::rest::get` on the target namespace,
to read its label, is the one I/O step).

`admission::resource_quota` is validating, `CREATE`-only, **pods,
PersistentVolumeClaims, and Services** — a faithful-but-substantially-scoped
port of real upstream's own `ResourceQuota` plugin
(`staging/src/k8s.io/apiserver/pkg/admission/plugin/resourcequota/controller.go`
+ `pkg/quota/v1/evaluator/core/{pods,persistent_volume_claims,services}.go`,
fetched and read directly): forbids a `Pod`/`PersistentVolumeClaim`/
`Service` `CREATE` that would push a namespace's tracked usage
(`pods`/`cpu`/`requests.cpu`/`limits.cpu`/`memory`/`requests.memory`/
`limits.memory`/`ephemeral-storage`/`requests.ephemeral-storage`/
`limits.ephemeral-storage`/`hugepages-<size>`/`requests.hugepages-<size>`
— real upstream's own `podComputeUsageHelper`, restricted to this subset
— plus `persistentvolumeclaims`/
`requests.storage`, plus both again under the claim's own storage
class's scoped key (`<class>.storageclass.storage.k8s.io/...`,
`V1ResourceByStorageClass`) when it names one — real upstream's own
`pvcEvaluator.Usage`, minus only the alpha
`RecoverVolumeExpansionFailure`-gated `status.allocatedResources`
comparison — plus `services`/`services.nodeports`/`services.loadbalancers`
— real upstream's own `serviceEvaluator.Usage`, ported exactly including
the real `allocateLoadBalancerNodePorts: false` node-port-counting
carve-out) over any `ResourceQuota`'s own `spec.hard`. A `ResourceQuota`
only applies to PVCs/Services when it's unscoped (`spec.scopes` empty) —
real upstream's own `pvcEvaluator.Matches` only consults scopes behind
the alpha `VolumeAttributesClass` feature gate, and
`serviceEvaluator.Matches` never consults scopes at all either way, so
this matches both evaluators' real stable behavior, not a shortcut.
Reuses `limit_ranger`'s own
`pod_requests`/`pod_limits` for the aggregation, since real upstream's
own quota usage function (`PodUsageFunc`) calls the exact same
underlying helper `limit_ranger` already ported. Terminal pods
(`Failed`/`Succeeded`) are excluded from summed usage, matching real
upstream's own `QuotaV1Pod` convention. Real upstream's own denial
message format is ported exactly: `"exceeded quota: <name>, requested:
<resource>=<val>, used: <resource>=<val>, limited: <resource>=<val>"`,
restricted to only the resource(s) that actually exceeded; the first
`ResourceQuota` found to be exceeded wins (matches upstream's own
first-failure-wins loop, not an aggregate of every quota's own
violations). `ResourceQuota.spec.scopes` is now matched too (real
upstream's own all-scopes-must-match semantics — an existing pod is only
summed into a quota's usage if it matches every scope the quota lists,
computed per-quota, not once globally): `Terminating`/`NotTerminating`
(real upstream's own `IsTerminating` — `activeDeadlineSeconds` set and
non-negative) and `BestEffort`/`NotBestEffort` (a real port of upstream's
own `ComputePodQOS` — per-container "does every container set both cpu
and memory limits matching its own requests", not the pod-wide
sidecar-aware total `pod_requests`/`pod_limits` compute for a different
purpose), and `PriorityClass` (real upstream's own `podMatchesScopeFunc`
for the classic `spec.scopes` list form: an implied `Exists` operator,
so this route alone is genuinely just "does the pod have *any* priority
class name set"), and `CrossNamespacePodAffinity` (real
upstream's own `usesCrossNamespacePodAffinity`: a structural presence
check across all four real pod-(anti-)affinity term lists for an
explicit `namespaces` list or any `namespaceSelector` at all — not an
evaluation of what the selector actually matches, same as upstream's
own check). **All six real scope names are now matched (for pods).**

**`spec.scopeSelector` is now ported too** (`quota_matches_pod_scopes`,
`scope_requirement_matches`): real upstream's own
`getScopeSelectorsFromQuota` concatenates `spec.scopes` (each
synthesized as an implied-`Exists` requirement) with
`spec.scopeSelector.matchExpressions` (the real per-expression
`scopeName`/`operator`/`values` form) into one list, then requires every
entry in it to match — ported exactly, so a quota can now combine both
forms (e.g. `scopes: [BestEffort]` AND `scopeSelector: {PriorityClass In
[high]}`) with real AND semantics. `PriorityClass` is the one real scope
name whose match depends on the operator: `Exists`/`DoesNotExist` are
plain presence checks, `In`/`NotIn` match against a specific set of
priority class names (real upstream's own `podMatchesSelector` — a
label-selector match against a synthetic single-key
`{PriorityClass: <name>}` label set, ported as real `In`/`NotIn`
selector semantics, not reimplemented from scratch). Every other scope
name ignores the operator/values, matching real upstream's own
`podMatchesScopeFunc` switch exactly. The PVC/service/generic
object-count evaluators' own "unscoped quotas only" check
(`quota_has_any_scope_selectors`) now also treats a `spec.scopeSelector`
with any `matchExpressions` as making the quota scoped, not just
`spec.scopes` — matching real upstream's own `generic.Matches`, which
folds every entry from *either* source through the same `scopeFunc`.

On top of the three specialized evaluators, `admission::resource_quota`
also ports real upstream's **generic `objectCountEvaluator`**
(`staging/src/k8s.io/apiserver/pkg/quota/v1/generic/evaluator.go`,
fetched and read directly): `check_object_count_create` covers *every*
other resource kind — the same real-upstream mechanism that gives a
plain `ResourceQuota.spec.hard: {count/secrets: "10"}` its meaning —
through the stable `count/<resource>` (core group) /
`count/<resource>.<group>` (other groups) key convention
(`count_quota_resource_name`, a direct port of real upstream's
`ObjectCountQuotaResourceNameFor`). Like real upstream's own
`MatchesNoScopeFunc` for this evaluator, it only ever matches an
*unscoped* `ResourceQuota` — no scope semantics apply to a bare object
count. Wired in `server::listener` as the `else` arm alongside the
pod/PVC/service dispatch: any namespaced `CREATE` whose resource isn't
one of those three specials runs the generic check instead, listing the
existing objects of that same `(group, resource)` and every
`ResourceQuota` in the namespace. Real upstream's own registry
(`pkg/quota/v1/evaluator/core/registry.go`'s `NewEvaluators()`) confirms
this is the *complete* real evaluator set — three specials plus the
generic fallback for everything else — so, unlike the pod/PVC/service
evaluators (each independently a narrowed port of a specialized real
evaluator), the generic evaluator itself is now a **complete** port.
`ephemeral-storage`/`requests.ephemeral-storage`/
`limits.ephemeral-storage` are now tracked too, the same
request/limit shape as `cpu`/`memory` (`pod_compute_usage`, extended
this session), and so is the `hugepages-<size>`/
`requests.hugepages-<size>` prefix family (real upstream's own
`podResourcePrefixes`/`requestedResourcePrefixes` — hugepages carry no
separate `limits.hugepages-*` tracking at all in real upstream either,
since a hugepage request and its limit are always equal in a real pod
spec; `quota_applies` matches a `spec.hard` key under either the
`hugepages-`/`requests.hugepages-` prefix, same as real upstream's own
`quota.ContainsPrefix`). Extended resources (e.g. `nvidia.com/gpu`) are
now tracked too, in their real `requests.<name>`-only form (real
upstream's own `isExtendedResourceNameForQuota`/`IsExtendedResourceName`
— overcommit isn't supported for extended resources, so no bare or
`limits.`-prefixed form is ever quota-recognized, matching real
upstream's own `podComputeUsageHelper` extended-resource branch and its
own comment on why). `is_native_resource`/`is_extended_resource_name`
port real upstream's own `helper.IsNativeResource`/
`IsExtendedResourceName` (`pkg/apis/core/v1/helper/helpers.go`) — not
ported: upstream's own final `IsQualifiedName` structural re-validation,
a named simplification. The PVC evaluator's own real per-storage-class
resource family is now tracked too (`pvc_storage_class_ref` — real
upstream's own `storagehelpers.GetPersistentVolumeClaimClass`, beta
`volume.beta.kubernetes.io/storage-class` annotation taking precedence
over `spec.storageClassName` same as `admission::default_storage_class`'s
own `pvc_has_class` already established — charges
`persistentvolumeclaims`/`requests.storage` a second time under the
claim's own `<class>.storageclass.storage.k8s.io/...` key,
`quota_applies_to_pvcs` matching any `spec.hard` key ending in that
suffixed form, same as real upstream's own `MatchingResources`'
`strings.HasSuffix` check). **Substantially scoped, named honestly** in
the one resource-family gap now left: real upstream's three specialized
evaluators track no further families this crate doesn't; there is **no
persisted `status.used` counter** — usage is recomputed live from a
fresh object list on every check rather than an incrementally
maintained, optimistic-lock-protected running total, which means
(unlike real upstream) two concurrent `CREATE`s that each individually
fit under the quota can both be admitted, together exceeding it — a
real, narrow, accepted concurrency gap, not silently glossed over.
Placed last among this crate's admission blocks (after `LimitRanger`'s
own defaulting), the same relative position real upstream's own default
plugin order uses, so quota sees the final, fully-defaulted object.

**Not yet landed**: every other built-in plugin, `ResourceQuota`'s own
persisted usage counter (above), a
generic plugin-chain/registry abstraction (today `server::listener`
hand-calls each plugin directly, not through
any dispatch table), mutating/validating webhooks, and
ValidatingAdmissionPolicy/
MutatingAdmissionPolicy on CEL. **Build the CEL cost budget before wiring
any CEL-driven admission path** — an unbudgeted CEL evaluator in the
request path is a denial-of-service surface.

**K. CRDs (apiextensions)** — **not started**. Dynamic storage
registration, structural schemas, pruning, defaulting,
`x-kubernetes-validations` CEL with type-checking, conversion webhooks,
`established`/`namesAccepted` condition machinery.

**L. Aggregation layer** — **not started**. `APIService` objects,
`ServiceResolver`, reverse proxying, discovery merge, availability
conditions.

**M. APF, audit, observability** — **started**. `audit::event::build_event`
is a pure builder for one real `audit.k8s.io/v1` `Event` document
(`staging/src/k8s.io/apiserver/pkg/apis/audit/v1/types.go`, fetched and
read directly), `Metadata` level (who did what to which object — not
request/response bodies, real upstream's own `Request`/
`RequestResponse` levels), single-stage (`ResponseComplete` only — real
upstream's other three stages, including the long-running-request-only
`ResponseStarted`, aren't modeled — `watch` is a named, narrow exception:
its one logged event is stamped `ResponseComplete` right as the stream
*starts*, since this crate has no hook into when a stream later ends).
**Now wired into `server::listener`**: `handle_with_audit` wraps every
request (the far less invasive place to add this than threading an
audit context out through `handle`'s own many early returns), building
and logging one real event per request once the response status is
known. Every request is unconditionally logged at `Metadata` level —
real upstream's own policy-driven per-rule level selection isn't
modeled. **The sink is this crate's own `tracing` output**
(`target: "nodeapiserver::audit"`, one JSON line per request) — a real,
working choice consistent with how every other component in this
workspace already logs, not real upstream's own dedicated
`--audit-log-path` file with rotation, and not a webhook backend either.
`/healthz`/`/readyz`/`/livez` now have real per-check output too
(`server::healthz`, a faithful-but-scoped port of real upstream's own
`k8s.io/apiserver/pkg/server/healthz`, fetched and read directly):
`/healthz`/`/livez` run just the `ping` check (upstream's own default
when no checks are explicitly installed); `/readyz` adds a `storage`
check reflecting whether the listener's own `StorageClient` connection
is present — a coarser, named-honest signal than real upstream's own
live per-request etcd ping (this crate's storage connection is
established once at listener startup, "best-effort, `None` on failure",
not re-probed per request). Response shape ported exactly: bare `"ok"`
on full success with no `?verbose`; per-line `[+]<name> ok`/`[-]<name>
failed: reason withheld` output (upstream's own real "never leak the
actual error to an unauthenticated caller" posture) followed by `"<name>
check failed"` and a real `500` on any failure, always verbose
regardless of the query param; the same per-line output followed by
`"<name> check passed\n"` on success when `?verbose` is given. Not
ported: the `log`/`informer-sync`/`shutdown` checks (klog-specific,
no-informers-here, and no graceful-shutdown machinery respectively) and
the `?exclude=` query param. `deploy/setup-control-plane.sh` already
polls `/readyz?verbose`, so this is now genuinely meaningful there, not
just a bare `200`. FlowSchema/PriorityLevelConfiguration queueing and
`/metrics` remain **not started**.

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
