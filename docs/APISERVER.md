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

**A. Vendoring + build-time codegen** — **not started**. Vendor 63
openapi-spec v3 files + 80 `generated.proto` files from `release-1.34`, with
a refresh script recording the exact upstream ref. `build.rs` emits the
protobuf field-number table, the SMP/SSA schema metadata table, and the
discovery GVK map.

**B. Wire formats** — **not started**. JSON, YAML, protobuf codec
(`k8s\x00` + `runtime.Unknown` envelope) over Group A's table. Content
negotiation, `Table` printing, `PartialObjectMetadata`.

**C. Storage over nodestore** — **not started**. etcd v3 client,
`/registry/<group>/<resource>/<ns>/<name>` key layout, `resourceVersion` ==
nodestore revision, optimistic concurrency → 409, encryption-at-rest
providers.

**D. Watch cache** — **not started**. LIST-then-WATCH init, in-memory
serving, bookmarks, RV=0/consistent reads, label/field selector filtering.

**E. Generic server + handler chain + REST endpoints** — **not started**.
hyper + h2 + rustls listener, path grammar, full verb set incl.
`deletecollection` and subresources, `/api`, `/apis`, aggregated discovery
v2, `/openapi/v2` + `/openapi/v3`, `/version`. Handler-chain order
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
