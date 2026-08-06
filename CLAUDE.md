# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this project is

`not-k8s` replaces kubelet (the node agent) with `nodelet`, a lean event-driven
Rust binary, while keeping a real (stripped) k3s control plane for 1:1
`kubectl`/CRD compatibility. The pitch: kubelet's idle cost (PLEG polling,
cAdvisor housekeeping, per-component watch caches, iptables sync) lives almost
entirely on the node side, not the apiserver — replace only that, keep
everything else real. Goal is genuine kubelet feature parity (not a
single-node-only toy), verified against `docs/GAP_CLOSURE.md`'s live checklist,
not claimed from the design doc alone.

Read `docs/ARCHITECTURE.md` (design rationale), `docs/GAP_CLOSURE.md`
(feature-by-feature parity status — what's ✅/🟡/❌ and why), and
`docs/E2E_FINDINGS.md` (numbered findings, each a real bug found and fixed by
live-testing against real infrastructure, with root cause) before assuming
something is or isn't implemented — grep first, GAP_CLOSURE second, don't
guess from the architecture doc alone.

## Build

Two Cargo features gate the runtime:
- Default (no `--features cri`): mock runtime, no real containers — fast to
  build/test, used for pure logic.
- `--features cri`: the real containerd/CRI runtime — needs `protoc` on PATH
  (gRPC codegen for CRI/CSI/DRA/device-plugin/plugin-registration protos).

```bash
cargo build -p nodelet                    # mock runtime, debug
cargo build --release --features cri -p nodelet   # real binary, optimized
cargo test -p nodelet                     # mock-runtime unit tests
cargo test -p nodelet --features cri      # cri-gated unit tests too
```

**Release profile is expensive on purpose**: `Cargo.toml`'s `[profile.release]`
uses `lto=true, codegen-units=1` for the smallest edge binary — a real build
takes ~5x longer than debug. Never needed for iterating on correctness (use a
debug build); CI's e2e stage builds with `NOTK8S_BUILD_PROFILE=debug` and its
release-artifact stage overrides to `CARGO_PROFILE_RELEASE_LTO=thin
CODEGEN_UNITS=16` purely for CI turnaround (edge devices still get full LTO
from a local `cargo build --release`).

**Memory-constrained build hosts** (this repo has been developed partly on a
resource-constrained phone VM): `deploy/lib/nodelet-build.sh`'s
`build_nodelet()` auto-detects <4GB RAM and falls back to
`CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16` on its
own — don't run a bare `cargo build --release --features cri` on such a host;
use the deploy script or set those env vars yourself first. `CARGO_BUILD_JOBS=1`
is also worth forcing on such hosts; a full-parallelism build has been known to
OOM the whole VM, not just the build.

Run a single test: `cargo test -p nodelet --features cri <test_name_substring>`
(cargo's own substring matching — no special harness).

## Running the real thing

```bash
./deploy/bootstrap-source.sh --with-cri     # build from source, install everything
./deploy/bootstrap-release.sh --with-cri    # fetch a prebuilt binary instead (no Rust toolchain)
```

Both install k3s (`--disable-agent`, stripped control plane only),
containerd/runc, CNI, and nodelet as a systemd/OpenRC service. `deploy/lib/*.sh`
are the individual concern modules these two entry points source (toolchain
setup, control-plane install, container runtime, CNI, nodelet build/service
lifecycle) — read the specific `lib/` file for a concern rather than the whole
entry-point script.

## E2E testing

```bash
./deploy/test-e2e.sh                        # full suite against an already-running cluster
./deploy/test-e2e.sh --only=<substring>      # substring match on test *function name*, not filename
```

Test cases live in `deploy/lib/test/cases/*.sh`, one file per feature area,
each calling `register_test` for its `test_*` functions. Harness is
`deploy/lib/test/harness.sh`. A test that needs infrastructure this bash-only
suite can't stand up itself (a real GPU, a real DRA/CSI driver) either skips
cleanly with a clear message, or — preferred, and what CI now does — the
harness installs a real reference driver first:
`deploy/lib/e2e-full-setup.sh` installs `kubernetes-csi/csi-driver-host-path`
(fetches its own real upstream deploy tooling, not a hand-reconstructed copy —
see that script's own doc comment for why) and
`kubernetes-sigs/dra-example-driver` (via Helm), the same reference drivers
real Kubernetes e2e/conformance uses, then sets the `TEST_CSI_*` env vars the
gated tests key off. Manual-only test cases stay manual (documented, not
automated) only when genuinely nothing in this harness could stand up the
prerequisite.

Env vars `NOTK8S_E2E_MAX_FAILURES` (stop the whole run after N failures instead
of always running all ~142 — CI sets 3) and `NOTK8S_E2E_DEBUG_ON_FAIL` (print
node conditions/taints + nodelet's log tail right after each failure — CI sets
1) make a systemic break fail fast with real signal instead of burning 30+
minutes re-discovering the same root cause across the rest of the suite.

## CI/CD (`.github/workflows/`)

`release.yml` is the real pipeline, triggered on every push to `main` (direct
push or PR merge — **not** on `pull_request`, since the e2e stage needs real
sudo/cluster access that must never run against untrusted PR code). Four
sequential stages, each gating the next: `build-and-test` (debug build + full
unit tests, both feature sets) → `e2e` (full suite against real CSI/DRA
reference drivers) → `build-release` (debug+release profiles ×
x86_64/aarch64/armv7l, concurrently) → `publish-release` (bumps the version
branch, tags, publishes a GitHub Release with all 6 binaries + `deploy.tar.gz`,
compiles and publishes the standalone installer scripts). PRs get CodeRabbit
review only (`.coderabbit.yaml`) — no tests run until merge.

`e2e.yml` is a manual (`workflow_dispatch`) on-demand utility for debugging just
the e2e suite (optionally `--only=<pattern>`) without the rest of the pipeline.
`profiling.yml` is a manual job comparing nodelet's idle CPU/RSS against stock
k3s's real bundled kubelet, publishing results to the `profiling-results`
branch.

**Version tracking**: the `version` branch holds a single `VERSION` file
(`MAJOR.MINOR.PATCH`) — `deploy/lib/version-bump.sh` reads the current value
for the release in progress, then commits the incremented PATCH back for next
time. Bump MAJOR/MINOR by hand (edit `VERSION` on that branch directly) ahead
of a release that should carry one; this is independent of `Cargo.toml`'s own
`version` field, which doesn't need to track it.

**Standalone installer**: `deploy/lib/compile-install-script.sh` generates the
tiny script published to the `install-scripts` branch (`install.sh` — always
overwritten, resolves latest; `install-v<version>.sh` — never overwritten, one
per release). It fetches a separate `deploy.tar.gz` release asset rather than
embedding data inline — a self-extracting version that read its own trailing
bytes worked as a saved file but silently extracted nothing when actually
piped (`curl | bash`), because bash's own script-reading from a pipe consumes
stdin unpredictably. Don't reintroduce that pattern.

**Known CI gotcha**: GitHub's `ubuntu-latest` runners ship Docker with its own
bundled containerd already running, whose shipped config disables the CRI
plugin (Docker doesn't use CRI). `deploy/lib/container-runtime.sh` strips `cri`
out of `disabled_plugins` and restarts an already-running containerd
unconditionally now — if CRI calls start failing with `"unknown service
runtime.v1.RuntimeService"` again, that's where to look first.

## Architecture

**Split between `mock` and `cri` runtimes** (`crates/nodelet/src/runtime/`):
almost everything in `src/*.rs` (pod reconciliation, probes, eviction, cpu/
memory managers, node status) is runtime-agnostic, working against the
`PodRuntime` trait. The `cri` feature's real implementation lives under
`runtime/cri/` (one file per concern: `container_create.rs`,
`volumes_resolve.rs`, `sandbox.rs`, `claims.rs` for DRA, etc.) and is the only
part that talks to containerd. When changing pod/container lifecycle logic,
check whether it belongs in the trait-generic layer or the CRI-specific one.

**Reconciliation is watch-driven, not polled**: `pods.rs`'s `PodController`
only calls `ensure_pod()` in response to a watch event on the Pod object
itself (or a referenced ConfigMap/Secret changing) — there is no periodic
"resync everything" loop. Anything that changes a pod's real state *without*
going through a Pod object mutation (a probe-triggered restart is the example
that bit this project for real — see `docs/E2E_FINDINGS.md` finding #19) must
explicitly re-trigger `ensure_pod()` itself; it will not happen automatically.

**Plugin registration is one shared protocol** (`plugin_registry.rs`): CSI
drivers, device plugins, and DRA drivers all register through the same
Unix-socket-watching handshake (`GetInfo`/`NotifyRegistrationStatus`), each
routed by `PluginInfo.type` to its own subsystem (`runtime::csi`,
`device_plugins.rs`, `dra.rs`). New plugin-protocol-driven features should
reuse this rather than inventing a new discovery mechanism.

**k8s-openapi is pinned to the `v1_33` schema feature** (`Cargo.toml`) —
deliberately, not an oversight. Bumping it to get a newer API type (this came
up for `resource.k8s.io/v1`, which only exists from `v1_34` onward) ripples
into unrelated breaking field renames across the whole codebase. The
established workaround: fetch the resource via a raw request into a small
hand-written struct (see `runtime/cri/claims.rs`'s `RawResourceClaim`) instead
of bumping the schema — same pattern `container_support.rs`'s
`resolve_service_account_token()` already uses for a subresource k8s-openapi
doesn't generate a helper for either.

**Everything is found-and-fixed against real infrastructure, not assumed
correct from reading the spec.** The pattern this whole project follows: when
a feature "looks done," stand up the real reference implementation (real CSI
driver, real DRA driver, a real GitHub Actions runner) and watch it actually
run, rather than trusting that following the K8s API docs was sufficient.
`docs/E2E_FINDINGS.md`'s numbered findings are the record of every time that
surfaced a real bug the design/code review alone missed — read a few before
assuming a subsystem's existing tests already prove it works end-to-end.
