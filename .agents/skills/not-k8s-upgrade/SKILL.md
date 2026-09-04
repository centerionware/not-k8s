---
name: not-k8s-upgrade
description: Assess or implement a Kubernetes minor-version upgrade in not-k8s, including upstream API and behavior deltas, cross-component compatibility, migration tests, and conformance evidence. Use when changing the Kubernetes target or planning parity work; do not activate for ordinary bug fixes.
---

# Upgrade Kubernetes compatibility

Read [AGENTS.md](../../../AGENTS.md). Honor whether the user requested an
assessment, an implementation, or a live-cluster upgrade: those are different
scopes. Planning an upgrade does not authorize changing binaries or an installed
cluster. The project's next intended target after apiserver stabilization is
1.37; do not mix that upgrade into unfinished stabilization unless asked.

## Establish the baseline and target

Read the [repository upgrade map](references/upgrade-map.md) before choosing
files or commands. Record the current code SHA, vendored upstream ref, typed
schema feature, comparison-server version, runtime/driver versions, and existing
validation evidence. A `v1_34` feature or `/version` response is not a complete
inventory of implemented Kubernetes 1.34 behavior.

Verify the requested release using the official sources in the map. Pin an
actual upstream patch tag/commit and artifact digests. Do not vendor a moving
release branch or invent a future `k8s-openapi` feature. Verify the published
Rust crate's supported schema versions separately from Kubernetes release
availability. If bindings lag the desired release, identify the affected API
surface and a bounded adapter/vendor option; do not silently claim the target
while continuing to use old types that drop new fields.

For 1.34 to 1.37, examine changes in 1.35, 1.36, and 1.37. Source-level migration
and rolling upgrades of installed clusters are separate questions. Determine
supported skew, persisted-data compatibility, upgrade order, and rollback
limits from the target release; do not assume an in-place three-minor jump is
safe because the new code compiles.

## Build a compatibility ledger before broad edits

Use one maintained ledger in the upgrade branch, with rows containing:

`feature/API | target version and gate/default | upstream source at pinned ref |
owner crate | current implementation | required change | regression/conformance
test | evidence SHA/run | status | CPU/RSS risk`

Statuses must distinguish unknown, absent, partial, implemented-but-unverified,
verified, and explicitly out of scope. Reuse existing component gap documents
where appropriate instead of creating competing truth sources.

Inventory additions, removals, promotions, and changed semantics, including
defaults and feature-gate transitions. Organize by observable behavior, not
just struct names. Include the cross-component surfaces in the upgrade map.
Read upstream implementation/tests when release notes or schemas do not answer
the semantic question. KEP intent alone does not prove what shipped.

Define the compatibility denominator explicitly. Report conformance test counts
separately from the feature ledger. Passing all current repository tests or the
upstream conformance subset does not prove 100% of Kubernetes features. Keep
alpha/opt-in features, OS/architecture support, external provider/driver features,
and unsupported configurations visible; do not remove inconvenient rows to
improve a percentage.

## Implement reviewable changes

Preserve the user's integration-branch arrangement. Separate generated input
refreshes from behavioral changes for review, while keeping dependent pieces
coherent. A mechanically additive schema change can still expose an API whose
controller, admission, or runtime behavior is absent.

- Diff OpenAPI and protobuf inputs structurally. Check removed/renamed fields,
  defaults, requiredness, enums, integer widths, protobuf wire numbers, list/map
  semantics, and `x-kubernetes-*` metadata used by pruning and SSA.
- Update vendor provenance and binding/dependency versions coherently. Inspect
  the refresh script before running it; it replaces existing vendor directories.
  Use an isolated clean checkout/staging area and verify complete downloads
  before replacing authoritative inputs. Generate outputs in CI; never hand-edit
  generated lookup tables to make a new type appear supported.
- Implement behavior in the owning crate. Keep `notk8s` packaging-only and
  preserve component replacement and split/combined equivalence.
- Test compatibility of existing persisted objects, CRD `storedVersions`,
  conversion, SSA ownership, defaulting, validation, and status/subresource
  updates. Do not make old data unreadable just to accept a new API version.
- Follow event-driven invariants. A new feature needs dependency events and
  recovery paths, not another unconditional periodic scan.
- Update advertised discovery/version support only with honest implementation
  status and validation. Do not equate schema availability with conformance.

## Prove behavior and efficiency

Use [not-k8s-ci](../not-k8s-ci/SKILL.md) for economical CI iteration and exact
run tracking. Run scoped checks per change, targeted real-cluster regressions,
then the full repository suite on the final target. Add target-version upstream
conformance testing using the current official procedure. Preserve raw results,
skips, failures, image/version identity, and cluster configuration. Do not claim
certification merely from running a test runner.

Use a pinned upstream kube-apiserver as a differential reference for request/
response semantics. Compare validation, status codes, defaults, patch/SSA,
watch/relist behavior, authorization, and deletion while accounting for
server-assigned timestamps, UIDs, and revisions. Include real controllers,
containerd, CSI/DRA drivers, and workloads where the behavior spans components.

Exercise restart/recovery and existing-data migration in disposable clusters
before an authorized live upgrade. Verify supported mixed-version arrangements
and state precisely which upgrade and rollback paths were tested.

Use [not-k8s-performance](../not-k8s-performance/SKILL.md) for CPU/memory evidence.
Keep correctness and efficiency gates separate: a faster implementation that
misses watch events, delays convergence, or omits features fails compatibility.
Do not discard features to preserve a favorable benchmark. Document measured
regressions and resolve them against the agreed budget before claiming success.

Finish with the ledger, exact supported/tested versions and configurations,
remaining gaps, full-suite/conformance evidence, and comparable performance
results. No feature-complete or efficiency-dominance claim without that evidence.
