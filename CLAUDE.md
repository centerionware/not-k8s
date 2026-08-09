# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this project is

`not-k8s` replaces the node side of Kubernetes with two lean event-driven Rust
binaries — `nodelet` (kubelet, the node agent) and `nodeproxy` (kube-proxy:
Service/ClusterIP/NodePort routing via nftables) — while keeping a real
(stripped) k3s control plane for 1:1 `kubectl`/CRD compatibility. The two are
separate binaries and separate services with no ordering between them, for the
same reason kubelet and kube-proxy are separate upstream: a node can swap in
Cilium or a real kube-proxy, or run none at all
(`bootstrap-source.sh --proxy=none`), without touching the node agent. The pitch: kubelet's idle cost (PLEG polling,
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

cargo build -p nodeproxy                  # the Service proxy — no features, ever
cargo test -p nodeproxy                   # its nft ruleset tests (need CAP_NET_ADMIN, else self-skip)
```

`crates/nodeproxy` deliberately shares **none** of nodelet's dependency tree —
no `cri` feature, no tonic/prost/zbus. If a change wants one of those there,
that's the signal the split is being eroded; the boundary is enforced by
`crates/nodeproxy/Cargo.toml` and nothing else.

**Don't build locally on a constrained host** — use GitHub Actions.
`.github/workflows/build.yml` (manual `workflow_dispatch`) compiles both
crates, runs the unit tests, and uploads the binaries as run artifacts, with
`profile` (debug/release/both) and `arch` (x86_64/aarch64/armv7l/all) inputs
and no e2e stage. Download them with `gh run download <run-id> -n
notk8s-<arch>-<profile>` and point a local deploy at them via
`NOTK8S_NODELET_PREBUILT` / `NOTK8S_NODEPROXY_PREBUILT` — the same prebuilt
seam `release.yml`'s e2e shards use, so no toolchain is installed on the
device.

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
containerd/runc, CNI, and `nodelet` + `nodeproxy` as two independent
systemd/OpenRC services (`deploy/lib/nodelet-service.sh`,
`deploy/lib/nodeproxy-service.sh`). `--proxy=none` skips `nodeproxy`
entirely, including its nftables/br_netfilter host setup. `deploy/lib/*.sh`
are the individual concern modules these two entry points source (toolchain
setup, control-plane install, container runtime, CNI, nodelet build/service
lifecycle) — read the specific `lib/` file for a concern rather than the whole
entry-point script.

## E2E testing

```bash
./deploy/test-e2e.sh                              # full suite against an already-running cluster
./deploy/test-e2e.sh --only=<substring>            # substring match on test *function name*, not filename
./deploy/test-e2e.sh --only=<substring1>,<substring2>  # comma-separate to match several tests at once
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

`harness.sh`'s `run_all_registered_tests()` automatically reorders any test
that restarts nodelet (`nodelet_restart_with_env`) or another host service
(containerd, swap) to the very end of the run — detected by grepping each
test function's real source (`declare -f`), not a hand-maintained list, so
new tests doing this get deferred automatically. Found live in CI: mixing
these with ordinary pod-creation tests caused flaky "pod never reached
Running" failures that moved to a different test on every rerun (node
briefly NotReady, CSI/DRA plugins re-registering, volumes re-materializing).

**Iterating on a known e2e failure: use `.github/workflows/e2e.yml` (manual
`workflow_dispatch`, `only` input — same comma-separated matching as above),
not the full `release.yml` pipeline.** `gh workflow run e2e.yml --ref main -f
only=<pattern1>,<pattern2>` runs just the matching tests in one dispatch
(~5-7 min: build+deploy+CSI/DRA-driver setup dominate, not test count)
against a fresh cluster, instead of the full pipeline's `build-and-test` →
full unfiltered `e2e` → `build-release` → `publish-release` chain (30-40+
min). Fix, dispatch, read the result, repeat — only dispatch the full
`release.yml` once the targeted runs are believed green, to confirm
end-to-end (including that the fix didn't regress unit tests or anything
the `--only` filter excluded) and actually attempt a release.

## Verifying a PR end to end before merging it

The loop below builds a branch in CI and runs targeted e2e against the
real binaries on a local box, so a change is proven end to end *while it
is still a PR*. Use it for anything with a runtime surface — a new
component (`nodeapiserver`, `nodescheduler`, …) especially, where "it
compiles" says almost nothing.

**1. Push the branch and build it.** `build.yml` is `workflow_dispatch`,
so it must exist on the default branch to be dispatchable — but it runs
the copy from whatever `--ref` you give it, against that ref's code.
Match `arch` to the machine you'll test on; an x86_64 artifact is useless
on an aarch64 phone VM.

```bash
gh workflow run build.yml --ref <branch> -f profile=debug -f arch=aarch64
gh run watch "$(gh run list --workflow=build.yml --limit 1 --json databaseId --jq '.[0].databaseId')"
```

**2. Download the artifact — not to `/tmp`.** `/tmp` here is a ~1GB
RAM-backed tmpfs and a debug `nodelet` alone is ~350MB. A truncated
download can still be a valid-looking ELF that runs (the cut lands in
trailing debug sections), so this fails silently rather than loudly.

```bash
D=/home/droid/nk8s-artifacts && rm -rf "$D" && mkdir -p "$D"
gh run download <run-id> -n notk8s-aarch64-debug -D "$D" && chmod +x "$D"/*
```

**3. Deploy with the prebuilt seam.** No toolchain is installed and
nothing is compiled on-device — the whole point on a host that OOMs on a
release build.

```bash
sudo -E NOTK8S_NODELET_PREBUILT="$D/nodelet" NOTK8S_NODEPROXY_PREBUILT="$D/nodeproxy" \
  ./deploy/bootstrap-source.sh --with-cri
```

To swap binaries into an already-running deployment, `install` them over
`bin/` and restart the units — much faster than re-bootstrapping:

```bash
sudo install -m0755 "$D/nodeproxy" bin/nodeproxy && sudo systemctl restart nodeproxy
```

**4. Run only the relevant tests.** `--only` matches test *function*
names, comma-separated.

```bash
export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
./deploy/test-e2e.sh --only=clusterip,nodeport,hairpin
```

**5. Merge once it's green**, then let `release.yml` run the full gate.

### Things that will bite you here

- **A stale `flanneld` survives a re-bootstrap.** The installer leaves an
  already-active service alone, so a flanneld still watching a previous
  cluster keeps running and never writes `/run/flannel/subnet.env`. Every
  pod then sits `Pending` with `loadFlannelSubnetEnv failed`. Fix:
  `sudo systemctl restart flanneld`.
- **`nft` lives in `/usr/sbin`**, which is not on an unprivileged PATH.
  `command -v nft` fails on a host where `sudo nft` works fine — do not
  gate anything on it.
- **`pkill -f test-e2e.sh` kills your own shell**, because the pattern
  matches the command line running it. Use `pkill -f 'test-e2e[.]sh'`.
- **Unbounded `git` repack will OOM-kill this box.** Confirmed three
  times from `dmesg`: the victim was `git` at ~1.1GB RSS, not the build.
  `.git` had accumulated 14,959 loose objects / 347MB against a 14MB
  pack, and it was self-perpetuating — auto-gc triggers, gets OOM-killed,
  the objects stay loose, the next operation needs more. Fixed by
  `git gc --prune=now` (347MB → 0 loose, `.git` 362MB → 13MB) with
  repo-local caps now committed to `.git/config`:
  `pack.windowMemory=32m`, `pack.deltaCacheSize=16m`, `pack.threads=1`.
  If git starts getting killed again, check `git count-objects -vH`
  first.
- **Don't let `gh run download` buffer an artifact here.** It holds the
  whole zip in memory, and the debug pair is ~490MB. Stream it instead:
  `curl -sL -H "Authorization: token $(gh auth token)" -o a.zip
  .../actions/artifacts/<id>/zip`, then extract the one binary you need
  with a chunked reader (`unzip` isn't installed).
- **Long runs need `setsid nohup … &` with `disown`** and a log file on
  disk; poll the log rather than holding a foreground command open.
- **This kernel is missing nftables modules** (`nft_fib`, `nft_numgen`,
  `nft_hash`) — see `crates/nodeproxy/src/svc.rs`'s `probe_caps()`. That
  is a feature of the test host, not a bug: it is the only place these
  degradation paths get exercised, so a green run here means more than a
  green run on a GitHub runner.

## CI/CD (`.github/workflows/`)

`build.yml` is the one to reach for during development: manual
(`workflow_dispatch`), builds both crates and runs the full unit tests, no
e2e, no release, and uploads the binaries as run artifacts (`profile`:
debug/release/both; `arch`: x86_64/aarch64/armv7l/all). This exists because
this repo is developed partly on hosts that can't build it — see the Build
section above for the download-and-deploy flow.

`release.yml` is the real pipeline — manual (`workflow_dispatch`) only; see
its own top comment for why it isn't push-triggered (a real, unexplained
GitHub-side issue, not a deliberate design choice like `e2e.yml`/`profiling.yml`
being manual). Four sequential stages, each gating the next: `build-and-test`
(debug build + full unit tests, both feature sets) → `e2e` (full suite against
real CSI/DRA reference drivers) → `build-release` (debug+release profiles ×
x86_64/aarch64/armv7l, concurrently) → `publish-release` (bumps the version
branch, tags, publishes a GitHub Release with all 6 binaries + `deploy.tar.gz`,
compiles and publishes the standalone installer scripts). Not triggered on
`pull_request` either — the e2e stage needs real sudo/cluster access that must
never run against untrusted PR code; PRs get CodeRabbit review only
(`.coderabbit.yaml`).

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

**Two binaries, two crates.** `crates/nodelet` is the node agent;
`crates/nodeproxy` is the Service proxy (`svc.rs`: Service + EndpointSlice
watch, one `inet not_k8s_svc` nftables table rebuilt atomically per event,
no periodic resync). They share no code and no config — `nodeproxy` reads
`NODEPROXY_IP_FAMILY`/`NODEPROXY_LB_METHOD` (still accepting the pre-split
`NODELET_*` spellings), and `nodelet` warns and ignores if it sees those
set. e2e coverage is `deploy/lib/test/cases/service_proxy.sh`.

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
