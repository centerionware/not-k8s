# Version-upgrade source map

Recheck these paths on the active branch. These are navigation aids, not a
second version manifest. Read the files before proposing a refresh command.

## Repository inputs

| Surface | Source of truth / first files |
| --- | --- |
| Rust typed Kubernetes objects | Root `Cargo.toml` (`k8s-openapi` version/schema feature) and `Cargo.lock`; downstream serde/raw adapters across crates |
| API schema and wire metadata | `crates/nodeapiserver/vendor/REF`, `vendor/refresh.sh`, `vendor/openapi-spec/v3/`, `vendor/protos/` |
| Generated discovery/protobuf/SSA tables | `crates/nodeapiserver/build.rs`, `build/`, `src/codegen.rs`; generated files are written to Cargo OUT_DIR |
| Upstream comparison binaries | `crates/nodebootstrap/src/targets/upstream.rs` (`K8S_VERSION`), fetch verification and setup |
| Served version identity | `crates/nodeapiserver/src/server/version.rs` and its build inputs |
| API semantics | `nodeapiserver/src/server/rest/`, `scheme/`, `patch/`, `cacher/`, `admission/`, `authn/`, `authz/`, `flowcontrol/`, `apiextensions/` |
| Scheduler/controller behavior | `nodescheduler/src/`, `nodecontroller/src/controllers/`, their shared watches and work queues |
| Node and runtime interfaces | `nodelet/src/runtime/`, `nodelet/proto/`, `nodelet/build.rs`; CRI, CSI, DRA, plugin registration, device plugins, PodResources |
| Etcd wire/storage semantics | `nodestore/src/command.rs`, `store.rs`, `consensus.rs`, `replication/`, `proto/`; apiserver etcd-client proto synchronization |
| Bootstrap compatibility | `nodebootstrap/src/targets/`, `pki.rs`, `rbac.rs`, `manifests.rs`, `containerd.rs`, `cni.rs`, `services.rs` |
| Real integrations and version pins | `nodebootstrap/src/e2e/`, `.github/workflows/e2e.yml`, reference-driver setup and manifests |

The baseline inspected when this skill was introduced used `k8s-openapi` 0.28
with `v1_34`, vendored `release-1.34`, and upstream comparison binary v1.34.11.
These are historical observations, not instructions to retain those versions.

`vendor/refresh.sh` currently removes its OpenAPI/proto directories before
fetching replacements and defaults to a moving release branch. Do not invoke
it blindly on the only good copy. Its source selection includes API staging
repositories and kube-aggregator; verify coverage against the target tree so
an API does not enter discovery without a usable wire schema.

## Behavioral inventory

Check target release changes in these areas, including their defaults/gates:

- API availability, discovery/OpenAPI negotiation, serialization and protobuf;
  CRUD, subresources, selectors, pagination, LIST/WATCH consistency and bookmarks.
- Validation/defaulting/pruning, CEL libraries/options/cost behavior, admission
  plugins and webhooks, authentication, RBAC/bootstrap grants, APF, audit.
- CRD versions/conversion/storage migration and SSA/managedFields across versions.
- Scheduling, resource accounting, topology/affinity, preemption, disruption,
  workload controllers, GC/finalizers, namespace and ServiceAccount controllers.
- Pod lifecycle/status, sidecars, ephemeral containers, resize, probes, eviction,
  node conditions, cgroups/resource managers, service-account tokens, logs/exec.
- Services/EndpointSlices, dual stack, DNS and proxy behavior; volumes, CSI,
  DRA, device plugins, containerd/CRI and CNI requirements.
- Upgrade/restart recovery, storage compatibility, component/version skew,
  feature-gate removals and emulation behavior if applicable.

CSI, CNI, Gateway API, external providers, and third-party drivers have their
own release/support contracts. Do not assign them a Kubernetes minor version
by assumption or count an external implementation as code this project owns.

## Primary upstream sources

Use versioned documentation and exact tags/commits for evidence. Fetch once
and search locally where that avoids repeated expensive remote reads.

- [Release availability and patch history](https://kubernetes.io/releases/)
- [Kubernetes source, release tags, and changelogs](https://github.com/kubernetes/kubernetes)
- [API removal/migration guidance](https://kubernetes.io/docs/reference/using-api/deprecation-guide/)
- [Deprecation policy](https://kubernetes.io/docs/reference/using-api/deprecation-policy/)
- [Component version-skew policy](https://kubernetes.io/releases/version-skew-policy/)
- [Enhancements and KEP history](https://github.com/kubernetes/enhancements)
- [CNCF conformance procedure](https://github.com/cncf/k8s-conformance/blob/master/instructions.md)

Resolve these to the chosen release at execution time. For behavior not stated
in docs, inspect the corresponding upstream implementation and tests at that
same ref. Do not make a compatibility decision from a search snippet alone.
