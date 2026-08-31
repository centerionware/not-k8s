# nodeapiserver — design

| GROUPNAME | STATUS | COMPLETED |
| --- | --- | --- |
| Phase 0 — prerequisites | done | 2/2 |
| A — Vendoring + build-time codegen | done | 5/5 |
| B — Wire formats | done for current scope | 7/7 |
| C — Storage over nodestore | done for current scope | 7/7 |
| D — Watch cache | done for current scope | 7/7 |
| E — Generic server, handler chain, and REST | in progress | 10/12 |
| F — Scheme, conversion, defaulting, and validation | in progress | 8/10 |
| G — Patch and Server-Side Apply | in progress | 6/7 |
| H — Authentication | done for current scope | 7/7 |
| I — Authorization | in progress | 6/7 |
| J — Admission | in progress | 14/15 |
| K — CustomResourceDefinitions | done for current scope | 7/7 |
| L — Aggregation | done for current scope | 4/4 |
| M — APF, audit, and observability | in progress | 8/9 |
| N — Streaming and proxy subresources | done for current scope | 5/5 |
| O — nodebootstrap integration | done for current scope | 1/1 |

For in-progress groups, the denominator includes each explicitly named
remaining work item in the live sections below; “done for current scope”
counts the currently supported scope and may still name broader follow-up
compatibility work.

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

For an in-progress group, the denominator includes each explicitly tracked
remaining work item; “done for current scope” means the documented scope is
complete while compatibility extensions remain.

## Current status snapshot

This snapshot is checked against `origin/nodeapiserver` at `efa2b3a1` on
2026-08-31. It describes what is integrated on that branch; open child PRs
are not counted until they merge. The detailed sections below remain the
explanation of each boundary.

| Area | Status | What remains before this area is complete |
| --- | --- | --- |
| Phase 0 prerequisites | **done** | The workspace uses `k8s-openapi` v1_34 and the related dependency migration is integrated. |
| A. Vendoring/codegen | **done** | Packaging and bootstrap wiring are tracked under O. |
| B. Wire formats | **done for current scope** | Generic protobuf/JSON/YAML, Table, partial-object support, common built-in printers, and CRD additional printer columns are integrated; less-common per-resource printers and wire edge cases remain. |
| C. Storage | **done for current scope** | etcd storage, encryption providers, stale-value key rotation/rewrite, and full read/write/transaction/watch wiring are integrated. |
| D. Watch cache | **done for current scope** | Built-in resources are boot-cached and CRD cache creation/removal and lifecycle refresh are integrated; remaining cache work is compatibility hardening. |
| E. Server/REST | **in progress** | The generic verbs, watches, status paths, discovery, and OpenAPI endpoints are present; remaining compatibility edges and internal dispatcher cleanup remain. |
| F. Scheme | **in progress** | Conversion, structural validation/defaulting, published OpenAPI-local constraints, quantities, and much CEL support are present; the remaining per-kind and CEL compatibility surface is substantial. |
| G. Patch/SSA | **in progress** | JSON/merge/strategic patch, CRD-aware Server-Side Apply, ordinary-write managed-fields tracking, and status-subresource field exclusion are integrated; less-common managed-fields edge cases remain. |
| H. Authentication | **done for current scope** | Static tokens, service-account tokens, x509, OIDC, anonymous-auth configuration, TokenReview, and authentication-file reload are integrated; structured anonymous diagnostics and some upstream OIDC diagnostics remain. |
| I. Authorization | **in progress** | RBAC, node authorization, review APIs, and the authorization webhook path are present; remaining upstream authorizer behavior and compatibility coverage remain. |
| J. Admission | **in progress** | The implemented built-ins (including DefaultIngressClass and StorageObjectInUseProtection) and validating/mutating policies are wired; the generic plugin registry/order, remaining built-ins, and remaining typed CEL compatibility edges remain. |
| K. CRDs | **done for current scope** | CRD CRUD, schema behavior, status subresources, discovery, conversion projection, proactive lifecycle cache refresh, REST/watch conversion webhooks, and storage-version schema revalidation are integrated; multi-version storage migration and remaining conversion edge cases remain. |
| L. Aggregation | **done for current scope** | The standard front-proxy identity and HTTP/1.1 upgrade path are integrated; uncommon transport details remain. |
| M. APF/audit/observability | **in progress** | Audit stages, health, live-storage readiness, full request metric labels, bounded APF plumbing, flow distinguishers, shuffle-sharded queues, seat borrowing, one-second sampled inflight gauges, size-based audit-log rotation, bounded webhook delivery, and policy-selected request/response object capture are present; remaining observability refinements remain. |
| N. Streaming/proxy | **done for current scope** | Pod log/exec/attach/port-forward and node and Service proxy subresources are integrated; uncommon proxy transport details remain. |
| O. nodebootstrap integration | **done for current scope** | The nodebootstrap path defaults to nodeapiserver and can explicitly select upstream for comparison; nodebootstrap's own bootstrap features are tracked separately. |

Branching: `nodeapiserver` is a long-lived integration branch (pushed to
`origin/nodeapiserver`). Every group below is its own sub-branch and PR
**into `nodeapiserver`**, not `main` — see `APISERVER_PLAN.md`'s
"Verification" section. `main` takes this work only once, in one PR, once a
cluster boots with **no k3s installed** and the full unfiltered
`test-e2e.sh` suite passes.

**Working on this branch, real gotchas hit repeatedly:**
- **`git fetch origin nodeapiserver` right after every merge, before
  branching the next slice off it.** Skipping this once produced a PR
  whose diff showed already-merged files as brand new — caught by
  `git branch --show-current` before pushing, fixed with a rebase. Also
  confirm that command shows a sub-branch, not `nodeapiserver` itself,
  before a slice's first commit — it's easy to commit straight onto the
  integration branch by accident mid-session.
- **Dispatch `quick-check.yml -f components=nodeapiserver` for the
  iterate/verify loop**, not `build.yml` — much faster, and doesn't waste
  compute on this repo's other crates. `build.yml`'s own per-crate loop
  is hardcoded and has, at least once, silently never included
  `nodeapiserver` at all (a genuinely green `build.yml` run that proved
  nothing about this crate) — `quick-check.yml` is what actually caught
  that.
- **The commit-message lint (`CONTRIBUTING.md`) checks every individual
  commit in a PR, not just the final squash title** — a 100+ char header
  buried three commits back in an otherwise-fine PR still fails CI. If a
  reword is needed and `git rebase -i` isn't available in the sandbox
  (it commonly isn't), do it non-interactively: `git branch -f
  <tmp> <base>`, `git checkout <tmp>`, `git cherry-pick --no-commit
  <sha> && git commit -m "<fixed message>"`, cherry-pick the rest in
  order, then move the real branch to `<tmp>` and
  `git push --force-with-lease`.

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

**Phase 0 — prerequisites** — **done**. The workspace now uses
`k8s-openapi` v1_34 (additive per `APISERVER_PLAN.md` finding 10 — zero
field removals across 572 shared structs), and `crates/nodescheduler` now
uses `cel = "0.14"` with its migrated dynamic-resource execution path and
regression tests.

**A. Vendoring + build-time codegen** — **done**. `crates/nodeapiserver`
exists; `vendor/refresh.sh` vendored `release-1.34`'s 64 openapi-spec v3
files + 72 `generated.proto` files, recording the ref in `vendor/REF`.
`build.rs` (`build/proto_parse.rs`, `build/openapi_parse.rs`) emits the
protobuf field-number table, the SMP/SSA schema metadata table (now
including `ref_schema` — the `$ref`'d schema a field's value is shaped
like, added for Group G's Strategic Merge Patch recursion), and the
discovery GVK map; `src/codegen.rs` builds runtime indexes over them plus
the proto-style<->openapi-style message-name resolver (`resolve_message_ref`)
Group B's codec depends on. The codegen output is integrated into the
nodeapiserver build; packaging and bootstrap integration are tracked under
Group O.

**B. Wire formats** — **done for the current scope**. `codec::protobuf` is a generic
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
`RemainingItemCount` carried through from the List's own metadata. The
verified built-in set now includes the standard Pod, Deployment, ReplicaSet,
StatefulSet, DaemonSet, Service, Node, and Namespace printers, including
their common wide-mode columns. CRD `additionalPrinterColumns` are resolved
from the negotiated served version and evaluated for dynamic Table responses,
with the implicit Name column and upstream-compatible default Age column when
no custom columns are declared. Named honestly: real kube-apiserver's much
larger *per-type* printer set is only partly started — resources without a
registered printer get the generic table, same as a fresh CRD does in real
kube-apiserver until it earns its own printer. **Now actually wired into
`GET`/`LIST`** (`server::listener`
checks `Accepted::wants_table()` from the request's own `Accept` header,
resolves the matching CRD version when needed, and runs the response through
the appropriate table converter when set) — a real gap
found this session: the converter had been landed and correctly
documented for a while, but nothing in `server/` ever called it, so a
real `kubectl get pods` against a live nodeapiserver got raw JSON
instead of the columnar `Table` output every `kubectl get` actually
negotiates by default. The standard
`meta.k8s.io/v1` `PartialObjectMetadata` and `PartialObjectMetadataList`
representations are now served for negotiated `GET`, `LIST`, and `WATCH`
responses; per-resource printer behavior remains separate work.

**C. Storage over nodestore** — **done for current scope**. `storage::client::StorageClient`
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
(`storage::encryption`): `Identity`, AES-256-GCM, AES-256-CBC,
Secretbox, and Kubernetes KMS v1/v2 providers, plus the
generic `PrefixTransformers` composition every provider list (including
per-key rotation) uses — a faithful port of upstream's own
`storage/value` package (`transformer.go`'s `prefixTransformers`,
`encrypt/identity/identity.go`, `encrypt/aes/aes.go`'s `gcm`/`cbc` types), fetched
and read directly. Real envelope format confirmed against upstream:
`k8s:enc:aesgcm:v1:<key-name>:<nonce><ciphertext+tag>`,
`k8s:enc:aescbc:v1:<key-name>:<iv><padded-ciphertext>`, and
`k8s:enc:secretbox:v1:<key-name>:<nonce><tag+ciphertext>`. Secretbox uses
the upstream-compatible 24-byte nonce and 32-byte key. KMS v1 and v2 use
the upstream gRPC plugin protocol over a configured endpoint, with v1's
length-prefixed encrypted DEK envelope and v2's serialized
`EncryptedObject`/HKDF-seed envelope.
**`EncryptionConfiguration` YAML parsing now exists too**
(`storage::encryption_config`, fetched and read directly against
`staging/src/k8s.io/apiserver/pkg/apis/apiserver/v1/types_encryption.go`):
parses the real document shape (`resources: [{resources: [...],
providers: [...]}]`) into a resolvable set of `encryption::
PrefixTransformers`, one per resource entry, matched by real upstream's
own resource-name/wildcard rules (`secrets`, `<resource>.<group>`, `*.`,
`*.<group>`, `*.*`) with real "earlier entries take precedence"
first-match-wins resolution. `aesgcm`/`aescbc`/`secretbox`/`identity`/`kms`
build against their real providers; KMS endpoints remain lazy until an
encrypted read or write.
`NODEAPISERVER_ENCRYPTION_CONFIG_FILE`
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
`persist_update`). AES-GCM uses the object's own etcd key as authenticated
data, matching real upstream's own `dataCtx.AuthenticatedData()` convention;
AES-CBC follows upstream's unauthenticated CBC provider and ignores that
argument; Secretbox does likewise. A resource with no matching entry in the
loaded config is
written/read as-is, unchanged from before this wiring existed — encryption
is opt-in per resource, never a blanket switch. The real `stale` flag
`transform_from_storage`
returns (upstream's own "this was encrypted under a non-primary key,
rewrite it with the current one next write" signal) is now honored on
every nodestore-backed read. The rewrite is guarded by an MVCC compare
against the revision that was read, so concurrent updates win and a
failed rotation never breaks the successful read.

**D. Watch cache** — **done for current scope**. `cacher::store::WatchCache` is the
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
`mvccpb::Event`'s own doc comment; only an explicit progress response with a
newer header revision becomes a `Bookmark`; the initial `created: true`
acknowledgement does not advance the resource cache), and `reflect()` is
the reconnect loop — LIST, WATCH, and on any failure or stream end,
relist and try again forever, the same "never give up" posture a real `client-go`
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
resource)`, and `remove()` stops and forgets one. The listener registers
every built-in resource from the generated discovery table at boot, then
reconciles CRD-defined caches from the live CRD cache as versions become
Established/served or are removed. A first watch still has a live-discovery
fallback if that asynchronous reconciliation has not caught up. `server::rest::get` now
*can* consult a `SharedCache` if
handed one (`cache.get(key)`, a hit skips nodestore entirely; a miss
always falls through to a real `Range` rather than trusting the cache's
absence of an entry to mean "not found," since a not-yet-synced or
never-registered cache is indistinguishable from a genuinely empty one
using only what `SharedCache` exposes today — a pure latency win on the
hit path, never a correctness risk on the miss path). `GET`/`LIST` use
that cache for every built-in GVR and for dynamically reconciled CRD GVRs.
**Per-Kind `SelectableFields` is now enforced**:
built-in registries accept their verified metadata and resource-specific
fields, while CRDs accept only the universal metadata fields; unsupported
field paths return `400 BadRequest` instead of silently matching no objects.
CRD GVRs cannot be registered before their definitions exist; Group K's live
CRD-cache reconciler registers and retires those caches as the definitions
change, with a first-watch fallback for the startup race.

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
as `cache: None` would. `server::listener` uses the same registry for every
built-in resource and reconciles dynamically served CRD GVRs from the live
CRD cache; the first-watch fallback remains only for the startup race.

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
now actually binds and serves. Its resource handler dispatches the landed
REST verbs below; unsupported paths return Kubernetes-shaped errors rather
than a bring-up echo. `server::discovery` builds the `/api` (`APIVersions`)
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
The parser still skips only the deprecated `/watch/`-prefixed path family.
Subresources are emitted as their own discoverable entries (`pods/status`,
`pods/log`, `pods/exec`, and the other single-item subresource paths), with
the advertised `connect` verb retained for streaming subresources.
`singularName` uses real kube-apiserver's own RESTMapper default (lowercased
kind) since no per-type override table is vendored; standard built-in
`shortNames` and CRD-declared `spec.names.shortNames` are emitted, while
standard built-in categories and CRD-declared `spec.names.categories` are
emitted too. **Now wired into the listener's actual routing**:
`server::listener`'s `route_discovery` (pure, unit-tested apart from the
async handler) dispatches all five non-resource discovery routes (`/api`,
`/api/{version}`, `/apis`, `/apis/{group}`, `/apis/{group}/{version}`) to
`server::discovery`'s real builders, with a genuine `404` (a minimal
`Status` body — `kind`/`apiVersion`/`status`/`message`/`reason`/`code`,
the exact subset `client-go`'s own `errors.NewNotFound` decoding path
reads, not a full `Status` type) for an unknown group/version rather than a
false-success response. A
resource-shaped path (`/api/v1/namespaces/
default/pods`) is handled by the resource dispatch described below, not by
the discovery-only route.
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
`None` rather than stopping the listener from serving discovery, which needs
no storage at all). Resource requests return `503 ServiceUnavailable` while
that backend is unavailable, and unknown non-resource paths return a
Kubernetes `404` rather than a bring-up echo. The listener still
clones it per connection (`StorageClient` wraps a cheap-to-clone
`tonic::transport::Channel`, same posture `cacher`'s own driver takes).
Named honestly, not overclaimed: `GET`/`LIST` consult the registered
`cacher::store::SharedCache` for synchronized, non-paginated reads and
fall back to nodestore for cache misses, unsynchronized caches, paginated
reads, or callers without a cache. `WATCH` is served from the same cache;
request bodies are bounded by `NODEAPISERVER_MAX_REQUEST_BODY_BYTES` and
return a Kubernetes `413 RequestEntityTooLarge` before decoding when the
limit is exceeded; the default is 3 MiB, matching kube-apiserver's default.
no subresources are included in these generic read paths.
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
`Content-Type` (JSON/YAML and Kubernetes' `k8s\0`-framed protobuf envelope
for built-in resources; CRD bodies use the envelope's raw JSON because they
have no generated schema). `dryRun=All` is honored for create, update,
patch, apply, and status-subresource writes, performing the same resolution,
admission, validation, defaulting, and
conflict checks without persisting the object. Real,
distinct `Status` responses per outcome: `201` created, `409
AlreadyExists` (lost the create race), `422 Invalid` (validation
failures, with both the aggregate message and structured
`details.causes[]` field/reason entries), `400` when neither `metadata.name` nor
`metadata.generateName` is supplied, or for a namespace mismatch between
the body and the URL. When `generateName` is supplied, the server appends
a collision-resistant suffix before validation and persistence.

`server::rest::delete` (single-object `DELETE`) is real too: it reads the
object, honors `resourceVersion`/`uid` preconditions from
`metav1.DeleteOptions.Preconditions`, and removes it with an MVCC-guarded
transaction so a concurrent update cannot invalidate the check. Objects with
finalizers are marked with `metadata.deletionTimestamp` instead; they remain
visible until the last finalizer is removed, at which point the removal is
also MVCC-guarded. It returns the deleted or terminating object, matching
real upstream's synchronous-delete shape.
Named honestly as the current scope: no `propagationPolicy`
(Foreground/Background/Orphan — no owner-reference garbage collector
exists to orphan from or cascade through in the first place).

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

**Milestone: `WATCH` is real end to end**, for any registered built-in or
discovered CRD resource: a `GET .../pods?watch=true` (or the
`/api/v1/watch/pods` legacy path form) now
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
a gap. A resource with no registered cache returns a real Kubernetes error,
the same posture as `GET`/`LIST` already had for an uncached resource.
**`WATCH` is now RBAC-gated too**
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

The standard watch options are also honored: `allowWatchBookmarks=true`
opts into cache bookmark events, and a positive `timeoutSeconds` bounds an
otherwise idle stream. Malformed values return `400`; the live timeout
behavior is covered by `test_nodeapiserver_honors_watch_options`.

**`PATCH` is real too now** (`rest::patch_prepare`/`patch_persist`,
reusing Group G's already-landed `patch::json_patch`/`merge_patch`/
`strategic_merge`): the real `Content-Type` selects the patch kind
(`application/json-patch+json`/`application/merge-patch+json`/
`application/strategic-merge-patch+json`; when `Content-Type` is omitted,
built-ins default to strategic merge and CRD-defined resources default to
JSON merge patch —
`rest::patch_kind_for_content_type`, a real `415` for anything else,
Server-Side Apply's own `application/apply-patch+yaml` deliberately not
recognized by this function — `server::listener` routes it into
`rest::server_side_apply` instead, a wholly separate real code path now that
Group G's SSA arc has landed; see that group's own section), applied to
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
pre-deletion `List`, real upstream's own response shape. Each matched object
now passes configured admission before deletion, matching upstream's
per-object delete validation callback. **Named, honest simplifications**:
real upstream deletes with a worker pool and paginates
the list internally; this port deletes sequentially and lists in one
shot, while the ordinary `list` path itself now supports pagination.

**Every real, generic REST verb this build knows about is now wired
in** — `GET`/`LIST`/`CREATE`/`DELETE`/`UPDATE`/`PATCH`/`DELETECOLLECTION`,
plus a real streaming response for `WATCH`. **The authorization stage is
now centralized**: after authentication and APF have completed, one
listener gate evaluates every resource request before any admission or
REST-specific handler, including PATCH, status, deletecollection, watch,
token, aggregation, and node/service/pod proxy routes. Virtual access-review
resources remain outside that gate because they answer authorization
questions rather than authorize a resource mutation. The request stages
already execute in the required order (authn -> authz -> APF -> admission ->
REST); the remaining cleanup is to hide the listener's explicit admission
selection behind a reusable dispatcher without changing that ordering.
`/openapi/v2` is now served as a Swagger 2.0
document derived from the same vendored resource schemas and paths as
`/openapi/v3`; the nodeapiserver target e2e check verifies it is populated.
The throwaway e2e rig described above should land as part of this group,
not after it.

The scheduler's core `pods/binding` subresource is also live: a
`POST /api/v1/namespaces/{namespace}/pods/{name}/binding` validates the
Binding target and optional UID/resourceVersion preconditions, atomically
sets `spec.nodeName`, merges binding labels/annotations, and records the
`PodScheduled=True` condition. This is the REST operation the replacement
scheduler uses instead of a full Pod update, and it has a nodeapiserver-only
e2e check.

The built-in workload `scale` subresources are live too: `GET` returns the
virtual `autoscaling/v1 Scale` for Deployments, ReplicaSets, StatefulSets,
and ReplicationControllers; `PUT` and JSON/merge/strategic `PATCH` update
only the parent's `spec.replicas` with normal resource-version conflict
checks. This is the Scale API used by `kubectl scale` and HPA clients, and
has a nodeapiserver-only e2e check.

The core Pod `ephemeralcontainers` subresource is live as well. `GET` returns
the complete Pod, while `PUT` and JSON/merge/strategic `PATCH` retain only the
requested `spec.ephemeralContainers` change. Existing ephemeral containers
are append-only and immutable, and invalid names, targets, or fields are
rejected before the normal optimistic-concurrency write. The existing
ephemeral-container e2e test exercises the real container startup path; a
nodeapiserver-only check also verifies that unrelated Pod changes sent to this
subresource are reset.

`GET` and `LIST` also honor a positive `resourceVersion` by reading a
consistent nodestore MVCC snapshot. These requests bypass the live watch
cache, matching the snapshot semantics clients need when relisting after a
watch interruption.

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
exactly like `update_status`'s own `PUT` semantics. CRD status writes now
run the matched version's structural pruning, required/type, and local
constraint validation before persistence; built-in status strategies remain
generic because their per-type rules are hand-written upstream. **Named,
honest scope narrowing**: no Group J admission runs on either
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
The same watch path restores the scoped top-level `kind` and `apiVersion` on
decoded Added/Modified/Deleted objects: those fields belong to the stored
protobuf envelope rather than the message body, and clients such as flannel
require them to decode watch events as Kubernetes objects.

**F. Scheme: conversion, defaulting, validation** — **in progress**. The
largest handwritten chunk. `scheme::defaulting::apply_defaults(schema, value)`
recursively fills a JSON object's absent fields from Group A's `FIELD_META`
table, now carrying each property's real vendored `"default"` value
(`default_json`, added alongside this module). An absent object-typed field
first materializes from its own structural default (usually `{}`, which is
what tells the recursion to keep going into that schema's own fields) via
`ref_schema`, then gets that schema's own defaults applied — proven with a
genuinely two-levels-deep case (`Container.ports[].protocol`).
`apply_builtin_defaults` now supplies the conditional core defaults that
cannot be represented in OpenAPI field metadata: Pod and PodTemplateSpec
restart/DNS/security/service-link defaults, image pull policy and termination
message defaults, probe defaults, resource-limit-to-request copying, volume
and projected-token defaults, Service type/session-affinity/traffic-policy
and target-port defaults, plus the corresponding common Secret, ConfigMap,
PV, PVC, Endpoints, Namespace, ReplicationController, and apps workload
template defaults. It is wired into create, update, patch, and apply before
conversion and is covered by a nodeapiserver-only live Pod round trip.
Published OpenAPI-local constraints are now applied generically as well:
`scheme::validation::validate_openapi_constraints` resolves the built-in
GVK's vendored schema and reuses the CRD constraint walker for formats,
enums, ranges, lengths, patterns, uniqueness, and `allOf`/`anyOf`/`oneOf`/
`not` alternatives on create, update, patch, and apply. The generic write
paths also enforce the universal Kubernetes metadata grammar for
label/annotation keys and values and finalizers. This closes the schema-local
and universal-metadata part of the generic validation gap; cross-field and
per-kind semantic rules remain separate work, rather than pretending this is
a full replacement for upstream's hand-maintained
`pkg/apis/*/v1/defaults.go` and validation packages.
`scheme::validation::validate_required(schema, value)` lands the
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
happened to lex it. Named honestly: these two generated-table functions are
still only structural (presence + kind). The schema-local constraints are
covered by the supplemental OpenAPI pass described above; cross-field and
other per-kind semantic validation remains hand-written Go upstream, with no
generic shortcut.

`cel_ext::type_check` now supplies the schema-aware CEL acceptance step that
the runtime `cel` crate does not provide: `self`/`oldSelf` are resolved against
the rule's own schema scope, undeclared structural fields and obvious
operator/member-function overload mismatches are rejected, and a validation
rule must have a boolean result. Kubernetes' opaque CEL extension values
(`Quantity`, IP, CIDR, URL, Semver, and named-format optionals) are declared
with their obvious receiver and argument types too. Root
`apiVersion`/`kind`/selected metadata fields are exposed as Kubernetes does
even when a CRD schema omits them; dynamic or preserved-unknown schema
sections remain permissive. The checker is wired into CRD `CREATE`/`UPDATE`
alongside syntax and static-cost checks.
It is intentionally a schema/type phase, not a replacement for runtime
evaluation: CEL overloads that remain dynamic at compile time still get the
existing runtime error behavior.

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

`scheme::conversion::to_version` now runs at the JSON response boundary for
stored objects served through another API version. Compatible fields are
projected through the requested version's vendored OpenAPI schema, including
nested `$ref` fields, arrays, and maps, so source-version-only fields are not
leaked to clients. Semantic shape changes remain explicit conversions: the
real autoscaling HPA v1/v2 CPU target is converted between v1's
`targetCPUUtilizationPercentage` and v2's Resource metric form (including
status). The small set of version pairs with hand-written semantic renames
remains separate work; a missing target schema fails open to the existing GVK
rewrite rather than silently dropping data.

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
each plugin's current real scope). The parser also backs the opaque
`kubernetes.Quantity` CEL value: `quantity()` now constructs one, and
`isInteger`, `asInteger`, `asApproximateFloat`, `sign`, `add`, `sub`,
`isLessThan`, `isGreaterThan`, and `compareTo` are registered with the
same runtime semantics as upstream's quantity library. The Rust CEL
runtime has no static type checker, so overload misuse is reported as a
runtime function error rather than rejected during compilation.

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
correctly, not just parent -> immediate child. The `$patch`/
`$setElementOrder`/`$deleteFromPrimitiveList` directives are supported for
built-in and CRD schemas. **All three
are now wired into a real `PATCH` verb** (Group E's own section has the
detail: `server::rest::patch`, selected by real `Content-Type`, real
optimistic concurrency, `namespace_lifecycle`/`LimitRanger` admission
both run on it).

**Server-Side Apply/`managedFields` is now real and reachable from a
request**, built on the same `FIELD_META` (`ref_schema` included) this
group's other patch logic reads — a full, faithful port of real
upstream's `sigs.k8s.io/structured-merge-diff/v6` (fetched and read
directly, no Rust crate to reuse, confirmed): `patch::fieldset` (the
`PathElement`/`Set` data structure, its real `fieldsV1` JSON wire shape,
`set_from_object`/`remove_items`/`ensure_named_fields_are_members`, and
`Set`'s own algebra), `patch::typed_merge` (the real merge, a deliberate
sibling of `strategic_merge` differing in two confirmed ways — deduplicated
set-list union, atomic-map wholesale replacement), `patch::typed_compare`
(the real diff, `{removed, modified, added}`), `patch::updater`
(`merge.Updater` itself: `update`/`apply_update`/`prune`/`apply` — real
conflict detection, pruning fields a manager stops claiming, with
version-aware per-manager comparisons in the Apply path), and
`patch::managed_fields` (the real
`metadata.managedFields[]` wire shape and its conversion to/from the
version-aware manager state `updater` operates on). `server::rest::server_side_apply`
wires all of this to real storage, and `server::listener` routes `PATCH`
with `Content-Type: application/apply-patch+yaml` into it
(`?fieldManager=` required, `?force=true` honored, a real `409 Conflict`
on an unresolved ownership conflict). **Create-on-apply is real too**: no
object at this key creates one through the same create-only-if-absent
`Txn` idiom `rest::create` uses, `updater::apply` run against an empty
`live` either way. `rest::apply_prepare`/`apply_persist`'s own split (the
same shape `patch_prepare`/`patch_persist` already has) lets both
`namespace_lifecycle` *and* `LimitRanger` admission run against the real
candidate object, matching the ordinary three-patch-kind `PATCH`
branch's own coverage exactly. REST and watch conversion webhooks already
convert CRD objects between served and storage versions. **Named, honest
scope remaining**:
`updater`'s compiled `FIELD_META` path still applies only to built-in
resources; CRD-defined resources use the matching runtime-schema SSA
implementation (`patch::crd_apply`) against each established version's
`openAPIV3Schema`, including managed-field ownership, conflict detection,
structural list/map merge behavior, pruning, and create-on-apply. The Apply
path preserves each manager's recorded API version, compares converted
live/candidate objects under that manager's own schema, and performs
cross-version pruning when a manager changes the API version it applies with,
including built-in version conversions and CRD conversion-webhook boundaries.
Both paths share the same optimistic-concurrency persistence behavior.
Runtime CRD managed-field sets are reconciled when a CRD schema changes
between granular and atomic map/list relationships, matching the upstream
schema-change ownership transition for the supported schema forms.
Main-resource managed-field reconciliation now applies the supported
server-managed field exclusion for status subresources: `status` is omitted
from main-resource ownership, conflict, and removal bookkeeping for built-ins
and runtime CRDs, while status writes retain their separate subresource
manager entry. This mirrors upstream's `IgnoredFields` behavior at the REST
strategy boundary.
Ordinary CREATE/PUT/PATCH and
status writes now also reconcile `metadata.managedFields` using the explicit
`fieldManager` or the request's `User-Agent`, with generated server metadata
excluded from ownership calculation.
Explicit and default patch-strategy selection both honor the directive set;
the default is strategic merge for built-ins and JSON merge patch for CRDs.
Less-common upstream managed-fields compatibility cases remain outside this
current scope.

**H. Authentication** — **complete for the supported authentication
paths**. `authn::x509::identity_from_der`
derives an `Identity{name, groups, uid, credential_id}` from a client
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
through to `server::listener::handle` and used for request authentication
and audit identity. **Authorization now
does check this identity too** (Group I's `enforce_rbac`, opt-in — see
that section below; this doc line used to say authentication had no
authorization to enforce against, stale since Group I landed).
`authn::self_review` is real too now — `SelfSubjectReview`
(`kubectl auth whoami`), wired into `server::listener` as its own `POST`
branch, purely reflecting whatever identity `x509` (or the anonymous
fallback) already produced into the real `UserInfo` shape, no new
authentication logic, never persisted (same virtual-resource posture
`authz::sar`'s review kinds established). `authn::service_account` is now
wired for the nodeapiserver bootstrap target: it signs ES256
projected/bound tokens from the cluster `sa.key`, serves the core
`serviceaccounts/token` TokenRequest subresource, validates
issuer/audience/lifetime, and answers authentication.k8s.io `TokenReview`
for nodelet's bearer-token webhook path. `authn::oidc` is now an optional
discovery-backed bearer-token authenticator: when
`NODEAPISERVER_OIDC_ISSUER_URL` and `NODEAPISERVER_OIDC_CLIENT_ID` are set,
the listener validates the issuer metadata, loads its JWKS, verifies
RS256/PS256/ES256 tokens, checks issuer/audience/expiry/required claims, and
maps configurable username and group claims. A configured CA bundle is used
for issuer requests, and a rotated JWKS is refreshed once on verification
failure. `NODEAPISERVER_ANONYMOUS_AUTH` now controls the upstream-compatible
boolean anonymous-authentication switch (enabled by default, matching
`--anonymous-auth=true`); disabling it returns `401 Unauthorized` for a
request with neither a client certificate nor a bearer token. The standard
`--token-auth-file` CSV shape is available through
`NODEAPISERVER_TOKEN_AUTH_FILE`; its username, UID, and groups flow through
request identity and `SelfSubjectReview`, and a changed file is reloaded on
the next authentication request while the last valid table remains active
during a malformed rotation. A real e2e probe exercises startup
authentication and in-place token rotation without restarting the service.
Structured anonymous-authentication conditions remain intentionally outside
this implementation's supported scope. The ServiceAccount signing key and
client-CA bundle now reload for new authentication operations/connections;
malformed rotations retain the last valid contents. Some upstream OIDC
diagnostic details remain outside the current scope.

**I. Authorization** — **in progress**. `authz::rbac` is the RBAC
rule-matching primitive — a faithful port of real upstream's own
`VerbMatches`/`APIGroupMatches`/`ResourceMatches`/`ResourceNameMatches`/
`NonResourceURLMatches` (`pkg/apis/rbac/v1/evaluation_helpers.go`) and
`RuleAllows`/`RulesAllow` (`plugin/pkg/auth/authorizer/rbac/rbac.go`),
fetched and read directly. Covers the real wildcard semantics
(`verbs`/`apiGroups`/`resources` `"*"`, the `*/status`-style subresource
wildcard, the trailing-`*` prefix wildcard `nonResourceURLs` supports,
and empty `resourceNames` meaning "every name") and the resource vs.
non-resource request split. `authz::subject` is the other half `DefaultRuleResolver` combines with
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
one pre-dispatch gate calls `resolve::rules_for` + `rbac::rules_allow` for
every resource route, including the generic CRUD verbs, PATCH/status,
deletecollection/watch, token and proxy subresources, with a real `403` on
denial, gated behind
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
resolution errors. `authz::node` now implements the Node authorizer's
node-identity checks, node/lease/CSINode ownership rules, field-selector
scoping for pod and ResourceSlice watches, and storage-backed relationship
checks for pods, Secrets, ConfigMaps, PVCs, PVs, ServiceAccount token
requests, ResourceClaims, and VolumeAttachments. It runs before RBAC on
the nodeapiserver target, so a broad legacy `system:node` binding cannot
bypass those object-specific checks. `admission::node_restriction` now
completes the upstream Node authorizer chain for body-sensitive operations
that authorization cannot inspect: node identity/name and restricted-label
checks, mirror-Pod ownership and API-reference checks, Pod status/eviction
relationships, ServiceAccount token bindings, node leases, CSINodes, and
ResourceSlices. The remaining authorization gap is webhook compatibility
hardening. An optional
`NODEAPISERVER_AUTHORIZATION_WEBHOOK_URL` delegates each parsed request to
an external `authorization.k8s.io/v1` `SubjectAccessReview` authorizer and
preserves its three decisions: `Allow` short-circuits the local Node/RBAC
chain, `Deny` returns `403`, and `NoOpinion` falls through to the next local
authorizer. Transport and malformed-response failures fail closed with
`503`; transient transport/5xx and rate-limit failures now use a small bounded
retry, while the remaining upstream diagnostic details are still outside the
current scope. Decisions are cached with separate bounded TTLs for allowed
and non-allowed results; `NODEAPISERVER_AUTHORIZATION_WEBHOOK_CACHE_AUTHORIZED_TTL`
and `NODEAPISERVER_AUTHORIZATION_WEBHOOK_CACHE_UNAUTHORIZED_TTL` configure
those lifetimes, and zero disables the respective cache. PKI primitives
(`rcgen`, `p256`, `x509-parser`, `pem`) are
already in-tree from `nodecontroller`'s CSR
group.

**J. Admission** — **in progress**. `admission::namespace_lifecycle` is a
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

`admission::chain::MutatingRegistry` now owns the ordered pure-mutator stage:
`DefaultTolerationSeconds` runs before ServiceAccount-name defaulting, and
new pure mutators can be registered without another direct listener call.
Storage-backed admission steps remain explicit because their I/O and failure
policy need request-specific handling.

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
specifies none of its own. When the `ServiceAccount` carries
`kubernetes.io/enforce-mountable-secrets: "true"`, it also enforces the
mountable-secret restriction for Secret volumes, container and init-container
`env[].valueFrom.secretKeyRef`/`envFrom[].secretRef` references, and pod
`imagePullSecrets`, against the `ServiceAccount`'s corresponding allowlists.
That opt-in check is off by default, matching upstream. Split the same pure-
decision/real-I/O-step way as `namespace_lifecycle`. The
`pods/ephemeralcontainers` subresource is served by its own Pod update
strategy; its separate ServiceAccount validation path now checks new
ephemeral containers' `env[].valueFrom.secretKeyRef` and
`envFrom[].secretRef` references against the annotated account's allowed
secrets before the update is persisted.

`admission::taint_nodes_by_condition` is the default-on, pure
`TaintNodesByCondition` mutation for core `Node` `CREATE` requests. It adds
the upstream `node.kubernetes.io/not-ready`/`NoSchedule` taint when absent,
preserves submitted taints, and is idempotent so node lifecycle reconciliation
can remove the taint after the node becomes Ready.

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

`admission::default_ingress_class` is mutating, `CREATE`-only — a faithful
port of real upstream's `DefaultIngressClass` plugin
(`plugin/pkg/admission/network/defaultingressclass/admission.go`, fetched
and read directly): an Ingress without `spec.ingressClassName` and without
the legacy `kubernetes.io/ingress.class` annotation gets the newest
cluster-scoped `IngressClass` carrying
`ingressclass.kubernetes.io/is-default-class: "true"`. Multiple defaults
use the real newest-`creationTimestamp`, name-ascending tie-break. No
default class, an explicit field, or the legacy annotation is a no-op. The
listener performs one live `LIST` of `networking.k8s.io/v1/ingressclasses`
before the candidate reaches the remaining admission stages.

`admission::storage_object_in_use_protection` is mutating, `CREATE`-only — a
faithful port of real upstream's `StorageObjectInUseProtection` plugin
(`plugin/pkg/admission/storage/storageobjectinuseprotection/admission.go`,
fetched and read directly): PersistentVolumes, PersistentVolumeClaims, and
VolumeAttributesClasses receive their standard protection finalizer when
created. The existing nodecontroller protection controllers remove those
finalizers once the objects are no longer in use. The mutation is applied to
the request candidate before the remaining admission stages and is covered
by unit tests for all three resource families, duplicate-finalizer handling,
and subresource exclusion.

`admission::runtime_class` is mutating and validating, `CREATE`-only for
ordinary Pods — a faithful port of real upstream's `RuntimeClass` plugin
(`plugin/pkg/admission/runtimeclass/admission.go`, fetched and read
directly): a named `RuntimeClass` is resolved from the cluster, its
`overhead.podFixed` is copied into the Pod, its scheduling `nodeSelector` is
merged with conflicts rejected, and its tolerations are appended without
duplicates. A Pod-supplied overhead must match the RuntimeClass's overhead,
or is rejected when no matching RuntimeClass overhead exists. The listener
performs the live RuntimeClass lookup and the pure module applies the
mutation/validation before the remaining Pod admission stages.

`admission::priority` is mutating for Pod `CREATE`/`UPDATE` and validating
for `PriorityClass` `CREATE`/`UPDATE` — a faithful port of real upstream's
`Priority` plugin (`plugin/pkg/admission/priority/admission.go`, fetched and
read directly): a Pod's named PriorityClass is resolved and its integer
priority/preemption policy are applied; a global-default class is selected by
the lowest priority value when the Pod names none; plugin-owned fields are
preserved across updates; and creating or updating a second global-default
PriorityClass is rejected. The listener performs the storage lookups while
the object rules remain pure and unit tested.

`admission::pvc_resize` is validating, `UPDATE`-only — a faithful port of
real upstream's `PersistentVolumeClaimResize` plugin
(`plugin/pkg/admission/storage/persistentvolume/resize/admission.go`, fetched
and read directly): a storage request may grow only when the existing claim
is Bound, its StorageClass is unchanged, and that StorageClass explicitly
allows volume expansion. The check runs for both full updates and patch
candidates after the patch has been materialized.

`admission::pod_node_selector` implements the namespace-annotation form of
the upstream `PodNodeSelector` plugin. A Pod created in a Namespace carrying
`scheduler.alpha.kubernetes.io/node-selector` gets those exact-match labels
merged into `spec.nodeSelector`; conflicting Pod labels and unsupported
selector syntax are rejected before persistence. The cluster-wide admission
configuration-file form remains separate work because this crate does not yet
have an admission plugin configuration-file surface.

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
closes `resource_quota`'s persisted-counter gap completely. A process-local
admission reservation lock now also serializes namespaced creates across the
live quota check and the following object write, so concurrent requests to
one nodeapiserver cannot both pass against the same pre-create usage snapshot.
Multi-process quota reservation remains outside this crate's single-process
scope.
Placed last among this crate's admission blocks (after `LimitRanger`'s
own defaulting), the same relative position real upstream's own default
plugin order uses, so quota sees the final, fully-defaulted object.

`ValidatingAdmissionPolicy` and `ValidatingAdmissionPolicyBinding` are now
loaded from storage and matching bindings are evaluated against the final
candidate object before persistence. The existing pure policy
decoder/matcher/CEL evaluator is reused; named and selector-based
`paramRef` objects are resolved through generic REST storage. `Deny` rejects
the request, `Warn` is returned as an HTTP `Warning: 299` header, and `Audit`
adds the upstream `validation.policy.admission.k8s.io/validation_failure`
annotation to the request's audit event. Multiple failures are accumulated so
warnings and audit details survive even when another binding denies the
request.

The shared CEL evaluator enables Kubernetes' optional field and index selection
syntax (`.?` and `[?...]`) while compiling policy expressions. This is required
for current Kubernetes admission policies, including the reference DRA
driver's `userInfo.extra` and optional `spec.nodeName` expressions.

`MutatingAdmissionPolicy` and `MutatingAdmissionPolicyBinding` are now
loaded from storage before validating admission. Matching bindings can apply
multiple JSON Patch operations or an apply configuration in order, including
parameter and selector matching, with the policy's `failurePolicy` honored.
The CEL adapter now exposes schema-backed typed `JSONPatch{}` and `Object{}`
declarations, including nested object fields and list-item aliases, while
preserving the mutation result's JSON shape. Composed
`spec.variables` are evaluated in declaration order and exposed to validation and mutation expressions through
the `variables` object; match conditions remain evaluated before composition.
Each matching policy chain is evaluated once per write; the e2e coverage uses
a non-idempotent finalizer append to guard against duplicate dispatch.
The policy request object also carries the submitted kind and authenticated
`userInfo` for mutation expressions, with the anonymous identity used when
the request has no authenticated certificate.

Configured mutating and validating webhooks are also invoked for matching
create, update, delete, and deletecollection requests (the latter once per
selected object). Dry-run requests are rejected with a
`400` when a matching webhook omits `sideEffects` or declares `Unknown`/
`Some`; `None` and `NoneOnDryRun` webhooks continue through the normal
AdmissionReview path.

**Not yet landed**: the remaining built-in plugins, a complete registry covering
storage-backed mutators and validators, the remaining typed-CEL /
variable surface of MutatingAdmissionPolicy, and interpreter-level fuel
accounting. The ValidatingAdmissionPolicy path uses the existing
per-expression deadline and shared request-side CEL budget. Admission
webhooks, validating/mutating admission policies, and ResourceQuota's
persisted usage counter are integrated; their remaining compatibility edges
are tracked in the paragraphs above.

**K. CRDs (apiextensions)** — **done for current scope**. Found on inspection, not
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
`server::listener`'s own `WATCH` dispatch. A live CRD-cache reconciler now
registers every served version of an Established CRD and removes the
reflector when that version is no longer served or the CRD is deleted; the
first-watch dynamic resolve remains as a bounded startup-race fallback.

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

Schema-local enum membership, numeric/string/collection bounds, patterns,
uniqueness, and standard scalar formats are enforced by
`apiextensions::schema_validation`. Cross-field consistency remains the
CRD's `x-kubernetes-validations` CEL mechanism — runtime evaluation now has
a shared request-side budget, while interpreter-level fuel accounting
remains a real DoS-hardening limitation.

**CRD conversion webhooks are real for REST CRUD paths now** —
`apiextensions::conversion` sends the ordered `ConversionReview` request to
the configured URL or Service, reusing the admission client's ClusterIP
resolution, TLS, and CA-bundle handling. CRD objects are converted to the
declared storage version before writes and back to the requested served
version for `GET`/`LIST`, watches, and write responses; the focused
`tests/crd_roundtrip.rs` test proves this against a real nodestore and a
local conversion webhook. Converted objects are pruned and checked against
the storage version's structural schema before persistence, so a webhook
cannot bypass storage-version required/type/constraint validation.

**Status-subresource schema handling is real now** — for an established CRD
version that declares `subresources.status`, both `update_status` and
`patch_status` apply that version's `properties.status` schema before
persistence. Unknown status fields are pruned, and required/type/schema-local
constraints return a real `422`; built-in status paths remain the generic
untyped path because their per-kind status strategies are not represented by
the dynamic CRD schema walker. The behavior is covered by the live
`test_nodeapiserver_validates_crd_status_subresource` e2e check.

**`cel_ext` — the CEL cost budget, a real design pass (2026-08-21).** The
static estimator and request-side runtime budget now protect
`x-kubernetes-validations` (Group K) and
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
`cel::parser::Parser::new().enable_optional_syntax(true).parse(expr)` ->
`cel::Context::default()` +
`ctx.add_variable(name, value)`/`ctx.add_function(name, f)` ->
`ctx.resolve(&expression)` -> `cel::Value::Bool(bool)` on success. The
nodeapiserver path uses `cel::parser::Parser::enable_optional_syntax(true)`
instead of the convenience compile function because Kubernetes policy
expressions rely on optional field/index selection (`.?` and `[?...]`).
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
   `x-kubernetes-validations`). The raw helper remains a unit-testable
   primitive; real requests use the budgeted `cel_evaluate` wrapper below.
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
   APF work). This budgeted path is wired into CRD create/update/patch
   validation through `apiextensions::cel_evaluate`; it remains a
   wall-clock approximation because the `cel` crate exposes no
   interpreter-level interruption hook.
3. Static checked-cost estimation (layer 1) — real upstream's own
   defense against a malicious *rule* (not just malicious input), needed
   before `x-kubernetes-validations` can be accepted at CRD-creation
   time with any confidence its worst case is actually bounded.
   **Started**: `cel_ext::cost` — the real `SizeEstimate`/`CostEstimate`
   min/max-range arithmetic (`cel-go`'s own `checker/cost.go` +
   `common/cost/cost.go`, fetched and read directly), the primitive
   every other piece of the estimator is built on. Real finding, worth
   recording: the `cel` crate's `Program::expression()` exposes a real,
   fully walkable AST (`Call`/`Comprehension`/`Ident`/`List`/`Literal`/
   `Map`/`Select`/`Struct`, all `pub`, confirmed directly in the
   vendored crate source) — **no CEL type-checker is needed to walk
   it**, only for the two narrow places real upstream's own algorithm
   consults resolved *type* info (deciding whether a `Select`'s operand
   is itself indexable, and a comprehension's range-container kind when
   entry-size tracking alone doesn't resolve it) — both approximable
   with a named, honest simplification rather than requiring a real
   type-checker subsystem. **Also started**: `cel_ext::decl_type` — the
   schema-driven size lookup's own prerequisite, a faithful port of real
   upstream's `SchemaDeclType` (`k8s.io/apiserver/pkg/cel/common/
   schemas.go`, fetched and read directly): recursively converts a CRD's
   own runtime `openAPIV3Schema` `Value` (the same `apiextensions::
   schema_*` walking convention already established elsewhere) into a
   `DeclType` tree, each node carrying a real `max_elements` worst-case
   bound — the schema's own explicit `maxLength`/`maxItems`/
   `maxProperties` when declared, else a real derived bound from
   `MaxRequestSizeBytes` (`k8s.io/apiserver/pkg/cel/limits.go`'s own
   `Min*Size`/`Max*SizeJSON` constants and `estimateMax*` formulas,
   fetched and read directly, all now confirmed and ported). Real
   finding: k8s's own `sizeEstimator.EstimateCallCost` (`k8s.io/
   apiextensions-apiserver/.../schema/cel/compilation.go`) always
   returns `nil` — k8s supplies **only** size estimates to cel-go's own
   estimator, never call-cost overrides, so the entire real per-
   builtin-function cost table lives in cel-go's own `checker/cost.go`
   alone, with nothing k8s-specific to additionally port there.
   `decl_type::estimate_size` is real upstream's own
   `sizeEstimator.EstimateSize` itself, landed in the same PR: walks a
   resolved path one `DeclType` level at a time (`@items`/`@values`/
   `@keys`/a named field), returning that location's own `SizeEstimate`
   — confirmed real upstream's own `Min` is always `0` here, never
   derived from `minLength`/`minItems`, and a map's own `@keys` size is
   untracked (`{0, 0}`, real upstream's own `StringType` carries no
   `MaxElements` override for keys either). **Also landed**: `cel_ext::
   path` — resolves a CEL expression's own `Select`/`Ident` chain into
   the field path `estimate_size` consumes (real upstream's own
   `coster.getPath`/`costIdent`/`costSelect`), including the
   single-variable comprehension form's own iteration-variable path
   (`list.all(x, ...)`, real upstream's own `pushIterSingle` narrowed to
   list-only, named honestly — see that module's own doc comment for the
   real scope). **`cel_ext::cost_walk` is the actual `cost()`
   AST-walking dispatcher itself**: real upstream's own `(*coster).cost`
   (`checker/cost.go`), every structural node kind
   (`Literal`/`Ident`/`Select`/`List`/`Map`/`Struct` — a presence test's
   own real `presenceTestCost` vs. an ordinary select's
   `selectAndIdentCost` distinction ported exactly), plus `Call` for a
   deliberately scoped real subset. **Real finding that shaped `Call`'s
   own scope, confirmed directly in the vendored crate source**: this
   crate's parser has no type checker at all, so `a + b` compiles to the
   identical `Call{func_name: "_+_"}` node whether `a`/`b` are numbers,
   strings, or lists — real upstream's own type-specialized overloads
   (`add_string`/`add_list`, O(n), vs. plain numeric `+`, O(1)) genuinely
   can't be told apart at this AST level, and the same ambiguity hits
   `==`/`!=`/`<`/`>`/`<=`/`>=`. **Asked explicitly which way to resolve
   this, the answer was: treat every ambiguous operator as the cheap
   O(1) case** (real upstream's own fallback for a genuinely-unrecognized
   function) — a deliberate choice, not an oversight, to avoid
   over-rejecting a purely-numeric rule at CRD-acceptance time. What
   *is* real and type-unambiguous without a type checker — a named
   method call whose function name only ever applies to a string in
   real CEL's own standard library — gets real upstream's own exact
   formula: `matches`/`contains`/`startsWith`/`endsWith`, each scaled by
   a real schema-derived `SizeEstimate` via `decl_type`/`path`'s own
   machinery (`cel_ext::cost_walk::Coster`, the stateful walker this
   needs — a bare `cost()` free function remains for callers with no
   schema at all). Everything else not named falls to real upstream's
   own O(1) default too.
   **`Comprehension` is landed too — the `cost()` dispatcher is now
   complete for every real AST node kind.** Real upstream's own
   `costComprehension`: the range's own real element count (from a
   schema-bounded `maxItems`, when one exists) times the combined
   per-iteration cost of the loop condition and step — an unknown range
   size (no schema, or an unresolvable path) correctly saturates the
   resulting cost to "effectively unbounded" rather than silently
   under-counting a pathological `list.all(...)`-shaped rule at a small
   fixed number; a schema-bounded range instead produces a real, finite
   worst-case number, live-verified against a real `maxItems`-bounded
   test fixture. **Named, honest gap**: real upstream's own `cel.bind()`
   macro reuses this identical AST shape with a distinctive signature
   (`isBind`/`costBind`) real upstream costs completely differently (its
   body runs once, never multiplied by a loop) — not detected here, so
   an actual `cel.bind()` expression's own empty-range shape would
   *under*-cost its loop term to `{0,0}`; named rather than silently
   glossed over, and not caught by any test since `cel.bind()` isn't
   real, documented `x-kubernetes-validations` authoring practice.
   **Every real primitive `cost()` itself needs is now complete and
   tested.** `cel_ext::budget::check_rule_cost` is the real accept/reject
   decision itself, a faithful (if deliberately scoped) port of real
   upstream's own CRD-acceptance-time static check
   (`pkg/apis/apiextensions/validation/validation.go`'s own
   `ValidateCustomResourceDefinitionOpenAPISchema`, fetched and read
   directly): a rule whose effective worst-case cost exceeds real
   upstream's own `StaticEstimatedCostLimit` (`10_000_000` — confirmed
   directly, numerically identical to `RuntimeCELCostBudget` but a
   conceptually distinct constant: one bounds a rule's estimated worst
   case at CRD-acceptance time, the other bounds actual accumulated
   runtime cost against one real object) is rejected. The effective cost
   is `cr.MaxCost * cardinalityCost.MaxCardinality`
   (`getExpressionCost`): the recursive CRD schema walk propagates the
   maximum product of enclosing `maxItems`/`maxProperties` bounds, so a
   rule nested under a repeating array/map is charged once per possible
   element/entry rather than only once. That propagated cardinality is
   distinct from `DeclType::max_elements`, which bounds one node's own
   element count. The standalone `check_rule_cost` helper remains the
   single-evaluation form for callers that do not have a schema path.
   Also not ported: real upstream's own static `ast.OutputType() !=
   cel.BoolType` rejection (this crate's type-checker-free parser can
   only discover a non-bool rule at runtime, via `eval_bool`'s existing
   `Error::NotBool`).
   **Now wired into a real CRD-acceptance request path — this closes
   Phase 3's own real remaining item.** `apiextensions::cel_validations::
   validate_schema_cel_costs` is real upstream's own recursive schema
   walk (`ValidateCustomResourceDefinitionOpenAPISchema`, scoped to its
   CEL-cost half), checking every `x-kubernetes-validations` rule at
   *every* schema level (not just the root — a nested rule's own `self`
   genuinely means "the value here," so `decl_type_for` runs fresh
   against each level it's found at, not the whole schema's own root)
   against a real cost budget. `validate_crd_cel_costs` walks every
   declared `spec.versions[]` and formats each real violation into the
   same `Vec<String>` shape every other structural check already
   produces. `server::rest::create`/`update` both call it for a
   `CustomResourceDefinition` write specifically, alongside the existing
   required/type/name-format checks — a client authoring a runaway rule
   now gets a real `422` at CRD-acceptance time.
4. **Done.** Real upstream's own runtime evaluation of
   `x-kubernetes-validations` against an actual custom resource instance
   — `apiextensions::cel_evaluate::validate_object`, real upstream's own
   `customResourceStrategy.Validate`/`ValidateUpdate`
   (`k8s.io/apiextensions-apiserver/pkg/registry/customresource/
   strategy.go`, fetched and read directly), scoped to just the rule
   evaluation itself (defaulting/pruning/structural validation are
   already their own modules, not duplicated here). Recursively walks
   the schema *and* the real object together (`apiextensions::
   schema_validation`'s own data-driven recursion convention), `self`/
   `oldSelf` bound per schema level. Transition rules that reference
   `oldSelf` are skipped when no prior value exists, while a rule with
   `optionalOldSelf: true` receives a real CEL optional whose
   `hasValue()`/`value()` behavior works on both `CREATE` and `UPDATE`. Each
   rule is capped by
   `eval_bool_with_deadline` (Phase 2, already landed) at real upstream's
   own `PerCallLimit` (~0.1s), under one shared ~1s wall-clock
   `RuntimeCELCostBudget` per object. Once that shared window is exhausted,
   the evaluator stops walking further schema rules. Wired into `server::rest::create`/
   `update`/`patch_persist`'s existing CRD branches, after pruning and
   required/type validation, against the fully-defaulted object (real
   upstream's own ordering — a rule commonly assumes a field already
   carries its real default). **Named, honest limitation**: the `cel` crate
   exposes no interpreter-level fuel or interruption hook, so a timed-out
   worker thread may finish in the background even though request-side
   evaluation is bounded and the concurrency gate limits how many such
   evaluations can be started at once. Also not ported: real upstream's own
   static `UsesOldSelf`/uncorrelatable-schema
   rejection at CRD-acceptance time (an `oldSelf`-referencing rule that
   can't validly correlate old/new values in some schema shapes).
5. **ValidatingAdmissionPolicy enforcement is landed.**
   `admission::policy_enforcement` loads policies and bindings from storage,
   resolves named or selector-based `paramRef` objects, and evaluates every
   matching binding against the final candidate object before persistence.
   It also supplies `oldObject` for update/delete and the real
   `request.dryRun` value. The same storage-backed policy machinery also
   applies MutatingAdmissionPolicy JSONPatch and ApplyConfiguration results
   before the remaining admission plugins run.
   `ValidatingAdmissionPolicy` itself is now **confirmed, live, working
   generic CRUD** (`tests/validating_admission_policy_roundtrip.rs`, a
   real round trip against a real `nodestore`: create/get/list/update/
   delete all pass) — the same "generic REST just works once the schema
   exists" finding Group L Phase 1 made for `APIService`, now verified
   live rather than assumed. Getting there found and fixed two real,
   previously-latent codec bugs (neither specific to VAP — both apply to
   any resource hitting the same proto shapes): (a) real upstream Go
   struct embedding — `NamedRuleWithOperations` → `RuleWithOperations` →
   `Rule` flattens in JSON with no wrapper key, but the vendored proto
   keeps each as an ordinary nested message field, so every field past
   `resourceNames` silently vanished on write until
   `codec::protobuf::is_inline_embedded_field` special-cased it; (b)
   `go-to-protobuf` capitalizes a field name whenever the Go struct's own
   `json` tag omits an explicit name, and while most of those really are
   capitalized in real upstream's JSON too (`FieldsV1.Raw`,
   `DaemonEndpoint.Port`, `CustomResourceColumnDefinition.JSONPath` —
   confirmed, left alone), `Validation.Expression`,
   `Variable.Name`/`.Expression`, and both webhook configs' `Webhooks`
   field are not — real upstream's own lowercase `json` tag didn't carry
   through codegen, fixed via `build/proto_parse.rs`'s new
   `real_json_name_override` table (audited every capitalized field in
   the whole vendored proto tree, not just the one that broke live).
   **`matchConditions` itself is landed**: `admission::
   match_conditions::match_conditions` is a faithful port of real
   upstream's own `matchconditions.Matcher.Match`
   (`k8s.io/apiserver/pkg/admission/plugin/webhook/matchconditions/
   matcher.go`, fetched and read directly) — the real CEL pre-filter
   shared by both webhooks' and `ValidatingAdmissionPolicy`'s own
   `spec.matchConditions`, all conditions must evaluate `true`, the
   first real `false` short-circuits the rest even past an earlier
   condition's own evaluation error, `FailurePolicy::Fail`/`Ignore`
   both real. Built on `cel_ext::eval_bool_with_vars`/
   `eval_bool_with_vars_and_deadline`, a real generalization of
   `eval_bool`/`eval_bool_with_deadline` to an arbitrary named-variable
   set (`object`/`oldObject`/`request`/`params`, not just `self`/
   `oldSelf`) — landed in the same PR, `eval_bool`/`eval_bool_with_deadline`
   now thin wrappers around it, behavior unchanged. The storage-backed
   policy adapter calls it for real admission requests.

   **The `resourceRules`/`namespaceSelector`/`objectSelector` matching
   engine and `request` CEL variable construction are also now landed**,
   same standalone-primitive posture: `admission::policy_matching` — real
   upstream's own `rules.Matcher` (`k8s.io/apiserver/pkg/admission/
   plugin/webhook/predicates/rules/rules.go`, fetched and read directly)
   for `resourceRules`/`excludeResourceRules` (a request matches when it
   matches any `resourceRules` entry and no `excludeResourceRules` entry;
   `Rule.Scope` is matched against the served resource's discovered
   namespaced-vs-cluster scope), `metav1.LabelSelectorAsSelector`'s own
   conversion for
   `namespaceSelector`/`objectSelector` (reusing `cacher::selector`'s
   already-landed label matcher rather than reimplementing it), and
   `CreateAdmissionRequest`'s own JSON shape
   (`k8s.io/apiserver/pkg/admission/plugin/cel/condition.go`, fetched and
   read directly) for the `request` variable — `kind`/`requestKind` come
   from the submitted or old object, and `userInfo` comes from the
   authenticated request identity.

   **The actual `spec.validations[]` Admit/Deny decision is also now
   landed**: `admission::policy_validations` — real upstream's own
   `validator.Validate` (`k8s.io/apiserver/pkg/admission/plugin/policy/
   validating/validator.go`, fetched and read directly), given an
   already-bound variable set, evaluates every validation (no
   short-circuit, unlike `match_conditions` — real upstream reports every
   violation, not just the first) with real upstream's own exact
   message-resolution order (`messageExpression` if it evaluates to a
   non-empty, single-line, ≤5KiB string; else the rule's own `message`;
   else a generic `"failed expression: ..."`) and the same
   `failurePolicy`-governed handling of a compile/evaluation error
   (`Fail` denies, `Ignore` admits, either way marked as a real error, not
   a real `false` result). Built on two new primitives,
   `cel_ext::eval_string_with_vars`/`eval_string_with_vars_and_deadline`
   — this crate's first real use of a non-boolean CEL result, since
   `messageExpression` evaluates to a `string`.

   **The real per-policy decision that composes all three is also now
   landed**: `admission::validating_admission_policy::evaluate` — real
   upstream's own real order (`matchConstraints` → `matchConditions` →
   `validations`, each stage only narrowing further, matching
   `validator.Validate`'s own real shape), given one policy's own
   borrowed field view (`PolicyDefinition`) and an already-bound variable
   set. Returns `NotApplicable` (constraints/conditions excluded it — a
   real `matchConditions` `false` and real upstream's own `Ignore`-policy
   "skip this policy" outcome both collapse to this, matching
   `MatchResult::matches()`'s own real collapsing), a real
   `MatchConditionsError` (a `matchConditions` evaluation error under
   `failurePolicy: Fail`, kept distinct from a validation denial since a
   caller must handle the two differently), or `Decided` (the real
   per-`validations[]`-rule outcome).

   **A real `ValidatingAdmissionPolicy` object now decodes into a usable
   `PolicyDefinition` too**: `admission::policy_decode` — field names
   verified directly against the vendored OpenAPI schema (not assumed
   from memory), tolerant of a missing/malformed field the same way
   `cacher::selector::object_labels` already is. `ResourceRule`/
   `PolicyDefinition` both gained a second lifetime parameter to make this
   possible without an unsafe self-referential struct — see that module's
   own doc comment for the real two-step shape (`DecodedPolicy::
   resource_rules()`/`exclude_resource_rules()` hand back a freshly built
   `Vec` a caller binds to a local, rather than one method returning a
   fully-assembled `PolicyDefinition`) and why a single-call shape can't
   express it safely. A live end-to-end test
   (`policy_decode::tests::a_real_policy_document_decodes_and_evaluates_
   end_to_end`) decodes a real JSON policy document, evaluates it through
   `validating_admission_policy::evaluate`, and confirms `matchConditions`,
   `matchConstraints`, and `validations` all compose correctly together —
   not just each primitive tested in isolation.

   **The real `object`/`oldObject`/`request`/`params` variable assembly is
   also now landed**: `policy_matching::build_eval_vars` — binds
   `object`/`oldObject`/`params` to a real CEL `null` (not an absent
   variable) when the caller has none, matching real upstream's own real
   behavior (`object` is `null` on `DELETE`, `oldObject` is `null` on
   `CREATE`, `params` is `null` with no `paramKind`/`paramRef`) — verified
   live rather than assumed: a dedicated test binds `serde_json::
   Value::Null` through `cel_ext::eval_bool_with_vars` and confirms an
   expression like `oldObject == null` actually evaluates rather than
   erroring on an undefined variable. `namespaceObject` is now supplied to
   validation expressions while remaining unavailable to `matchConditions`,
   matching upstream's variable scope. Composed `spec.variables` are now
   evaluated in declaration order after the match gates and exposed through
   the `variables` object to validation and mutation expressions; a later
   declaration cannot reference an earlier declaration's future value.

   **The decision side is now complete**: `PolicyOutcome::is_denial`
   (folds `MatchConditionsError` back into a real denial, unlike
   `denies()` — safe unconditionally, since `evaluate` only ever produces
   that variant when `failurePolicy` is already `Fail`) /
   `denial_message` (what to actually report) and the standalone
   `validation_actions_deny`, `validation_actions_warn`, and
   `validation_actions_audit` (the real `ValidatingAdmissionPolicyBinding.
   spec.validationActions` gates). The storage-backed adapter accumulates
   the matching warning messages and upstream-shaped validation-failure
   audit annotation details for the request wrapper to publish.

   The storage-backed adapter is wired into `server::listener` after
   authorization and before persistence. Parameter references support named
   and label-selected parameters, including `parameterNotFoundAction`.
   The Kubernetes `authorizer` CEL library is bound for validation
   expressions from a request-local RBAC snapshot. Its fluent
   `group`/`resource`/`namespace`/`name`/`check`/`allowed` chain,
   `requestResource`, non-resource `path` checks, and service-account
   principal switching are covered by focused tests. ResourceRule scope and
   the request's kind/userInfo fields are populated from discovery and the
   authenticated admission request.
6. Kubernetes' own CEL extension library — **started**: `cel_ext::
   kubernetes_lists` is real upstream's own `kubernetes.lists` library
   (`k8s.io/apiserver/pkg/cel/library/lists.go`, fetched and read
   directly), and **every function it declares is now landed**:
   `isSorted`/`min`/`max`/`indexOf`/`lastIndexOf`/`sum`/`includes`, wired
   onto every real `cel::Context` this crate builds via `cel_ext::
   register_kubernetes_extensions` (`cel::Context::add_function`,
   cel-rust's own real custom-function registration API — confirmed
   against that crate's own `example/src/functions.rs`/`cel/src/
   functions.rs` (`contains`/`string`/`size`), which the published docs
   don't render). Live round trips through a real `cel::Context` (not
   just each function's own pure unit tests) prove real CEL expressions
   can actually call each of them by name, including `min()` on a real
   empty list producing a real `ExecutionError`, not a panic or a silent
   default. `sum` is built on `Value`'s own real `Add` (`impl Add<Value>
   for Value { type Output = Result<Value, ExecutionError>; }`, confirmed
   against the published `cel` crate docs directly — a genuinely
   unsupported pair surfaces a real `ExecutionError` from `Value::add`
   itself, not a silent no-op); `includes` needed its own `This<Value>`
   binding shape (not `This<Arc<Vec<Value>>>`, unlike every other
   function here) since real upstream's own real behavior is a dual one —
   a list target does a real membership scan, anything else falls back to
   a plain equality check against the argument (`'model-a'.includes(
   'model-a')` → `true` is real upstream's own doc-comment example of
   exactly that fallback). **Named, honest divergences**: real upstream
   restricts every comparable/equatable/summable function here at
   CEL-compile time to a single element type per call; this crate's own
   bindings have no type checker to enforce that — `isSorted`/`min`/`max`
   treat a genuinely incomparable adjacent pair as "no match" (skipped
   rather than erroring) instead of real upstream's own compile-time
   rejection, and `sum` of an empty list always answers `Value::Int(0)`
   regardless of which real element type was actually intended (a real,
   incorrect answer specifically for a duration-typed empty sum, not just
   cosmetic).

   **`kubernetes.quantity` is now complete for its declared runtime surface**:
   `cel_ext::kubernetes_quantity::is_quantity`/`is_quantity_binding` is
   `isQuantity(<string>)`, real
   upstream's own `k8s.io/apiserver/pkg/cel/library/quantity.go`, fetched
   and read directly — `isQuantity(s)` is real upstream's own real
   definition (`true` iff `quantity(s)` wouldn't itself error), so this
   reuses `scheme::quantity::Quantity::parse` — Group G's own already-
   landed quantity port, the same parser `admission::limit_ranger`'s
   min/max/ratio comparisons are already built on — rather than a second,
   potentially-diverging parser. `quantity(<string>)` now returns a genuine
   opaque `kubernetes.Quantity` value, with `isInteger`/`asInteger`/
   `asApproximateFloat`/`sign`/`add`/`sub`/`isLessThan`/`isGreaterThan`/
   `compareTo` implemented against that same exact parser-backed value.
   Because this Rust CEL layer has no static type checker, overload misuse is
   reported as a runtime function error rather than rejected during compile.

   The named CEL libraries are now wired through the `authz` surface:
   `kubernetes.regex` provides upstream's `find` and `findAll` member
   functions, including the optional non-negative match limit and the real
   empty-string/empty-list no-match results. Invalid patterns surface as CEL
   execution errors. The `kubernetes.format` library provides native optional
   values plus named DNS/label/qualified-name, URI, UUID, byte, date, and
   datetime validators. Valid input returns optional.none; invalid input
   returns the upstream-shaped list of validation messages. Authorization
   resource checks now also carry upstream's `fieldSelector` and
   `labelSelector` methods through fluent chains. The current RBAC evaluator
   preserves those selector values on the opaque check but has no
   object-selection phase, so they do not alter its boolean decision. The Rust
   `regex` engine covers the RE2-compatible patterns used by Kubernetes, while
   this crate's runtime CEL layer still reports overload misuse for dynamic
   values at evaluation time, while the schema-aware acceptance checker
   rejects the statically obvious cases before the CRD is stored. The
   remaining CEL gap is full upstream type-environment parity for every
   extension overload and schema feature.

**L. Aggregation layer** — **all four phases done.**
`k8s.io/kube-aggregator`'s own `APIService` mechanism
(`pkg/apis/apiregistration/v1/types.go` + `pkg/apiserver/handler_proxy.go`,
fetched and read directly): an `APIService` object names a group-version
this build stops answering itself and instead reverse-proxies to a
backing `Service`. `aggregator/mod.rs`'s own module doc is the
authoritative live status — read it, not this paragraph, for the exact
current scope of each piece; summarized here:

1. **Done.** `APIService` is a real, working generic-REST resource
   (`tests/apiservice_roundtrip.rs` proves create/get/list/update/delete
   end to end against a real `nodestore`) — `vendor/refresh.sh`'s
   proto-fetch glob was missing `k8s.io/kube-aggregator` entirely (it
   doesn't start with `api*`), fixed by vendoring that package's
   `generated.proto` directly.
2. **Done.** `aggregator::availability` is the real availability
   controller's *decision logic* (`local`/`remote`, a faithful port of
   `kube-aggregator`'s own two controllers); `aggregator::reconcile::
   reconcile_once` is the live loop that actually runs it — lists every
   stored `APIService`, runs pre-flight plus (once that passes) a real
   discovery-endpoint dial, and writes the resulting `Available`
   condition to `status.conditions` via `rest::update_status`. Spawned as
   a periodic (30s) background task from `server::listener::run`.
   **Named, honest simplification**: the discovery-endpoint dial is a
   single real request (`proxy::http_client::fetch`), not upstream's own
   5-concurrent-probe check — one real network round trip either
   succeeds or it doesn't, and concurrency there only ever buys
   resilience against one flaky backend replica among several.
   `aggregate_proxy`/`discoverable_group_versions` now consult this
   loop's written condition too (`availability::cached_available`):
   `discoverable_group_versions` trusts a decisive cached answer outright
   (zero I/O); `aggregate_proxy` short-circuits straight to `503` on a
   cached `Available: False`, skipping the Service/`EndpointSlice` fetch
   — still runs the full fresh check on `True`/unknown, since resolving
   the actual dial target needs the backing Service fetched regardless.
3. **Done.** Discovery merge — `aggregator::route::
   discoverable_group_versions` (every stored, non-local `APIService`
   that currently passes pre-flight) feeds `server::discovery::
   merged_group_version_map` as a third input alongside the static table
   and Group K's CRD-sourced one, wired into `/apis`/`/apis/{group}`
   (both legacy and `apidiscovery.k8s.io/v2` shapes) and
   `/apis/{group}/{version}`'s own `APIResourceList` (a real live
   proxied fetch, not a builder).
4. **Done.** The actual reverse proxy — `server::listener::
   aggregate_proxy`, wired into `handle()`: resolves the one matching
   `APIService` (`route::resolve`), builds its per-`APIService` TLS trust
   (`aggregator::client_tls::build_client_config` — real
   `spec.caBundle`/`.insecureSkipTLSVerify`, `webpki-roots` fallback),
   and relays the whole request (method/headers/body, any verb) through
   `proxy::http_client::relay`. The standard `front-proxy-client` identity
   is now presented when `NODEAPISERVER_PROXY_CLIENT_CERT_FILE` and
   `NODEAPISERVER_PROXY_CLIENT_KEY_FILE` are configured; nodebootstrap
   generates those files plus a separate `front-proxy-ca` and migrates
   older PKI directories. Caller-supplied `X-Remote-*` headers are removed
   and rebuilt from the authenticated request identity, including the
   Kubernetes percent-escaped `X-Remote-Extra-*` form. HTTP/1.1 streaming
   upgrades are forwarded through `proxy::http_client::upgrade_with_headers`,
   which applies the same trusted-header replacement before splicing the
   upgraded streams. Less-common transport details such as legacy SPDY and
   HTTP/2 extended CONNECT remain outside the current scope.


**M. APF, audit, observability** — **in progress**. `audit::event::build_event`
is a pure builder for real `audit.k8s.io/v1` `Event` documents
(`staging/src/k8s.io/apiserver/pkg/apis/audit/v1/types.go`, fetched and
read directly), at the policy-selected `Metadata`, `Request`, or
`RequestResponse` level. `Request` and `RequestResponse` capture decoded
JSON/YAML request objects, and `RequestResponse` also captures bounded,
materialized JSON/YAML response objects; streaming, oversized, and
protobuf bodies remain uncaptured. The listener now emits the applicable
`RequestReceived`, `ResponseStarted`, and `ResponseComplete` stages with one
shared audit ID per request; long-running watches and streaming subresources
are recorded at `ResponseStarted` rather than falsely marked complete.
Requests rejected before normal handler dispatch are also recorded as
`ResponseComplete` events, including authentication, authorization, and
request-admission failures.
`NODEAPISERVER_AUDIT_POLICY_FILE` applies upstream-shaped first-match rules,
including `None` suppression and `omitStages` for those emitted stages;
the `Panic` stage remains outside the current scope. **The sink is this
crate's own `tracing` output**
(`target: "nodeapiserver::audit"`, one JSON line per request) and an
optional append-only JSON-lines file selected by
`NODEAPISERVER_AUDIT_LOG_PATH`. When configured with
`NODEAPISERVER_AUDIT_LOG_MAX_SIZE_BYTES`, the file rotates before a new
event would exceed the limit and retains the configured numbered backups via
`NODEAPISERVER_AUDIT_LOG_MAX_BACKUPS`. When
`NODEAPISERVER_AUDIT_WEBHOOK_URL` is set, the sink also delivers bounded
asynchronous `audit.k8s.io/v1` `EventList` batches to that HTTP(S) endpoint,
retrying transient transport, 5xx, and 429 failures without blocking API
responses. It can be used by itself or alongside the file sink; kubeconfig
credential-file authentication is still outside the current scope.
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
`"<name> check passed\n"` on success when `?verbose` is given. The
upstream `?exclude=<check>` query parameter is also supported, including
the verbose `excluded: ok` output and warnings for unknown check names.
Not ported: the `log`/`informer-sync`/`shutdown` checks (klog-specific,
no-informers-here, and no graceful-shutdown machinery respectively).
`deploy/setup-control-plane.sh` already polls `/readyz?verbose`, so this
is now genuinely meaningful there, not just a bare `200`. `/metrics` is
real too now (`server::metrics`, a
scoped port of real upstream's own `apiserver_request_total` counter,
`k8s.io/apiserver/pkg/endpoints/metrics`): every completed request is
recorded under upstream's full nine-label identity
(`verb`/`dry_run`/`group`/`version`/`resource`/`subresource`/`scope`/
`component`/`code`), matching the dimensions clients and dashboards use to
separate equivalent requests. Rendered as real
Prometheus text exposition format by hand (no metrics crate dependency,
mirroring `crates/nodelet/src/server/prom_metrics.rs`'s own established
`push_metric`-shaped approach, sorted output so repeated scrapes diff
cleanly), one process-wide `Mutex<HashMap<...>>` counter table — this
crate's own request rate never remotely approaches where a real
lock-free registry would matter. `apiserver_request_duration_seconds`
(a histogram, real upstream's own exact bucket boundaries),
`apiserver_response_sizes` (real upstream's own exponential buckets,
recorded from `http_body::Body::size_hint().exact()` — not recorded for
`watch`'s own unbounded stream, named honestly), and
`apiserver_watch_events_total` (`group`/`version`/`resource` labels,
incremented once per event actually encoded and written to a client) are
now ported too — see `server::metrics`'s own module doc for the exact
scope. `apiserver_current_inflight_requests` is now exposed with the
upstream `request_kind` labels (`readOnly`/`mutating`) and tracks requests
holding this build's bounded APF seat; exempt and long-running requests
that bypass the gate are not counted. **APF
(FlowSchema/PriorityLevelConfiguration queueing) now has a bounded request
gate**: in addition to the global ordinary and mutating budgets, selected
limited priority levels enforce nominal-share concurrency caps and their
`Reject`/queue-length policy, while exempt levels and long-running streams
remain outside the finite budgets. The inflight gauge reports the maximum
seat usage observed during the preceding one-second window, matching
upstream's pre-aggregated semantics. `flowcontrol::flow_schema` ports real
upstream's own
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
Fails open (no header) on resolution failure, while the listener still
applies its bounded ordinary/mutating request budgets. The
`distinguisherMethod` now computes upstream-compatible `ByUser` and
`ByNamespace` keys. Each limited level has one shared queue set; the
distinguisher selects a deterministic shuffle-shard hand within that set
instead of incorrectly creating an independent concurrency budget per flow.
Limited levels can borrow idle lendable seats from other limited levels up to
their configured borrowing limit. The two mandatory
bootstrap `FlowSchema`s real upstream always synthesizes (Group O's job).

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

`exec`, `attach`, and `port-forward` are now live upgrade proxies too:
`proxy::pod_stream` resolves the pod/node target, translates the API's
`stdin`/`stdout`/`stderr` and plural `ports` query parameters to nodelet's
kubelet-style routes, and `proxy::http_client::upgrade` forwards the
upgrade headers before splicing both upgraded connections. The listener is
served with hyper's upgrade-aware connection path. Existing streaming e2e
cases for exec, attach, and port-forward exercise these routes against a
real CRI runtime.

The core `nodes/{name}/proxy` and `services/{name}[:port]/proxy` subresources
are now live request/response relays. Node targets use the Node's preferred
address and kubelet port; Service targets select a ready EndpointSlice first
and fall back to the Service ClusterIP when endpoint data is temporarily
absent. The relay preserves the incoming method, query, supported headers,
body, response status, response headers, and response body. Service
`appProtocol: https` is supported; non-TCP Service ports are rejected because
the proxy is an HTTP relay. Both the normal `/.../{name}/proxy/...` and the
legacy `/.../proxy/{resource}/{name}/...` path forms are handled. Remaining
Group N work is limited to the upstream's less common proxy transport details
(for example SPDY-era protocol negotiation and endpoint retry/load-balancing
policy).

**O. nodebootstrap integration** — **done for this branch's scope.**
Nodebootstrap itself is not part of the nodeapiserver implementation. The
only item tracked here is the API-server-facing integration: the existing
bootstrap path can select and install nodeapiserver via
`--apiserver=nodeapiserver` (and uses it when no target is specified). The
upstream target remains available explicitly for comparison. PKI, RBAC
bootstrap policy, the default
`kubernetes` Service, CoreDNS, flannel, and the remaining installer lifecycle
belong to `docs/NODEBOOTSTRAP_PLAN.md`, not this document.

## Final acceptance

A cluster bootstrapped by `./deploy/bootstrap-source.sh --with-cri` with **no
k3s installed at all**, passing the full unfiltered `test-e2e.sh` suite
(~142 tests) including the real CSI and DRA reference drivers from
`deploy/lib/e2e-full-setup.sh`. Only then does `nodeapiserver` merge to
`main`, per `CLAUDE.md`'s merge protocol and the "no partial multi-phase
work" standing rule — satisfied here by merging into this integration branch
group-by-group and reserving `main` for the completed arc.
