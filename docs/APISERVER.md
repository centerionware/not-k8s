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
One real, named exception to "JSON field name identical to the proto
field name" found live by Group K's own strategic-merge-patch work: a
`JSONSchemaProps`'s seven `x-kubernetes-*` extension fields
(`x-kubernetes-list-type`, `x-kubernetes-preserve-unknown-fields`, ...)
have a real Go JSON tag that doesn't follow the standard
camelCase-from-field-name convention every other vendored field does —
undetected, a submitted CRD silently lost all seven on protobuf encode
(the field lookup by JSON key never matched, so they were treated as
unrecognized and skipped). `build/proto_parse.rs`'s
`real_x_kubernetes_json_name` now detects and corrects this one specific
family. A second, related finding from the same work: `JSONSchemaProps.
items`/`.additionalProperties` are `JSONSchemaPropsOrArray`/
`JSONSchemaPropsOrBool` on the wire — real upstream's own custom-marshaled
Go types (the same "doesn't marshal as its own struct shape" pattern
`metav1.Time`/`apiextensions.v1.JSON` already needed a codec exception
for) that write completely unwrapped in real JSON (a plain schema
object, or a plain array/bool), not as `{"schema": ...}`/`{"allows":
...}` — `codec::protobuf`'s `is_json_schema_props_or_array`/
`_or_bool` now handle both. `codec::json`/`codec::yaml` are thin wrappers; `codec::negotiation`
parses `Accept`/`Content-Type` including `kubectl get`'s `as=Table;g=...;v=...`
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
printer. **Now actually wired into `GET`/`LIST`** (`server::listener`
checks `Accepted::wants_table()` from the request's own `Accept` header
and runs the response through `convert_to_table` when set) — a real gap
found this session: the converter had been landed and correctly
documented for a while, but nothing in `server/` ever called it, so a
real `kubectl get pods` against a live nodeapiserver got raw JSON
instead of the columnar `Table` output every `kubectl get` actually
negotiates by default. Not yet done: any per-type printer, and
`PartialObjectMetadata`.

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
*new* dependency, already pulled in transitively by `rustls`).
**`EncryptionConfiguration` YAML parsing now exists too**
(`storage::encryption_config`, fetched and read directly against
`staging/src/k8s.io/apiserver/pkg/apis/apiserver/v1/types_encryption.go`):
parses the real document shape (`resources: [{resources: [...],
providers: [...]}]`) into a resolvable set of `encryption::
PrefixTransformers`, one per resource entry, matched by real upstream's
own resource-name/wildcard rules (`secrets`, `<resource>.<group>`, `*.`,
`*.<group>`, `*.*`) with real "earlier entries take precedence"
first-match-wins resolution. Only `aesgcm`/`identity` build (matching
`storage::encryption`'s own scope); `aescbc`/`secretbox`/`kms` parse
structurally but resolve to a real, named error rather than being
silently dropped or misapplied. `NODEAPISERVER_ENCRYPTION_CONFIG_FILE`
loads and validates the file at listener startup
(`config::Config::encryption_config_file`) — a misconfigured file is a
loud startup warning.

**Milestone: encryption-at-rest is genuinely wired end to end now** —
`range`/`put`/`txn` (via the shared `persist_update` tail every write
verb funnels through) and `watch` all agree, the correctness
requirement this doc used to name as the reason this was deferred.
`StorageClient` carries the parsed config (`with_encryption`, attached
once right after `connect`, before any clone — including every
long-running cache-reflect loop — is made) and exposes
`transformers_for(group, resource)`. Two functions in `server::rest`
are the entire wiring surface: `decrypt_and_decode` (the encrypted-aware
counterpart to `decode_stored_object` — every real read call site in
the crate uses this instead, `get`/`list`/`update`/`patch_prepare`/
`update_status`/`patch_status`/`delete`, plus `watch`'s own event
decoding in `server::watch_event`) and `encrypt_for_storage` (called at
both real `PutRequest` construction sites, `create` and
`persist_update`). Both use the object's own etcd key as AES-GCM's
authenticated data, matching real upstream's own
`dataCtx.AuthenticatedData()` convention exactly. A resource with no
matching entry in the loaded config is written/read as-is, unchanged
from before this wiring existed — encryption is opt-in per resource,
never a blanket switch. Named, honest gap: the real `stale` flag
`transform_from_storage` returns (upstream's own "this was encrypted
under a non-primary key, rewrite it with the current one next write" —
a key-rotation migration signal) is read but discarded; there's no
background re-encryption sweep to act on it yet.

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
`400 BadRequest` (not a `500`) for a client-malformed selector.
**Real pagination now too** (`?limit=`/`?continue=`, `path::RequestInfo`'s
own `limit`/`continue_token` fields): a paginated request always bypasses
`cacher::store::WatchCache` and reads directly from nodestore (ordered
range-scan-with-resume-point is what the underlying store gives for
free; the cache's own unordered in-memory store doesn't support it), one
extra `RangeRequest.limit`/`.more` round trip per page. The `continue`
token is this crate's own opaque encoding (base64 of `<resume-key>\0
<revision>` — no compatibility requirement with real upstream's own
token format, since nothing outside this crate's own client/server pair
ever reads one), where `resume-key` is the last-returned key plus a
single `0x00` byte — the standard etcd idiom for "the immediate
lexicographic successor," which is exactly the correct next `Range`
start. A malformed token is a real `400`
(`rest::ListOutcome::InvalidContinueToken`), not a `500` or a silently
wrong resume point. Real upstream's own documented caveat applies here
too: label/field selector filtering happens *after* the limited range
fetch, so a page can come back with fewer than `limit` items (even
zero) despite more matching items existing on later pages.

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

**`WATCH` now applies the same real label/field selector filtering
`LIST` already did — a real, previously-undocumented gap found and
closed the same way the `Table`-conversion wiring gap was**: every
event on the stream (replay and live alike) is checked against
`cacher::selector::object_matches` (`watch_event_matches_selector`)
before it's ever encoded and sent — a `kubectl get pods -w -l
app=foo` was silently getting *every* pod's events before this,
matching `LIST`'s own selector only for the initial snapshot, not the
live stream. A malformed selector is a real `400`, checked before the
stream even opens, same as `LIST`. `Bookmark` events and any event
this cache holds no value for always pass through (there's nothing to
test a selector against); a value this build can't decode also passes
through unfiltered rather than being silently dropped — filtering
narrows a watch, it never hides a real event this build failed to
parse.

**`PATCH` is real too now** (`rest::patch_prepare`/`patch_persist`,
reusing Group G's already-landed `patch::json_patch`/`merge_patch`/
`strategic_merge`): the real `Content-Type` selects the patch kind
(`application/json-patch+json`/`application/merge-patch+json`/
`application/strategic-merge-patch+json` —
`rest::patch_kind_for_content_type`, a real `415` for anything else,
Server-Side Apply's own `application/apply-patch+yaml` deliberately not
recognized, matching Group G's own "not yet landed" note), applied to
the object `patch_prepare` itself reads, then persisted by
`patch_persist` through the same optimistic-concurrency
`Txn`-compared-against-`ModRevision` tail `rest::update` already used
(factored out as `persist_update`) — no client-submitted
`resourceVersion` needed, unlike `PUT`, since the object being patched
*is* the one just read. **`PATCH` now runs Group J admission too**: the
function is deliberately split into a "prepare" half (fetch + apply the
patch) and a "persist" half (validate + default + write) so
`server::listener` can run admission against the real candidate object
in between — specifically `namespace_lifecycle` and `LimitRanger`'s own
PVC-`Update` validation, the only two Group J plugins that ever apply to
an `Update`-shaped write in this crate (every other plugin is
`CREATE`-only, so there's genuinely nothing else to wire here).

**`DELETECOLLECTION` is real too now** (`rest::delete_collection`, a
faithful-but-scoped port of real upstream's own `Store.DeleteCollection`,
`k8s.io/apiserver/pkg/registry/generic/registry/store.go`, fetched and
read directly): lists every match via the same `label_selector`/
`field_selector` filtering `LIST` already applies, deletes each one by
name via `rest::delete`, ignoring one that's already gone (matching real
upstream's own `!apierrors.IsNotFound(err)` guard — a concurrent delete
of the same object isn't a collection-delete failure), and returns the
pre-deletion `List`, real upstream's own response shape. **Named, honest
simplifications**: real upstream deletes with a worker pool and paginates
the list internally; this port deletes sequentially and lists in one
shot (this crate's own `list` doesn't paginate either yet). **Still
doesn't run Group J admission**, a small gap in practice:
`namespace_lifecycle`'s own immortal-namespace check needs a `name`,
which a collection delete never has, and `LimitRanger`'s only
`Update`-shaped check is a PVC *minimum*, which deleting can't violate.

**Every real, generic REST verb this build knows about is now wired
in** — `GET`/`LIST`/`CREATE`/`DELETE`/`UPDATE`/`PATCH`/`DELETECOLLECTION`,
plus a real streaming response for `WATCH`. **Not yet landed**: the rest
of admission (Group J's own section has the
running plugin list; `DELETECOLLECTION`'s own small gap above is the only
verb-level admission gap left), the real handler chain fully unified into one
ordered dispatcher (authn -> authz -> APF -> admission -> REST — a hard
requirement on order, not a style choice, once it fully exists; today
each piece is wired in ad hoc, in the right relative order, not through
one shared pipeline), `/openapi/v2`. The throwaway e2e rig described
above should land as part of this group, not after it.

**The generic `<resource>/status` subresource is real too now, both
`PUT` and `PATCH`** (`rest::update_status`/`rest::patch_status`, wired
into `server::listener` as their own branches): a faithful port of real
upstream's own generic `GenericStatusREST`
(`k8s.io/apiserver/pkg/registry/generic/registry/store.go`) — the
submitted body's (or, for `PATCH`, the patched candidate's) `.status`
field replaces the stored object's own, every other top-level field
(`spec`, most of `metadata`) is ignored, real optimistic concurrency for
`PUT` (`metadata.resourceVersion` must match; `PATCH` needs none, same
as the main resource's own `patch_persist`). This is the first write
path in this crate for status data on any resource — nothing could
persist a `status` write at all before this. `patch_status` reuses the
same `json_patch`/`merge_patch`/`strategic_merge` application Group
G's main `PATCH` path uses (factored into a shared `apply_patch` helper),
then merges only the result's own `.status` onto the existing object,
exactly like `update_status`'s own `PUT` semantics. **Named, honest scope
narrowing**: no structural/type validation of the status write (real
upstream's own per-type status strategies are hand-written Go with no
generic table to derive them from, same finding that already scoped down
`scheme::validation` elsewhere), and no Group J admission runs on either
— every plugin that ever applies to an `Update`-shaped write in this
build (`namespace_lifecycle`'s Terminating-namespace check,
`LimitRanger`'s PVC-minimum check) is about a create/full-object write
and has nothing to say about a status-only replace.

**Real, crate-wide bug found live and fixed** (Group L's own
`tests/apiservice_roundtrip.rs` — a plain get-then-update round trip,
never previously exercised since every prior write-then-read-back test
happened to reuse a `create`/`update` call's own return value directly):
`rest::get`/`list`/`delete` never stamped `metadata.resourceVersion` on
the object(s) they returned, for *any* resource, built-in or CRD.
Root cause: `resourceVersion` is never actually persisted into a stored
object's own bytes — `create`/`persist_update` both stamp it onto their
own return value only *after* the write that produces the revision,
since it doesn't exist yet while those bytes are still being built,
matching real upstream's own posture (`resourceVersion` is always
etcd's `mod_revision`, read back at serve time, never object content).
A plain read has to do that same stamping itself, from its own `Range`/
`DeleteRange` response's `mod_revision` — nothing did, so a genuine
`GET` followed by an `UPDATE` (the single most common real
kubectl/controller workflow: read, modify, write back with the read
`resourceVersion`) was silently broken for every resource this build
serves. Fixed in `get`/`list` (both the cache and direct-nodestore
paths) and `delete`; `server::watch_event::to_watch_event_json` had the
identical gap for `Added`/`Modified`/`Deleted` watch events (only the
synthetic `Bookmark` case already stamped one) and got the same fix.

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
need for real min/max/ratio comparisons — **now wired into both**
(this paragraph was stale about that; see Group J's own section for
each plugin's current real scope).

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
response's own `user` field for real observability. **Authorization now
does check this identity too** (Group I's `enforce_rbac`, opt-in — see
that section below; this doc line used to say authentication had no
authorization to enforce against, stale since Group I landed).
`authn::self_review` is real too now — `SelfSubjectReview`
(`kubectl auth whoami`), wired into `server::listener` as its own `POST`
branch, purely reflecting whatever identity `x509` (or the anonymous
fallback) already produced into the real `UserInfo` shape, no new
authentication logic, never persisted (same virtual-resource posture
`authz::sar`'s review kinds established). Everything else named above
(ServiceAccount JWT, OIDC, TokenReview, bootstrap tokens, anonymous) is
not started.

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
silently exempted.

**`SubjectAccessReview`/`SelfSubjectAccessReview` are real too now**
(`authz::sar`, wired into `server::listener` as its own `POST` branch,
unconditional — not gated by `enforce_rbac`, since answering "would RBAC
allow this" is a read on the engine's own state, not an enforcement
decision): `SubjectAccessReview`/`SelfSubjectAccessReview`/
`LocalSubjectAccessReview` all resolve straight to the same
`resolve::rules_for`/`rbac::rules_allow` real RBAC uses, no new
evaluation logic — `LocalSubjectAccessReview` (the namespaced variant)
shares the same parsing/response code, with the URL's own namespace
overriding whatever the body said (matching real upstream's own "the
namespace is the URL's, not the body's" rule for a namespaced
subresource). `SelfSubjectRulesReview` is real too, its own branch (a
different response shape — `resourceRules`/`nonResourceRules`, not a
single `allowed`): lists every already-resolved `PolicyRule` for one
namespace, split by which fields each rule actually names
(`authz::sar::build_rules_status`). **A genuine virtual resource, not
persisted**, all four kinds — this crate's dispatcher checks for them
*before* the generic `is_create` handling specifically so none of them
ever fall through to `rest::create` and actually try to write one to
nodestore, matching real upstream's own synthetic REST connector
(`pkg/registry/authorization/subjectaccessreview`, never etcd-backed).
Named, honest scope: `denied`/per-rule `reason` are never populated on
`SubjectAccessReviewStatus` (real RBAC's own authorizer never returns an
explicit deny either — only allow/no-opinion — and this crate's engine
doesn't track which rule matched to build a `reason` string);
`SelfSubjectRulesReview`'s own `incomplete`/`evaluationError` **are**
populated, straight from `resolve::rules_for`'s own per-binding
resolution errors. Node authorizer and webhook authorization are not
started. PKI primitives (`rcgen`, `p256`, `x509-parser`, `pem`) are
already in-tree from `nodecontroller`'s CSR
group.

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
evaluators track no further families this crate doesn't. **The
persisted `status.used` counter now exists for every `ResourceQuota`
evaluator this crate has — pod, PVC, service, and the generic
object-count `count/<resource>` fallback** (`admission::resource_quota::
usage_after_pod_create`/`usage_after_pvc_create`/
`usage_after_service_create`/`usage_after_object_count_create` +
`server::listener::persist_quota_usage_updates`, one shared persist
routine driven by whichever evaluator's own update list the caller
passes): once a `CREATE` is admitted, the same post-create usage total
the check just verified is written back to each matching quota's real
`status.used` via `rest::update_status` (Group E's own generic
`/status` subresource), with a bounded (3-attempt) retry on a real
optimistic-concurrency `Conflict`, read-modify-write so no evaluator's
own status keys ever clobber another's. **The underlying admit-time
concurrency gap is still real and still named honestly, not closed by
this**: usage is still recomputed live from a fresh object list at
*check* time (not read from the persisted counter, which this build
never treats as authoritative for the check itself — only for what
gets reported afterward) — two concurrent `CREATE`s that each
individually fit can still both be admitted, together exceeding the
quota, exactly as before. What's new is that `status.used` genuinely
reflects real usage afterward (`kubectl describe resourcequota` now
shows accurate data) rather than never being written at all. This
closes `resource_quota`'s persisted-counter gap completely — the
remaining, still-real gap is the concurrency race itself, which needs
a genuinely different mechanism (in-flight reservation tracking, not
just persistence) to close.
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

**K. CRDs (apiextensions)** — **in progress**. Found on inspection, not
assumed: `CustomResourceDefinition` itself needed *zero* new plumbing —
`apiextensions.k8s.io/v1` is a real group in the vendored OpenAPI/proto
sets (Group A's codegen has no group allowlist, it walks every `.json`
under `vendor/openapi-spec/v3`), so `resolve_kind`/`schema_for_gvk`
already served it and `server::rest`'s generic verbs already stored/read
it correctly before this group's own code existed. The actual gap was
narrower than the whole feature list below suggests: making the
resources a CRD *defines* — which have no compiled schema at all, since
an operator's own schema doesn't exist until they submit one — routable
through the same generic verb dispatch.

`apiextensions::registry` (pure): resolves `(group, version, resource)`
against a decoded `CustomResourceDefinition` document — matches
`spec.group`/`spec.names.plural`, requires the CRD to be `Established`
(`status.conditions`) and the requested version to be `served`, and
returns the matched version's own `spec.versions[].schema.
openAPIV3Schema`. `server::rest::resolve_resource` is the new single
choke point every real verb now goes through: the static
`resolve_kind`/`schema_for_gvk` pair first (no I/O, the common case),
falling back to a live `LIST` of `customresourcedefinitions` (itself
served by the *static* path, so this never recurses) only on a miss.
`apiextensions::schema_defaults` (pure): a faithful, scoped-down port of
real upstream's own structural-schema `Default` algorithm
(`k8s.io/apiextensions-apiserver/pkg/apiserver/schema/defaulting`) —
walks `properties`/`items`/schema-shaped `additionalProperties`
recursively, filling in `default` wherever a value is absent or
explicitly `null`, and (matching real upstream exactly) never
recursively defaults *into* a value it just applied a default to.
`apiextensions::conditions` (pure): computes a CRD's own
`NamesAccepted`/`Established`/`storedVersions` status — a real,
synchronous naming-conflict check against every other already-
`Established` CRD in the same group (`plural`/`singular`/`kind`/
`listKind`/`shortNames`, `pkg/apiserver/validation`, fetched and read
directly), with `storedVersions` accumulating rather than being
overwritten (real upstream's own migration-tracking invariant). **A
deliberate, named divergence from real upstream's architecture, not an
oversight**: real `kube-apiserver` computes both conditions from a
separate, asynchronous in-process controller
(`pkg/controller/establish`) that only flips `Established` once it has
confirmed the CRD's storage was actually installed; this build computes
both synchronously, right on `CREATE` of the CRD object itself
(`server::rest::create`'s own CRD special case), because it has no
separate controller-manager loop that could own that reconciliation
in-process the way real upstream's does (and, per the user's own framing
of this build's scope: *"it's up to operators to WATCH/LIST CRDs and
react to them, apiserver just has to track them and set some defaults as
defined by the CRD's own spec"* — an async establishing controller
exists in real upstream to protect against exactly the kind of
distributed-consistency problem a single-process build with no separate
storage-installation step to wait on doesn't have).

**Real, wired, and live-tested now** (`tests/crd_roundtrip.rs`, the CRD
analogue of `tests/encryption_roundtrip.rs` — a real `nodestore` spawned
and driven end to end, not assumed from unit tests alone): `GET`/
`LIST`/`CREATE`/`DELETE`/`DELETECOLLECTION` (`delete_collection` gets
CRD support for free — it already delegates to `list`/`delete`) for a
CRD-defined resource, with real schema-driven defaulting on `CREATE`.
**`WATCH` for CR objects is real too now** — `server::rest::
resolve_dynamic_kind` exposes Group K's registry directly to
`server::listener`'s own `WATCH` dispatch, which, on a cache miss for a
resource its static table has never heard of (never for one it knows but
simply isn't boot-cached — that still gets no watch support, unchanged),
resolves it dynamically and lazily spawns a
`cacher::registry::CacheRegistry` reflector for it right then, on this,
the resource's own first-ever watch request — `CacheRegistry::spawn` was
already callable at any time, not just at boot, so this needed no new
primitive, only the dynamic resolve step in front of it. **Named, honest
scope**: nothing proactively reacts to a CRD's own lifecycle — a
reflector spawned this way keeps running for the rest of the process's
life even if the CRD is later deleted (real upstream's own per-CRD
informer teardown on deletion isn't modeled), and a CRD that becomes
`Established` is only ever discovered by the *next* watch request for
its resource, not eagerly the moment it's created.

**`UPDATE`/`PATCH` are real for CR objects now — all three real patch
kinds, `strategic-merge-patch` included** — `update`/`patch_prepare`/
`patch_persist`/`update_status`/`patch_status` all resolve through
`resolve_resource` the same way `create` does, with the same "no
structural validation beyond pruning/required/type, schema-driven
defaulting where a schema exists" scope. `PatchContext` widened from a
compiled-only `schema: &'static str` to `Option<&'static str>` plus a
carried-through `open_api_schema: Option<Value>`.
`apiextensions::schema_strategic_merge` is the runtime-schema sibling of
`crate::patch::strategic_merge`: a list field merges by key when its own
schema names `x-kubernetes-list-type: map` +
`x-kubernetes-list-map-keys` — a real array of field names (composite
keys), matched against *every* named key, a genuine improvement over the
compiled path's single `patch_merge_key` (built-in types in the vendored
spec never need more than one key, which is why that simplification was
safe there — not a reason to cap the CRD path the same way). Live-tested
(`tests/crd_roundtrip.rs`'s `update_and_patch_work_against_a_crd_defined_resource`
for the scalar-replacement case, and the dedicated
`strategic_merge_patch_merges_a_crd_list_field_by_its_own_x_kubernetes_list_map_keys`
for the real by-key list-merge behavior — one patched element merges by
key, an untouched sibling survives unchanged, and a non-matching element
appends, proving this is a genuine merge rather than a replace that
happened to look right) against a real `nodestore`.

**Discovery merge is real too** — `/apis`, `/apis/{group}`,
`/apis/{group}/{version}`, and their aggregated-discovery-v2 counterparts
now include every served, `Established` CRD's own resources alongside
the static table, so `kubectl` (and anything else that discovers
resources rather than hardcoding a URL) can see a CRD-defined resource
for the first time. `apiextensions::registry::discoverable_resources`
turns a list of decoded CRDs into flat `(group, version, resource, kind,
namespaced)` entries (reusing `resolve` itself, so there's exactly one
place the served/`Established` logic lives); `server::discovery` gained
a parallel `*_with_crds` function for each of its existing pure builders
rather than widening their signatures in place, specifically to avoid
touching over a dozen already-passing static-only unit tests for a
change that's purely additive. `server::listener`'s own discovery
routing fetches the CRD list live (one `LIST` of
`customresourcedefinitions`) only for a genuinely `/apis`-shaped
discovery path (3 or fewer path segments) — a resource-shaped request
under the same prefix (`/apis/{group}/{version}/namespaces/...`, by far
the hottest real traffic) costs nothing extra, and neither does any
`/api`/`/openapi/v3`/`/version` request (the core group never has CRDs
in it at all — a CRD's own `spec.group` is never empty). Every CRD-backed
resource reports the same real generic verb set this build now actually
serves for one (`create`/`get`/`list`/`update`/`patch`/`delete`/
`deletecollection`/`watch` — matching real upstream's own `crdHandler`,
which installs identical generic storage for every CRD, no per-Kind
verb customization).

**Required/type validation against a CRD's own schema is real now
too** — `apiextensions::schema_validation` is the runtime-schema sibling
of `scheme::validation` (which walks this crate's own *compiled*
`REQUIRED_FIELDS`/`TYPE_INFO` tables for built-in types): same two
checks (a required field genuinely missing, a present field with the
wrong JSON kind), same recursive walk, producing the exact same
`MissingField`/`TypeMismatch` violation shapes so `create`/`update`/
`patch_persist`'s own violation formatting needed no change to handle
either kind. Wired into the same three call sites `schema_defaults`
already was, so a malformed CR is now a genuine `422`, not silently
accepted.

**`x-kubernetes-preserve-unknown-fields` pruning is real too** —
`apiextensions::schema_pruning`, a faithful (if scoped-down) port of real
upstream's own recursive pruning walk
(`k8s.io/apiextensions-apiserver/pkg/apiserver/schema/pruning`): drops
any object key the schema's own `properties` doesn't declare (a
schema-shaped `additionalProperties` keeps the key but still recurses
into pruning its value; a bare `additionalProperties: true`, or
`x-kubernetes-preserve-unknown-fields: true` at any level, stops pruning
for that whole subtree), real upstream's own default posture for a
structural (`v1`) schema. Run *before* validation and defaulting in
`create`/`update`/`patch_persist` (matching real upstream's own order —
a default the schema names is by definition declared, so pruning never
removes one, and validation sees the object as it will actually be
stored). **Named, honest simplification**: `apiVersion`/`kind`/
`metadata` are hard-coded as always preserved at the object's own top
level, standing in for real upstream's schema-*completion* step (which
auto-injects those three into a CRD's effective schema regardless of
what the operator wrote) — this module doesn't implement that general
mechanism, only this one specific, security-relevant consequence of it
(an operator's schema that only ever describes `spec`/`status`, the
overwhelming common case, must never have this build silently prune the
object's own identity).

**The `status` subresource is now genuinely gated on the CRD declaring
it** — `registry::CrdResource::has_status_subresource`
(`spec.versions[].subresources.status`'s own presence, real upstream's
own opt-in): `update_status`/`patch_status` return a real
`UnknownResource` for a version that never declared it, not a silent
write, live-tested (`tests/crd_roundtrip.rs`'s
`status_subresource_is_gated_on_the_crd_declaring_it`) both ways — a
missing declaration rejected, a real one accepted end to end.

**Not yet landed, named honestly** (`apiextensions::mod`'s own doc
comment carries this list too): enum membership, numeric ranges, format
checks, and any cross-field consistency rule (`x-kubernetes-validations`
CEL is a CRD schema's real mechanism for all of that — **needs the CEL
cost budget built first**, Group J's own doc comment names this as a
real DoS surface, not optional hardening). Conversion webhooks. Pruning/
validation on the `status` subresource write itself (`update_status`/
`patch_status` keep the same "no structural checks on status" scope real
upstream's own generic status strategy has for built-ins too).

**`cel_ext` — the CEL cost budget, a real design pass (2026-08-21), no
code yet.** Blocks `x-kubernetes-validations` (Group K) and
ValidatingAdmissionPolicy/MutatingAdmissionPolicy (Group J) both —
scoped here rather than under either group since it's shared
infrastructure neither owns.

*Crate choice*: `cel` (the `cel-rust` project, MIT-licensed, actively
maintained — `0.14.3` as of 2026-08-21). **Correction to this section's
own first draft**: initially written as `cel-interpreter` from external
web research alone, before noticing `crates/nodescheduler` already
depends on `cel` for a real, live, already-merged use
(`framework::plugins::dynamic_resources`'s own DRA `CEL` device-selector
evaluation) — `cel-interpreter` is that same project's now-inactive
former crates.io name (confirmed both still resolve, but only `cel` has
had a release in the last year). Real API shape confirmed directly
against that already-working code, not docs.rs (whose auto-generated
summaries disagreed with each other on `Context`'s own basic shape):
`cel::Program::compile(expr)` -> `cel::Context::default()` +
`ctx.add_variable(name, value)`/`ctx.add_function(name, f)` ->
`program.execute(&ctx)` -> `cel::Value::Bool(bool)` on success.
`Context::add_variable`'s own bound (`TryIntoValue`, confirmed via a
blanket `impl<T: Serialize> TryIntoValue for T`) means a bare
`serde_json::Value` binds directly, no manual conversion needed. **No
built-in cost/step limiting of any kind** — confirmed by reading the
crate's own source, not assumed — so the entire cost-budget mechanism
below is this crate's own responsibility to build regardless of which
CEL evaluator sits underneath it; picking a different crate wouldn't
remove this work; it's not a rejected shortcut.

*Real upstream's own budget numbers* (`k8s.io/apiserver/pkg/apis/cel/
config.go` + `pkg/cel/limits.go`, fetched and read directly — the
project's own vendoring flow only pulls protos/OpenAPI specs, so this is
new: real Go source with no proto/OpenAPI representation, a genuinely
different kind of "vendor" than every prior group has needed):
- `RuntimeCELCostBudget = 10_000_000` — the overall runtime cost budget
  per `ValidatingAdmissionPolicyBinding` or per CustomResource
  validation (~1 real second of evaluation).
- `PerCallLimit = 1_000_000` — the cost limit for one individual CEL
  expression's own evaluation (~0.1s).
- `RuntimeCELCostBudgetMatchConditions = 2_500_000` — the separate,
  smaller budget for `matchConditions` (webhook/policy-binding
  pre-filters), per object.
- `CheckFrequency = 100` — real upstream doesn't check "has the budget
  been exceeded" after every single operation; it checks every 100
  iterations *within* a comprehension (`all`/`exists`/`map`/`filter`),
  the real reason a budget check is cheap enough to run at all without
  itself dominating the cost it's trying to bound.
- `MaxRequestSizeBytes = 3_145_728` (3MiB) — the real ceiling real
  upstream's own cost *estimator* (not the runtime evaluator) uses when
  a string/bytes field has no narrower schema-declared `maxLength` to
  bound a worst-case comprehension range by.
- A family of `Min*Size`/`Max*Size` constants (`MinStringSize = 2`,
  `MinBoolSize = 4`, `MinNumberSize = 1`, `MaxDurationSizeJSON = 32`,
  `MaxDatetimeSizeJSON = 32`, ...) — the literal-size bounds the same
  static estimator uses for scalar JSON types when no schema constraint
  narrows them further.

*The real mechanism is two layers, not one*, and both matter — a naive
single-layer implementation (either alone) is a real gap, not a
simplification:
1. **Static "checked cost" estimation**, run once when a CRD's
   `x-kubernetes-validations` rule (or a policy's own CEL rule) is first
   accepted, *before* it's ever evaluated against real data: walks the
   compiled CEL AST alongside the structural schema, computing a
   worst-case cost bound from the schema's own `maxItems`/`maxLength`/
   `maxProperties` (falling back to the `Max*Size` constants above when
   a field has no such bound) — a rule whose worst case could never fit
   the budget is rejected at CRD-acceptance time, a real `422`, not
   discovered lazily on the first CR that happens to trip it.
2. **Runtime cost accounting** during actual evaluation against a real
   object: an accumulator charged for each operation (a function call, a
   comprehension iteration, ...), checked every `CheckFrequency`
   iterations inside a loop, aborting the evaluation the moment the
   budget is exceeded — this is what actually stops a pathological real
   input (not just a pathological *rule*) from consuming unbounded CPU.

*Phased plan, each phase a real, separately verifiable slice, same
"land the primitive, wire it later" discipline every prior group has
used*:
1. **Done.** `cel_ext::eval_bool` — a pure `cel::Program::compile`/
   `cel::Context`/`.execute` wrapper against `serde_json::Value` bound
   variables (`self` for the value being validated, `oldSelf` on
   `UPDATE` — real upstream's own two well-known variable names for
   `x-kubernetes-validations`), no cost accounting yet, no k8s extension
   functions yet — proves the crate itself round-trips real expressions
   against real k8s-shaped data. Not reachable from any real request
   path yet (nothing calls it outside its own unit tests) — deliberate,
   see this section's own repeated warning on why.
2. **Partially done.** `eval_bool_with_deadline` — a real wall-clock
   deadline around evaluation, this build's own stand-in for real
   upstream's per-operation `PerCallLimit`/`RuntimeCELCostBudget`/
   `CheckFrequency` accounting, which needs interpreter-level hooks the
   `cel` crate doesn't expose at all (confirmed by reading `env.rs`/
   `context.rs` directly). Real upstream's own comments on those
   constants describe them in wall-clock terms too ("~0.1s"/"~1s"),
   so this bounds the same real property, not an unrelated
   approximation. **Named, honest limitation**: bounds how long the
   *caller* waits, not the CPU the spawned evaluation thread actually
   consumes — Rust has no safe way to forcibly kill an arbitrary running
   thread, so a pathological expression still runs to completion in the
   background; this alone does not bound how many concurrent evaluations
   can be in flight either (that's separate rate-limiting, Group M's own
   APF work). **Still not wired into any real request path** — a
   deadline alone isn't the same guarantee real upstream's own
   interruption provides, and this module's own doc comment says so
   again at the call site itself, not just here.
3. Static checked-cost estimation (layer 1) — real upstream's own
   defense against a malicious *rule* (not just malicious input), needed
   before `x-kubernetes-validations` can be accepted at CRD-creation
   time with any confidence its worst case is actually bounded.
4. Wire into Group K: `x-kubernetes-validations` evaluated in
   `server::rest::create`/`update`/`patch_persist`'s CRD branch, after
   pruning and required/type validation (`apiextensions::
   schema_validation`'s own existing two checks stay first — CEL rules
   commonly assume a field already passed basic structural validation).
5. Wire into Group J: ValidatingAdmissionPolicy/MutatingAdmissionPolicy,
   and `matchConditions` (its own separate, smaller budget) for webhooks
   and policy bindings.
6. Kubernetes' own CEL extension library (string/list helpers beyond
   base CEL, `isSorted`, quantity parsing, ...) and type-checking a rule
   against its declared schema at CRD-acceptance time (catching a rule
   that references a field the schema doesn't have, or compares
   incompatible types) — real upstream features this build doesn't need
   for a first working CEL path, named honestly as later phases rather
   than silently out of scope.

**L. Aggregation layer** — **a real design pass (2026-08-21), no code
yet**, same "ground it in real upstream source before writing anything"
discipline every big item this arc has used.

*What it really is* (`k8s.io/kube-aggregator`'s own
`pkg/apis/apiregistration/v1/types.go` +
`pkg/apiserver/handler_proxy.go`, fetched and read directly): an
`APIService` object (`spec.group`/`.version` naming the group-version it
takes over, `spec.service` naming a backing `Service` by
namespace/name/port, `spec.caBundle`/`.insecureSkipTLSVerify` for how to
trust it, `spec.groupPriorityMinimum`/`.versionPriority` for discovery
ordering) tells this build's own discovery/routing to stop answering a
group-version itself and instead reverse-proxy every request for it to
that backing Service — `metrics.k8s.io`/`custom.metrics.k8s.io` (metrics
server) is the real-world example almost every cluster actually runs
this for.

*Why this build is unusually well-positioned for the proxy half,
already*: unlike real kube-apiserver (which needs its own
`ServiceResolver` abstraction because a real cluster's Service->endpoint
mapping lives in etcd behind kube-proxy), this workspace already has a
real, live Service/EndpointSlice watch (Group D's own watch cache) *and*
a real Service-routing component (`crates/nodeproxy`) in the same repo —
resolving `spec.service.namespace`/`.name`/`.port` to a real routable
address doesn't need a new resolver abstraction invented from scratch,
just a read against data this build (or its sibling `nodeproxy`) already
has live. `proxy::http_client`/`proxy::client_tls` (Group N, already
landed for `pods/log`) are the other real, reusable primitive — an
`APIService` proxy is architecturally the same shape (dial a resolved
backend over TLS, relay the response unmodified), just resolving the
target from a Service instead of a Node.

*The availability controller* (`kube-aggregator`'s own
`pkg/apiserver/available_controller.go`): periodically health-checks
each `APIService`'s backing Service (or, for a `service: nil`
"local"/built-in group-version, is trivially always available) and
writes a real `Available` condition to `status.conditions` — discovery
merge (below) only ever advertises a group-version whose `APIService` is
currently `Available`, the same "don't advertise what you can't
actually serve" posture Group K's own `Established` gate already
established for CRDs.

*Discovery merge*: real upstream's own `/apis` response is the union of
every built-in group-version *and* every `Available` `APIService`'s
group-version, sorted by `groupPriorityMinimum`/`versionPriority` —
architecturally the same shape Group K's own `discovery::*_with_crds`
functions already are (a static table merged with a dynamically-fetched
set), likely reusable as a third merge input rather than a third parallel
implementation.

*Phased plan*: 1) **Done.** `APIService` as a real, generic-REST-served
resource — confirmed rather than assumed, and worth confirming turned
out to matter: `resolve_kind` already found `apiregistration.k8s.io/v1`
`APIService` (`vendor/openapi-spec/v3` has no group allowlist), but
`schema_for_gvk` had no compiled schema for it at all — `vendor/
refresh.sh`'s own proto-fetch glob (`staging/src/k8s.io/api*/generated.
proto`) misses `k8s.io/kube-aggregator`, `APIService`'s real staging
repo (it doesn't start with `api`), the exact same "looks known,
`UnknownResource` in practice" gap Group K's own CRD work found live
more than once. Fixed by vendoring `k8s.io/kube-aggregator/pkg/apis/
apiregistration/{v1,v1beta1}/generated.proto` directly (not a full
`refresh.sh` re-run, which would re-fetch the entire tree against
whatever `release-1.34` currently points to — real, unrelated drift a
one-resource fix has no reason to risk) and widening the script's own
glob for next time. Live-tested end to end
(`tests/apiservice_roundtrip.rs`) against a real `nodestore`:
create/get/list/update/delete all genuinely work, zero new application
code needed — the generic REST machinery really was already sufficient
the moment the schema existed.
2) **Partially done.** The availability controller's own *decision
logic* (`aggregator::availability`) — a faithful port of real upstream's
own two separate controllers (`github.com/kubernetes/kube-aggregator`'s
`pkg/controllers/status/{local,remote}`, fetched and read directly, a
genuinely separate GitHub repo from `kubernetes/kubernetes` this time,
not a staging package): `local` (`spec.service: null`) is always
`Available` (`Reason: "Local"`); `remote` runs a real pre-flight chain
before its own discovery-endpoint dial — service existence
(`ServiceNotFound`), listening on the configured port for a `ClusterIP`
service only (`ServicePortError`), `EndpointSlice` existence
(`EndpointsNotFound`), and at least one ready address on that port
(`MissingEndpoints`) — exact real `Reason` strings, not invented ones.
Pure, no I/O — not yet wired to a live reconciliation loop, and the
actual discovery-endpoint dial (real upstream's own "5 concurrent
`GET`s, any one succeeding is enough" check, `Reason:
"FailedDiscoveryCheck"`/`"Passed"`) is left to Phase 4, the natural home
for it since it needs the same dial primitive the reverse proxy itself
does. 4) The actual reverse proxy — resolve `spec.service` against live
Service/EndpointSlice data, dial via `proxy::http_client`'s
already-proven pattern, relay the response unmodified, matching
`pods/log`'s own "transparent proxy, no added behavior" posture.
3) Discovery merge — add `APIService`-sourced group-versions as a third
input alongside the static table and Group K's CRD-sourced ones.
**Real ordering correction, found while scoping the two**: unlike
Group K (where a CRD's own CRUD was already fully functional before its
discovery merge landed — CRDs were simply invisible to `kubectl`, never
broken), shipping *this* group's discovery merge (originally numbered
Phase 3, before Phase 4) ahead of the actual proxy dispatch would make
`kubectl api-resources` advertise a group-version nothing in `handle()`
actually routes anywhere — a real, user-visible lie, not a harmless
staging step. Phase 4 has to land first (or atomically with) Phase 3,
the reverse of every other group's own "land the primitive, wire it
later" — kept as list item "3)" for continuity with the numbering
elsewhere in this doc, but treat the *actual* build order as 1, 2, 4,
3.

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
just a bare `200`. `/metrics` is real too now (`server::metrics`, a
scoped port of real upstream's own `apiserver_request_total` counter,
`k8s.io/apiserver/pkg/endpoints/metrics`): every completed request is
recorded under `(verb, resource, code)` — a deliberately narrowed label
set next to real upstream's own nine (`verb`/`dry_run`/`group`/
`version`/`resource`/`subresource`/`scope`/`component`/`code`), the
three that answer the practically useful "what's erroring"/"what's
being hit hardest" questions without the cardinality cost of the full
set this early in the crate's metrics story. Rendered as real
Prometheus text exposition format by hand (no metrics crate dependency,
mirroring `crates/nodelet/src/server/prom_metrics.rs`'s own established
`push_metric`-shaped approach, sorted output so repeated scrapes diff
cleanly), one process-wide `Mutex<HashMap<...>>` counter table — this
crate's own request rate never remotely approaches where a real
lock-free registry would matter. `apiserver_request_duration_seconds`
(a histogram) and everything else in that real metrics package
(`apiserver_current_inflight_requests`, `apiserver_watch_events_total`,
...) are **not ported**, named honestly as separate, larger pieces of
work. **APF (FlowSchema/PriorityLevelConfiguration queueing) is
started**: `flowcontrol::flow_schema` ports real upstream's own
`FlowSchema` matching (`pkg/util/flowcontrol/rule.go`, fetched and read
directly) — `matches_flow_schema`/`matches_policy_rule`/`matches_subject`
(all three real subject kinds, `User`/`Group`/`ServiceAccount`,
including the `ServiceAccount` wildcard-name case's own real
namespace-only prefix check, kept as a genuinely separate function from
the exact-name matcher, same as real upstream) and real resource/
non-resource rule matching (verb/apiGroup/resource/namespace, and the
non-resource URL's real trailing-`*` prefix semantics). `select_flow_schema`
ports `apihelpers.FlowSchemaSequence`'s own real sort order — lowest
`matchingPrecedence` wins (defaulting to real upstream's own `1000`
when unset), ties broken by lexicographically smaller name.
`flowcontrol::resolve::select_for_request` is the storage-backed half:
lists real `FlowSchema`s, selects the governing one, fetches its
referenced `PriorityLevelConfiguration` by name — **wired into
`server::listener`'s `handle_with_audit`**, setting the real
`X-Kubernetes-PF-FlowSchema-UID`/`X-Kubernetes-PF-PriorityLevel-UID`
response headers (`k8s.io/api/flowcontrol/v1/types.go`'s own
`ResponseHeaderMatchedFlowSchemaUID`/
`ResponseHeaderMatchedPriorityLevelConfigurationUID` constants, fetched
and read directly) on every response that reaches a storage connection.
Fails open (no header, never a blocked/delayed request) on any
resolution failure. **Still no actual queuing/limiting** — every request
still runs at full priority, just correctly labeled; that
concurrency-limiting half of real APF (fair queuing, seat borrowing) is a
genuinely separate, larger undertaking, named honestly as not started,
along with the `distinguisherMethod` computation (meaningless without
queuing) and the two mandatory bootstrap `FlowSchema`s real upstream
always synthesizes (Group O's job).

**N. Streaming and proxy subresources** — **`pods/log` is a genuine live
proxy now, wired end to end.** `proxy::pod_log` ports real upstream's own
`pods/log` target resolution (`pkg/registry/core/pod/strategy.go`'s
`LogLocation`/`validateContainer`, plus the node connection-info
resolution `pkg/kubelet/client/kubelet_client.go`'s
`NodeConnectionInfoGetter.GetConnectionInfo` performs — preferred
address-type walk, real upstream's own default order `Hostname,
InternalDNS, InternalIP, ExternalDNS, ExternalIP` from
`cmd/kube-apiserver/app/options/options.go`, plus the
`daemonEndpoints.kubeletEndpoint.port`-or-default-10250 port fallback —
all fetched and read directly). Container defaulting (single-container
pods default automatically, `containers` + `initContainers` combined,
matching real upstream's own `AllFeatureEnabledContainers` visit) and
explicit-container validation both faithfully ported, real per-error
variants for "no default container"/"unknown container"/"pod not
scheduled"/"no node address" rather than one generic failure, each mapped
to its own HTTP status in `server::listener`'s dedicated `pods/log`
dispatch branch.

The credential problem this section previously named as unsolved is
solved by a different mechanism than the one it anticipated: rather than
nodeapiserver satisfying nodelet's bearer-token `TokenReview`
authenticator, `proxy::client_tls` builds a real `rustls::ClientConfig`
that dials nodelet's mTLS listener directly — insecure-by-default server-
cert verification (real upstream's own posture when no
`--kubelet-certificate-authority` is configured, `KubeletClientConfig`'s
`transportConfig()` sets `cfg.TLS.Insecure = true` whenever `!cfg.HasCA()`,
mirrored faithfully by `AcceptAnyServerCert`, which still cryptographically
verifies the handshake signature) plus an optional client certificate
(`NODEAPISERVER_KUBELET_CLIENT_CERT_FILE`/`_KEY_FILE`, raw DER matching
`crates/nodelet/src/server/tls.rs`'s own convention) that authenticates
via nodelet's `NODELET_CLIENT_CA_FILE` x509 path instead of a
`TokenReview` round trip — the same one shared client identity real
`kube-apiserver` itself presents to every kubelet via its own single
`--kubelet-client-certificate`, not unique per node. `proxy::http_client`
does the actual dial, reusing `crates/nodelet/src/server/exec.rs`'s own
proven low-level `hyper::client::conn::http1::handshake` pattern with a
TLS layer wrapped around the TCP stream first. `server::listener::run`
builds the `rustls::ClientConfig` once at startup (falling back to no
client identity on a misconfigured cert/key pair, logged rather than
fatal) and threads it through every connection; `handle()`'s own
`pods/log` branch fetches the pod, fetches its node, resolves the target,
dials nodelet, and relays its response — status, headers, and a still-
streaming body for `follow=true` — back completely unmodified, gated by
the same `enforce_rbac` RBAC check every other verb branch uses (`get`
on `pods/log`, a distinct resource from plain `pods` for RBAC purposes,
matching real upstream's own subresource-is-a-separate-resource rule). A
dial failure surfaces as a real `502`, not a `500` — the fault is
nodelet/the network, not this process.

exec/attach/port-forward (would reuse `crates/nodelet/src/server/
exec.rs`'s proven raw-upgrade-splice pattern, which this crate's `pods/
log` wiring doesn't need for a plain GET) and node/service proxy
subresources remain entirely unstarted, though they'd reuse the same
`client_tls`/`http_client` primitives.

**O. Cluster bootstrap — the k3s replacement half** — **not started, and
deliberately not `nodeapiserver`'s own code** (decided 2026-08-21, before
any of it was written): cluster PKI generation (CA, serving cert, SA
signing keypair, per-component client certs, kubeconfig emission), the
~90 `system:` ClusterRoles/Bindings from upstream's `bootstrappolicy`,
the `kubernetes` default Service + endpoint reconciler, and CoreDNS +
flannel manifests moved into `deploy/` don't belong inside the API
server binary itself — real upstream doesn't put this logic in
`kube-apiserver` either (it's spread across cluster-provisioning tooling
outside the binary). This build's equivalent is its own separate crate/
component — a `clusterbootstrap` app, forked into its own long-lived
integration branch the same way `nodeapiserver` itself was, following
the established component pattern (`deploy/lib/components.sh`'s table +
a `notk8s` applet — `components.sh:6` and `deploy/measure.sh:98` already
name `nodeapiserver` in anticipation, `clusterbootstrap` needs the same
treatment when its own branch starts). `deploy/setup-control-plane.sh`
still needs rewriting to stop installing k3s entirely once both
`nodeapiserver` and `clusterbootstrap` exist — that wiring is
`clusterbootstrap`'s own job, not folded back into `nodeapiserver`.

## Final acceptance

A cluster bootstrapped by `./deploy/bootstrap-source.sh --with-cri` with **no
k3s installed at all**, passing the full unfiltered `test-e2e.sh` suite
(~142 tests) including the real CSI and DRA reference drivers from
`deploy/lib/e2e-full-setup.sh`. Only then does `nodeapiserver` merge to
`main`, per `CLAUDE.md`'s merge protocol and the "no partial multi-phase
work" standing rule — satisfied here by merging into this integration branch
group-by-group and reserving `main` for the completed arc.
