# nodeapiserver — plan

## Context

`not-k8s` has replaced every part of Kubernetes except one. `nodelet` (kubelet),
`nodeproxy` (kube-proxy), `nodescheduler` (kube-scheduler), `nodecontroller`
(kube-controller-manager) and `nodestore` (etcd/kine) are all built, merged and
e2e-gated. The only thing still coming from stock k3s is **the apiserver itself**
— and with it, the cluster bootstrap that k3s does around it.

This plan covers building `crates/nodeapiserver` and removing k3s entirely.
When it lands, `not-k8s` is a standalone Kubernetes distribution.

Three decisions were made up front by the user and are **not open questions**:

1. **Replace k3s outright.** No shadow deployment, no dual-path deploy code.
   k3s is uninstalled from the start of the cutover branch. Accepted cost: no
   e2e signal from the *cluster* suite until the handler chain, authn, authz,
   admission, core/v1 storage and discovery all work together. See
   "Getting signal earlier" — there is a way to get real signal well before
   that without building dual-path deploy code.
2. **Absorb all four non-apiserver k3s responsibilities**: cluster PKI
   bootstrap, RBAC bootstrap policy, the `kubernetes` default Service +
   endpoint reconciler, and CoreDNS + flannel/CNI addon deployment. These are
   Group O.
3. **Bump the API surface to v1.34.** Measured below — this is far cheaper than
   `CLAUDE.md`'s pin warning implies.

CEL is `cel` 0.14 (Rust), per the user. Not cel-cpp, not a reimplementation.

---

## Research findings (verified this session — do not re-derive)

Everything below was checked against the real artifacts, not assumed. Fetched
copies live in the session scratchpad; re-fetch from the cited paths if gone.

### 1. Upstream's own architecture doc

`kubernetes/kubernetes` **`staging/src/k8s.io/apiserver/ARCHITECTURE.md`** (259
lines, on `master` only — it postdates `release-1.33`, which 404s). Fetch with:

```bash
gh api repos/kubernetes/kubernetes/contents/staging/src/k8s.io/apiserver/ARCHITECTURE.md \
  -H 'Accept: application/vnd.github.raw'
```

Its eight sections are the skeleton this component must match:

1. **Server chain** — three delegating `GenericAPIServer`s: aggregator →
   kube-apiserver (core) → apiextensions (CRDs). Built by `CreateServerChain`
   in `cmd/kube-apiserver/app/server.go`. 404 falls out the bottom.
2. **Handler chain** — `DefaultBuildHandlerChain` in
   `staging/src/k8s.io/apiserver/pkg/server/config.go`. Strict order:
   **authentication → authorization → priority-and-fairness → admission →
   REST endpoint.** The request body is *not deserialized* until admission —
   authn and authz run on the request line and headers alone. Get this order
   right; it is a security property, not a style choice.
3. **API group registration** — `Scheme` (GVK↔type, conversion, defaulting),
   `APIGroupInfo` (scheme + storage map + version priority), `Strategy`
   (per-resource business logic and validation), installed by `APIInstaller`.
4. **Watch cache** — LIST for a point-in-time RV, then WATCH from it. Serves
   most list/watch from memory. Consistent reads work by fetching the latest
   write revision from storage and waiting for the cache to reach it.
   Bookmarks keep the cache RV fresh so clients avoid expensive relists.
5. **Conflict resolution** — `resourceVersion` maps **directly to etcd
   `mod_revision`**; mismatch on write is `409 Conflict`. Plus Server-Side
   Apply with `managedFields` field ownership.
6. **Discovery + OpenAPI** — `/apis`, `/openapi/v2`, `/openapi/v3`.
7. **Audit + security** — policy-driven audit pipeline; mTLS; apiserver is an
   OIDC provider issuing ServiceAccount JWTs.
8. **Streaming** — websockets/SPDY upgrade for exec/attach/port-forward via
   `UpgradeAwareProxyHandler`.

### 2. Scale — measure, don't hand-wave

Measured from the upstream tree (`gh api "repos/kubernetes/kubernetes/git/trees/master?recursive=1"`),
Go files only, `_test.go` excluded, ~32 bytes/line:

| upstream area | ~kLOC | of which generated |
|---|---:|---:|
| `staging/src/k8s.io/apiserver/` — handler chain, storage+cacher, admission, authn/z, APF, audit, CEL | 163 | 16 |
| `staging/src/k8s.io/api/` — built-in types, all group-versions | 405 | 287 |
| `pkg/apis/` — internal types, conversion, defaults, validation | 232 | 152 |
| `staging/src/k8s.io/apiextensions-apiserver/` — CRDs | 75 | 31 |
| `staging/src/k8s.io/apimachinery/` — scheme, conversion, SMP, unstructured, watch | 72 | 15 |
| `pkg/registry/` — per-resource REST strategies | 44 | 0 |
| `staging/src/k8s.io/kube-aggregator/` | 22 | 9 |
| `plugin/pkg/admission/` | 10 | 1 |
| `pkg/controlplane/`, `plugin/pkg/auth/`, `cmd/kube-apiserver/` | 16 | 0 |
| **TOTAL** | **1038** | **511** |

**~527k handwritten Go.** For scale calibration: this repo's five existing
components total ~78k lines of Rust against roughly 390k lines of upstream Go
equivalent — call it 4-5x denser. Applying that ratio puts `nodeapiserver`
somewhere near **110-130k lines of Rust: larger than everything in this repo
put together.** Plan accordingly; do not let anyone believe this is a
one-branch job.

Two large mitigations, both verified below: ~287k of that generated code is
deepcopy + protobuf marshalling that serde/prost derives replace outright, and
`k8s-openapi` already supplies the entire type layer.

### 3. `nodestore` already is the storage backend

`crates/nodestore/src/server/mod.rs` implements the full etcd v3 gRPC surface —
`range`, `put`, `delete_range`, `txn`, `compact`, the whole Lease service,
Maintenance (`status`/`alarm`/`defragment`/`hash_kv`/`snapshot`), and Cluster
(`member_*`). Watches are in `server/watch.rs`, event fan-out in `watch.rs`.

**`resourceVersion` == nodestore's MVCC revision.** That mapping is already
exactly what upstream requires (ARCHITECTURE.md §5). `store.rs` owns the
revision semantics; `command.rs` states the determinism rules. Read
`command.rs` first — its header is the contract the rest of the crate obeys.

Two consequences worth designing around:

- The cacher exists upstream to shield etcd from watch load. nodestore hands
  applied events straight to watchers with no polling, so the cacher here is
  needed for *semantics* (RV=0 reads, bookmarks, consistent reads, selector
  filtering) rather than to protect the backend. Do not skip it — clients
  depend on those semantics — but expect it to be much simpler than upstream's.
- Keep the gRPC seam. An in-process shortcut would be faster and would erode
  the property that makes nodestore independently replaceable.

### 4. `k8s-openapi` gives the types and nothing else

Pinned at `v1_33` in the workspace `Cargo.toml`; crate version 0.28 supports
`v1_32`…`v1_36`. Verified it covers **every served group-version** (core/v1,
apps/v1, autoscaling v1+v2, resource v1alpha3/v1beta1/v1beta2, admissionregistration
v1/v1alpha1/v1beta1, …) **plus** `apiextensions_apiserver` (CRD types) and
`kube_aggregator` (APIService types).

What it does **not** have — grepped and confirmed absent:

- protobuf, in any form
- conversion functions between group-versions
- defaulting functions
- `patchStrategy` / `patchMergeKey` metadata
- any validation

Those four are exactly upstream's `conversion-gen`, `defaulter-gen`,
`go-to-protobuf` and struct-tag pillars. **Sourcing them is the core
engineering problem of this component**, and findings 5 and 6 are how.

### 5. The vendored OpenAPI v3 specs carry the patch and apply metadata

`api/openapi-spec/v3/*.json` — 63 files on `release-1.33`, one per
group-version, e.g. `apis__apps__v1_openapi.json` (935KB). Confirmed by
walking `apps/v1`, these carry:

| extension | count in apps/v1 | powers |
|---|---:|---|
| `x-kubernetes-group-version-kind` | 92 | discovery, GVK↔schema map |
| `x-kubernetes-list-type` | 87 | Server-Side Apply (atomic/set/map) |
| `x-kubernetes-patch-strategy` | 27 | Strategic Merge Patch |
| `x-kubernetes-list-map-keys` | 26 | SSA map-list identity |
| `x-kubernetes-patch-merge-key` | 25 | Strategic Merge Patch |
| `x-kubernetes-map-type` | 14 | SSA |
| `x-kubernetes-unions` | 3 | union validation |

Concrete sample — `PodSpec.containers`:
`{list-map-keys: [name], list-type: map, patch-merge-key: name, patch-strategy: merge}`;
`PodSpec.volumes` is `patch-strategy: "merge,retainKeys"`; `tolerations` is
`list-type: atomic`.

**One vendored artifact serves four subsystems**: strategic merge patch,
server-side apply, the discovery GVK map, and the body of the `/openapi/v3`
response itself. Vendor it, do not hand-reconstruct it — same reasoning
`deploy/lib/e2e-full-setup.sh` already applies to the CSI driver.

### 6. Protobuf: generate a codec, not a second set of types

80 `generated.proto` files under `staging/src/k8s.io/api*/`. **`syntax = "proto2"`.**

Verified the load-bearing assumption: **proto field names are camelCase and
identical to the JSON names.** `DaemonSetSpec` → `selector=1, template=2,
updateStrategy=3, minReadySeconds=4, revisionHistoryLimit=6` against
k8s-openapi's `selector, template, update_strategy, min_ready_seconds,
revision_history_limit`. Field numbers **have gaps** (DaemonSetSpec has no 5 —
a removed field), so the numbering must be *parsed*, never inferred from order.

Therefore: **do not generate structs with prost.** That would create a second
type universe requiring a conversion for all ~570 types. Instead, parse the
`.proto` set at build time into a `(message, jsonName) → (field number, wire
type, repeated)` table and drive protobuf encode/decode through serde over the
existing k8s-openapi types. One type universe, one place to be wrong.

The wire envelope for `application/vnd.kubernetes.protobuf` is: 4-byte magic
`k8s\x00`, then a length-delimited `runtime.Unknown`
(`staging/src/k8s.io/apimachinery/pkg/runtime/generated.proto`) whose `raw`
field holds the encoded object.

Prior art exists but was **not** adopted: `k8s-rs-pb` 0.5.0 bridges k8s-openapi
to `rust-protobuf` (wrong protobuf stack — workspace uses prost);
`engenho-kube-proto` 0.53.7 is another Rust apiserver's vendored codec (207
downloads, experimental). Worth reading, not depending on.

### 7. CEL — `cel` 0.14.3, and the three things it lacks

The crate was **renamed**: `cel-interpreter` → `cel`. `cel-interpreter` is
frozen at 0.10.0 (2025-07-23); `cel` is at **0.14.3 (2026-08-15)**, from
`github.com/cel-rust/cel-rust`. `crates/nodescheduler/Cargo.toml:63` still
pins the old `cel-interpreter = "0.10"` — migrate it (Phase 0).

0.13→0.14 is the rework the user means by "optimized": a `Val` trait with
`Box<dyn Val>` throughout to avoid cloning, an `Env` with real overload
resolution mirroring cel-go's, `StructDef`s, proper `Optional` overloads,
`dyn()`, and `Type` no longer carrying lifetimes.

**Present**: parser + interpreter, comprehension macros (`all`/`exists`/
`exists_one`/`map`/`filter`, confirmed in `cel/src/parser/macros.rs`), custom
functions and overloads via `Env`, structs, optionals, regex, chrono
timestamp/duration, `json` feature. Default features `["regex", "chrono"]`.

**Absent, and required for Kubernetes parity — this is real work, not glue:**

- **Cost estimation and runtime cost budget.** Searched the repo: no cost code
  at all. Kubernetes both statically estimates an expression's cost (rejecting
  it at CRD-write time if too expensive) and enforces a runtime budget per
  request. Without this, one CEL rule can wedge the apiserver. Must be built.
- **Kubernetes' extension libraries** (`staging/src/k8s.io/apiserver/pkg/cel/library/`):
  lists (`isSorted`, `sum`, `min`, `max`, `indexOf`, `lastIndexOf`), regex
  (`find`, `findAll`), URLs, `quantity`, IP/CIDR, `format.*`, sets, jsonpatch,
  and the `authorizer` object. All must be written as `Env` overloads.
- **Type checking against a schema.** `Env` does runtime overload dispatch, not
  static type-checking against declarations derived from an OpenAPI structural
  schema. Kubernetes type-checks a CRD's `x-kubernetes-validations` at write
  time. Must be built on top.

Note `crates/nodescheduler/src/framework/plugins/dynamic_resources.rs` already
documents its own gap here: capacity comparisons use plain f64, not upstream's
`Quantity` type. Fixing that is part of writing the quantity extension library,
and it retroactively fixes the scheduler.

### 8. What else exists, and what must be written

- **No Rust `structured-merge-diff`.** Searched crates.io — nothing. Server-Side
  Apply and its `managedFields` ownership tracking must be written from scratch,
  driven by the list-type/map-key metadata from finding 5. Upstream's is ~10k
  LOC of Go, self-contained and well-specified.
- **`json-patch` 4.2.0** — RFC 6902 + RFC 7386. 94M downloads. Reuse for JSON
  Patch and Merge Patch. Strategic Merge Patch is k8s-specific: write it.
- **`jsonschema` 0.49.9** — 82M downloads, actively maintained. A reasonable
  base for CRD structural schema validation, but Kubernetes layers its own
  structural-schema rules on top (pruning, `x-kubernetes-preserve-unknown-fields`,
  `x-kubernetes-embedded-resource`, defaulting). Treat as a component, not a
  solution.

### 9. Streaming is already solved in this codebase

`crates/nodelet/src/server/exec.rs` — read its module header. nodelet does
**not** implement SPDY or WebSocket. It dials the target with the client's
original upgrade request, mirrors the response back, and splices the two raw
upgraded connections (`hyper::upgrade::on` on both sides, `tokio::try_join!`).

The apiserver's `UpgradeAwareProxyHandler` equivalent can be the same splice,
client → nodelet:10250. Neither end parses the streaming protocol, so SPDY and
WebSocket both work for free. This is proven in production here — copy the
pattern rather than reaching for a SPDY crate.

nodelet already serves the routes the apiserver must proxy to
(`crates/nodelet/src/server/routes.rs`): `containerLogs`, `exec`, `attach`,
`portForward`, `stats/summary`, `metrics/resource`, `metrics/cadvisor`.

### 10. The v1.34 bump is nearly free — measured

`CLAUDE.md` warns that bumping `k8s-openapi` "ripples into unrelated breaking
field renames across the whole codebase." That warning is **not accurate for
v1_33 → v1_34.** Diffed the two generated schemas structurally:

- 572 structs exist in both. **Zero removed a field.**
- 24 gained fields (additive; only breaks struct literals lacking
  `..Default::default()`).
- 40 structs exist only in v1_33: **31 in `api/resource`** (the DRA alpha
  types) and **9 in `api/admissionregistration`** (alpha policy types).
- 61 structs are new. At group-version level the only change is `+resource/v1`.

And nothing references the disappearing types typed: all 26 `api::resource::`
hits across the workspace are `apimachinery::pkg::api::resource::Quantity`.
DRA goes through the hand-written raw structs in
`crates/nodescheduler/src/cache/dra.rs` (`RawResourceClaim`, `RawResourceSlice`,
`RawDeviceSelector`, …) and `crates/nodelet/src/runtime/cri/claims.rs` — which
is precisely the workaround `resource.k8s.io/v1` exists to retire.

**Recommendation: target `v1_34`, not `v1_35`/`v1_36`.** It is the minimum bump
that delivers the stated benefit (`resource.k8s.io/v1`), it is provably
additive, and each further bump is mechanical once the machinery exists.
Going to `v1_36` additionally churns `storage/v1alpha1` → gone,
`storagemigration/v1alpha1` → `v1beta1`, and adds `scheduling/v1alpha2`, for no
benefit this plan needs. Changing target later is a one-line feature edit.

### 11. Component wiring — three places, already anticipated

Adding a component is a row in `deploy/lib/components.sh`'s
`NOTK8S_COMPONENTS` table, an optional dependency in
`crates/notk8s/Cargo.toml`, and one line in its `APPLETS` table. Nothing else
learns the name. `components.sh:6` and `deploy/measure.sh:98` **already name
`nodeapiserver`** in anticipation.

The row will be:
`"nodeapiserver|nodeapiserver||want_nodeapiserver|NOTK8S_NODEAPISERVER_PREBUILT|protoc"`
— protoc unconditionally, because the `.proto` codegen is not feature-gated
(same reason `nodestore` carries it; see `any_component_needs_protoc`).

Follow `deploy/lib/nodestore-service.sh` for the service unit; it is the
closest precedent (a control-plane component with TLS, a data dir, and
systemd + OpenRC + supervisor-script variants).

### 12. HTTP stack

Precedent is `crates/nodelet/Cargo.toml`: hyper 1 + `hyper-util` + `tokio-rustls`
0.26 + `http` 1 + `http-body-util`, no framework. nodelet enables only
hyper's `http1`; **the apiserver needs `http2`** — client-go and kubectl
negotiate h2, and watch multiplexing depends on it.

Do not add axum. The apiserver's routes are not a route table, they are a
grammar:
`/apis/{group}/{version}/namespaces/{ns}/{resource}/{name}/{subresource}` with
cluster-scoped and `/api/v1` legacy variants. That is a path parser — which is
what upstream's `APIInstaller` is too, and what
`crates/nodelet/src/server/routes.rs` already does in miniature.

---

## Architecture

New crate `crates/nodeapiserver`. Module layout mirrors the upstream
concept boundaries so the two can be read side by side:

```
crates/nodeapiserver/
  build.rs              # parses vendored .proto + openapi-spec -> codegen (Group A)
  vendor/
    openapi-spec/v3/*.json      # 63 files, release-1.34
    protos/**/generated.proto   # 80 files, release-1.34
  src/
    main.rs / lib.rs
    config.rs           # env-var config, matching the other components' style
    scheme/             # GVK registry, conversion, defaulting  (Group F)
    codec/              # json, yaml, protobuf, content negotiation  (Group B)
    storage/            # etcd v3 client -> nodestore, key layout, RV  (Group C)
    cacher/             # watch cache, bookmarks, selectors  (Group D)
    server/             # listener, handler chain, path grammar, verbs  (Group E)
    registry/           # per-resource Strategy impls  (Group F)
    patch/              # json/merge/strategic patch, SSA + managedFields  (Group G)
    authn/              # x509, SA JWT, OIDC, tokenreview  (Group H)
    authz/              # RBAC, node, webhook  (Group I)
    admission/          # built-in plugins, webhooks, CEL policies  (Group J)
    cel/                # k8s extension libraries, cost budget, typecheck  (Groups J/K)
    apiextensions/      # CRDs  (Group K)
    aggregator/         # APIService proxying  (Group L)
    flowcontrol/        # APF  (Group M)
    audit/              # audit pipeline  (Group M)
    proxy/              # exec/attach/portforward/log splice  (Group N)
    bootstrap/          # PKI, RBAC policy, kubernetes Service  (Group O)
```

**Dependency discipline.** `crates/nodeproxy/Cargo.toml`'s comment states the
rule the project enforces: a component crate's dependency list *is* the
boundary. `nodeapiserver` must not depend on `nodelet`, `nodescheduler` or
`nodecontroller`. It may depend on `nodestore` **only** if a shared protobuf
or client type genuinely warrants it — prefer speaking etcd v3 over the wire
like any other client.

**Write `docs/APISERVER.md` first**, following `docs/CONTROLLER_MANAGER.md`'s
shape: state the end goal precisely, name the honest engineering problem,
group the work, and give each group a status marker maintained as it lands.
That doc is the source of truth for progress; this plan file is not.

---

## Delivery groups

Ordered by dependency, not by value. Group A unblocks everything.

**Phase 0 — prerequisites (own PR, own gates, merges before anything else).**
Bump `k8s-openapi` to `v1_34` across the workspace and fix fallout (finding
10: expect struct literals needing `..Default::default()`, nothing more).
Migrate `crates/nodescheduler` from `cel-interpreter = "0.10"` to `cel = "0.14"`
— the API changed substantially (`Val` trait, `Env` overloads), so
`dynamic_resources.rs`'s compile/execute path needs rework, and its test file
`dynamic_resources_tests.rs` is the safety net. Optionally retire the
`RawResourceClaim`/`RawResourceSlice` workarounds now that `resource.k8s.io/v1`
is typed — but that is a separate PR, not a rider on the bump.

**A. Vendoring + build-time codegen.** Vendor the 63 openapi-spec v3 files and
80 `generated.proto` files from `release-1.34`, with a refresh script recording
the exact upstream ref. `build.rs` emits: the protobuf field-number table
(finding 6), the SMP/SSA schema metadata table (finding 5), and the discovery
GVK map. Everything downstream reads these tables — nothing hand-maintains a
list of types.

**B. Wire formats.** JSON, YAML, and the protobuf codec over the Group A table,
including the `k8s\x00` + `runtime.Unknown` envelope. Content negotiation
(`Accept`/`Content-Type`), `Table` server-side printing, `PartialObjectMetadata`.

**C. Storage over nodestore.** etcd v3 client, `/registry/<group>/<resource>/<ns>/<name>`
key layout, `resourceVersion` == revision, optimistic concurrency → 409,
encryption-at-rest providers (aescbc/aesgcm/secretbox/KMS).

**D. Watch cache.** LIST-then-WATCH init, in-memory serving, bookmarks, RV=0
and consistent reads, label/field selector filtering. Simpler than upstream's
(finding 3) but semantically identical.

**E. Generic server + handler chain + REST endpoints.** The listener (hyper +
h2 + rustls), the path grammar, the full verb set including
`deletecollection` and subresources, `/api`, `/apis`, aggregated discovery v2,
`/openapi/v2` + `/openapi/v3`, `/version`. The handler chain order from
ARCHITECTURE.md §2 is a hard requirement.

**F. Scheme: conversion, defaulting, validation.** The largest handwritten
chunk (~232 kLOC upstream, of which 152k generated). Conversion is only needed
for genuinely multi-version groups — admissionregistration, autoscaling,
certificates, coordination, networking, resource, storage, apiserverinternal,
storagemigration. Validation has no shortcut: it is per-field business logic,
and it is where "fully upstream compatible" is actually earned. Worth watching
upstream's declarative-validation work as a lever, but do not plan on it.

**G. Patch + Server-Side Apply.** `json-patch` for RFC 6902/7386; write
strategic merge patch and structured-merge-diff + `managedFields` (finding 8),
both driven by Group A's metadata table.

**H. Authentication.** x509 client certs, ServiceAccount JWT issuance and
validation, projected/bound tokens, OIDC discovery + JWKS endpoints,
TokenReview, bootstrap tokens, anonymous.

**I. Authorization.** RBAC, the Node authorizer, webhook,
SubjectAccessReview/SelfSubjectAccessReview. Note `nodecontroller` Group I
already does CSR approval/signing — the PKI primitives are in-tree
(`rcgen` 0.13, `p256`, `x509-parser`, `pem`), including the SEC1→PKCS#8
conversion for k3s-style EC keys.

**J. Admission.** The built-in plugin chain (NamespaceLifecycle,
ServiceAccount, DefaultStorageClass, ResourceQuota, LimitRanger, PodSecurity,
DefaultTolerationSeconds, …), mutating + validating webhooks, and
ValidatingAdmissionPolicy / MutatingAdmissionPolicy on CEL. **Build the CEL
cost budget before wiring any CEL-driven admission path** — finding 7; an
unbudgeted CEL evaluator in the request path is a denial-of-service surface.

**K. CRDs (apiextensions).** Dynamic storage registration, structural schemas,
pruning, defaulting, `x-kubernetes-validations` CEL (with the type-checking
from finding 7), conversion webhooks, the `established`/`namesAccepted`
condition machinery.

**L. Aggregation layer.** `APIService` objects, `ServiceResolver`, reverse
proxying, discovery merge, availability conditions.

**M. APF, audit, observability.** FlowSchema/PriorityLevelConfiguration
queueing; policy-driven audit with the four stages and its backends;
`/metrics`; `/healthz`, `/readyz`, `/livez` with per-check verbose output
(`deploy/setup-control-plane.sh` already polls `/readyz?verbose`).

**N. Streaming and proxy subresources.** exec/attach/port-forward/log proxied
to nodelet:10250 using the splice from finding 9; node/service/pod proxy
subresources.

**O. Cluster bootstrap — the k3s replacement half.** All four user-confirmed
items: cluster PKI generation (CA, serving cert, SA signing keypair, per-component
client certs, kubeconfig emission), the ~90 `system:` ClusterRoles/Bindings
from upstream's `bootstrappolicy`, the `kubernetes` default Service + endpoint
reconciler, and CoreDNS + flannel manifests moved into `deploy/`. Then rewrite
`deploy/setup-control-plane.sh` to stop installing k3s at all, and add the
`components.sh` row + `notk8s` applet from finding 11.

---

## Getting signal earlier than the cutover

The chosen model means the cluster e2e suite cannot run against `nodeapiserver`
until Groups B–F and H–J are simultaneously working. That is a long time
without a gate, and this project's entire track record (`docs/E2E_FINDINGS.md`)
says design review does not catch what real traffic catches.

There is a precedent here that gives real signal without any dual-path deploy
code: **`deploy/lib/test/cases/datastore.sh` drives the real gRPC API with
`grpcurl` against a throwaway `nodestore`, never the running cluster's own.**

Do the same for the apiserver: a case file that boots a throwaway
`nodeapiserver` + `nodestore` pair on a scratch port and data dir, and drives
it with **real `kubectl` and real `curl`** — `kubectl --server=... get/create/
patch/apply/delete`, watch streams, discovery, `--v=8` to see negotiation.
That is a test rig, not a deployment path; nothing ships it, nothing has to be
deleted later. It can start returning verdicts as soon as Groups B, C and E
produce a single working resource, and it is the natural home for
upstream-compatibility assertions (protobuf round-trip, RV semantics, 409 on
stale write, SSA field ownership).

Recommend building that harness as part of Group E rather than at the end.

---

## Verification

Per `CLAUDE.md`'s merge protocol, in order, no gate skippable:

1. **Branch per group.** Never commit to `main`.
2. **Write the test that would have caught it** — a case in
   `deploy/lib/test/cases/*.sh` that fails without the change. For this
   component, most of that lands in the throwaway-rig case file above;
   `cases/retry_backoff.sh` is the shape to copy for anything behavioural.
3. **Open the PR** before asking CI for anything, so CodeRabbit gets its pass.
4. **Build in CI** — `gh workflow run build.yml --ref <branch> -f profile=debug
   -f arch=aarch64`. Do **not** build locally; this box OOMs on a release build
   and has no toolchain. Memory notes also say do not run `build.yml` before an
   `e2e.yml` dispatch — e2e already builds.
5. **e2e against real binaries** — `gh workflow run e2e.yml --ref <branch> -f
   only=<substrings>` while iterating. `--only=` matches **test function
   names, not filenames** (grep `register_test` in the case file). A filtered
   run is not merge-worthy on its own; a full unfiltered suite is what green
   means.
6. **Merge, then rebase** every other open PR onto the new `main` and re-run
   their gates.

Standing constraints from memory that apply here:

- **Never run e2e locally** — always dispatch `e2e.yml`, even with a live
  cluster on this box.
- **Do not merge partial multi-phase work.** Groups A–O are one arc; a group's
  own gates being green is not sufficient to land it if the arc is incomplete
  and the cluster cannot come up. Practically: this needs a long-lived
  integration branch that groups merge into, with `main` only taking the whole
  thing once a cluster actually boots on it. Confirm this with the user before
  the first group merges — it is the one place the chosen cutover model
  collides with an existing standing rule.
- The nftables modules missing on this kernel (`nft_fib`, `nft_numgen`,
  `nft_hash`) make a green run here mean more than a green run on a GitHub
  runner — see `crates/nodeproxy/src/svc.rs`'s `probe_caps()`.

Final acceptance: a cluster bootstrapped by `./deploy/bootstrap-source.sh
--with-cri` with **no k3s installed at all**, passing the full unfiltered
`test-e2e.sh` suite (~142 tests) including the real CSI and DRA reference
drivers from `deploy/lib/e2e-full-setup.sh`.
