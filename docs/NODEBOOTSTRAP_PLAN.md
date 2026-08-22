# nodebootstrap — plan

## Context

This supersedes the narrow scope `APISERVER.md`'s Group O entry gave
`clusterbootstrap` on 2026-08-21. That entry was right that PKI/RBAC/
kubeconfig/the `kubernetes` Service reconciler don't belong inside
`nodeapiserver` itself, and right that it needs its own long-lived
integration branch. It was too narrow about what else belongs in the same
crate, and it left `nodeapiserver` blocked on a chicken-and-egg problem:
Group O can't be tested or merged until `nodeapiserver` exists to consume
it, but `nodeapiserver` can't get real cluster signal until Group O exists
to bootstrap a cluster for it to run in.

Decided 2026-08-22, replacing that entry:

1. **One crate, `nodebootstrap`**, not two. It replaces
   `bootstrap-source.sh`/`bootstrap-release.sh`/most of `deploy/lib/*.sh` —
   toolchain checks, containerd install, CNI/flannel install (skippable, so
   Cilium etc. can be used instead), per-component skip flags, build-from-
   source vs. fetch-a-release (tagged or latest), layout choice
   (combined default, split option) — **and** the PKI/RBAC/kubeconfig/
   `kubernetes`-Service-reconciler/CoreDNS+flannel-manifest half that was
   Group O's original scope.
2. **Drop k3s from `nodebootstrap` entirely.** The crate never installs or
   configures k3s. Its own PKI/RBAC/kubeconfig generation is tested against
   a **real upstream `kube-apiserver` + `kube-controller-manager` +
   `kube-scheduler`** (the exact binaries `upstream-kube-apiserver.sh` and
   its siblings already fetch — see below), not against k3s and not against
   `nodeapiserver`. This is what breaks the chicken-and-egg problem: the
   bootstrap artifacts (CA, certs, kubeconfig, RBAC objects, Service
   reconciler) can be proven correct against a real, spec-compliant
   apiserver **today**, independent of how far `nodeapiserver` itself has
   gotten, and that proof merges to `main` now.
3. **The apiserver binary `nodebootstrap` points at is a config choice**,
   not a hardcoded target. `main` gets `nodebootstrap` defaulting to
   upstream `kube-apiserver`/`kube-controller-manager`/`kube-scheduler`
   (real binaries, real upstream Kubernetes, no k3s) as the interim control
   plane. The `nodeapiserver` integration branch then adds `nodeapiserver`
   itself as a second target and, once its own arc is complete per
   `APISERVER_PLAN.md`'s acceptance criteria, flips the default — same PKI/
   RBAC/kubeconfig code, same tests, different binary underneath. This is
   only possible because point 2 means that code was never coupled to k3s's
   own bundled PKI in the first place (unlike today's
   `upstream-kube-apiserver.sh`, which deliberately *borrows* k3s's PKI —
   see its header comment — precisely because nothing else minted one yet).

Net effect: `not-k8s` drops k3s as its control-plane installer on `main`
*before* `nodeapiserver` is done, running real upstream Kubernetes binaries
instead, wired up by our own bootstrap tooling instead of k3s's. `nodelet`/
`nodeproxy`/`nodescheduler`/`nodecontroller`/`nodestore` already don't care
which apiserver they're talking to as long as it's spec-compliant — this
makes that true of the bootstrap layer too.

## Scope

```
crates/nodebootstrap/
  src/
    main.rs / lib.rs
    config.rs           # env-var config, matching the other components' style
    toolchain.rs         # rustc/cargo/protoc/go presence checks (replaces
                          #   toolchain-{rust,c,go,protoc}.sh)
    containerd.rs         # install/verify containerd + runc + CNI plugins
                           #   (replaces container-runtime.sh)
    cni.rs                # flannel install, skippable for Cilium/other CNI
                           #   (replaces cni.sh)
    fetch.rs              # build-from-source vs. download-a-release (tag or
                           #   latest), combined vs. split layout selection
                           #   (replaces the prebuilt/layout logic in
                           #   bootstrap-source.sh / bootstrap-release.sh).
                           #   A from-source build also stamps the workspace
                           #   Cargo.toml's version from the `version`
                           #   branch's VERSION file before compiling, so a
                           #   source build reports the same version a
                           #   release build would carry instead of the
                           #   placeholder 0.1.0 every crate inherits today
                           #   -- read-only against `version`; bumping it
                           #   stays version-bump.sh's job at release time.
    components.rs          # per-component skip flags, mirrors
                            #   deploy/lib/components.sh's table so the Rust
                            #   and shell sides don't drift — see "Migration"
    pki.rs                # CA, serving cert, SA signing keypair, static
                           #   control-plane client certs (Group O's PKI
                           #   half). Per-node certs are NOT here -- that's
                           #   nodecontroller's existing CSR-signing flow,
                           #   fed by the CA this module mints.
    kubeconfig.rs          # kubeconfig emission for kubectl and every
                            #   in-cluster component
    rbac.rs                 # **finding, 2026-08-22**: does NOT need to
                             #   vendor/hand-build the ~90 system: Cluster-
                             #   Roles/Bindings -- kube-apiserver's own
                             #   `rbac/bootstrap-roles` PostStartHook
                             #   creates and reconciles them whenever
                             #   --authorization-mode includes RBAC, k3s or
                             #   not (confirmed: setup-control-plane.sh has
                             #   zero manual RBAC-object calls today). This
                             #   module is a thin smoke-check instead.
    service_reconciler.rs   # **finding, 2026-08-22, same shape as rbac.rs's**:
                             #   real kube-apiserver reconciles the
                             #   `kubernetes` Service/Endpoints itself,
                             #   unconditionally, via its own
                             #   bootstrap-controller PostStartHook. This
                             #   module is a thin verify, not a reconciler.
    manifests.rs             # CoreDNS only, applied via the generated
                              #   kubeconfig once the apiserver is up.
                              #   Flannel is NOT a manifest in this project
                              #   (it's cni.rs's host-level flanneld daemon,
                              #   same as today's cni.sh) -- corrected as of
                              #   this finding, the tree above previously
                              #   said otherwise.
    targets/
      upstream.rs             # installs/runs kube-apiserver + kube-
                               #   controller-manager + kube-scheduler
                               #   against nodestore (main's default)
      nodeapiserver.rs         # added on the nodeapiserver branch once that
                                #   binary exists; becomes the default once
                                #   its own acceptance criteria are met
```

`targets/` is the seam that makes point 3 above real: everything above it
(PKI, RBAC, kubeconfig, Service reconciler, manifests) is target-agnostic;
only `targets/*.rs` knows how to install and start a specific apiserver/
controller-manager/scheduler combination.

## `nodebootstrap` as a `notk8s` applet, and self-replacement (noted 2026-08-22)

**`nodebootstrap` is very likely a candidate to ship as an applet inside the
combined `notk8s` binary itself** (`CLAUDE.md`'s "Two build layouts" --
`crates/notk8s` dispatches on `argv[0]`/a `notk8s <component>` subcommand,
one row in `deploy/lib/components.sh` + `crates/notk8s/Cargo.toml`'s
optional-dependency table + its `APPLETS` table), not just a standalone
installer invoked once and thrown away. Consequence worth designing for,
not yet acted on: **a deployed `notk8s` binary needs to be able to build or
fetch a *replacement for itself*, using the exact same `fetch.rs` logic
that installed it in the first place** -- not a separate "self-update"
code path. `fetch.rs`'s `Source::Compile`/`Source::Release` already do
"produce a binary" generically per `components.rs`'s table; the only new
piece is atomically swapping the *running* binary's own file (or its
`bin/<component>` symlinks) for the newly built/fetched one -- safe on
Linux because replacing a file by path doesn't affect a process already
running off the old inode (rename-into-place, not overwrite-in-place, is
the mechanism every self-updating Unix tool uses). Also relevant to the
`ln -s e2e not-k8s && e2e` / `notk8s --e2e` idea raised in the same
conversation: if `nodebootstrap` becomes an applet, the same argv[0]/flag
dispatch `notk8s` already has for `nodelet`/`nodeproxy`/etc. is the natural
place for a `notk8s --nodebootstrap` (or `bootstrap`) entry point too, and
potentially an `e2e` applet wrapping the `deploy/lib/test/*.sh` harness
this plan's "shell scripts stay until fully replaced" position already
carves out as the one thing not migrating to Rust. Not designed further
yet -- flagging the shape of the problem for whoever picks up the
`notk8s`-applet-integration PR, which is downstream of Phase 1 actually
landing on `main`.

## Testing strategy

Same shape as `deploy/lib/test/cases/datastore.sh` (drives the real gRPC API
against a throwaway `nodestore`) and `APISERVER_PLAN.md`'s "getting signal
earlier" rig: a case file in `deploy/lib/test/cases/*.sh` that runs
`nodebootstrap` end to end — generate PKI, mint kubeconfig, install RBAC,
stand up `nodestore` + upstream `kube-apiserver`/`kube-controller-manager`/
`kube-scheduler` on scratch ports/data dirs — then drives it with real
`kubectl`: can a ServiceAccount token authenticate, does RBAC actually gate
what it says it gates, does the `kubernetes` Service resolve, is discovery
correct. This is real signal against a real apiserver, not a mock, and it
runs on `main`'s own e2e gate — no `nodeapiserver` dependency.

## Phasing / merge protocol

Standard `CLAUDE.md` merge protocol applies, group by group, same as
`nodeapiserver`'s own delivery groups:

1. **Phase 1 (mergeable to `main` independently of `nodeapiserver`):**
   `toolchain.rs`, `containerd.rs`, `cni.rs`, `fetch.rs`, `components.rs`,
   `targets/upstream.rs`, `pki.rs`, `kubeconfig.rs`, `rbac.rs`,
   `service_reconciler.rs`, `manifests.rs`, `service_mgr.rs`, `services.rs`
   (this project's own `nodestore`/`nodelet`/`nodeproxy` as real services).
   Each is its own branch/PR, own
   e2e case, own gate — no long-lived integration branch needed for this
   phase since nothing here depends on unfinished `nodeapiserver` code.
   CI/CD (`build.yml`/`e2e.yml`/`release.yml`) cuts over to invoking
   `nodebootstrap` instead of the shell entry points once this phase is
   e2e-green — one bootstrap path, not two maintained in parallel.
2. **Phase 2 (on the `nodeapiserver` integration branch):**
   `targets/nodeapiserver.rs`, plus whatever `nodeapiserver`'s own groups
   need from `nodebootstrap` that upstream's binaries didn't exercise
   (anything `nodeapiserver` does differently from real `kube-apiserver`).
   Flipping the default target away from `targets/upstream.rs` happens only
   once `APISERVER_PLAN.md`'s final acceptance criteria are met, per the
   existing "no partial multi-phase work" standing rule.

`deploy/bootstrap-source.sh`/`bootstrap-release.sh` and the shell libs they
call are deleted once Phase 1 lands and CI/CD has cut over — not kept as a
parallel fallback path.

## Implementation status (updated as it lands)

All of Phase 1's modules have real logic now, each with an explicit,
documented scope cut rather than a silent gap -- see each module's own doc
comment for the specifics and what's queued next:

| Module | Primary path | Deepest fallback tier(s) |
|---|---|---|
| `pki.rs` | ✅ real (CA + static client certs, `rcgen`) | n/a -- no fallback tier, this is new code |
| `kubeconfig.rs` | ✅ real | n/a |
| `rbac.rs` | ✅ real -- thin verify, **plus two real supplemental RBAC grants** found live: (1) `system:kube-scheduler`'s built-in bootstrap role doesn't cover DRA (`resource.k8s.io` `deviceclasses`/`resourceclaims`/`resourceslices`), which `nodescheduler` watches unconditionally; (2) `nodecontroller` runs every controller as the single `system:kube-controller-manager` identity instead of real upstream's per-controller service-account impersonation, so that identity needs far more than its built-in role grants -- bridged with a **known-too-broad, explicitly documented** `cluster-admin` binding pending `nodecontroller` growing real per-controller impersonation. See both findings in `rbac.rs`'s doc comment | n/a |
| `manifests.rs` | ✅ real (CoreDNS only -- flannel corrected out of scope, see its doc comment) | n/a |
| `toolchain.rs` | ✅ real, every tier (rust: package manager -> rustup; protoc: package manager -> official prebuilt -> from-source autotools build; C toolchain: package manager -> musl.cc static prebuilt -> from-source gcc+binutils build; Go: package manager -> official prebuilt -> from-source 3-stage bootstrap) | n/a |
| `containerd.rs` | ✅ real, every tier (package manager -> official prebuilt -> from-source, needs `toolchain::ensure_go`; config.toml + this project's 3 required patches; starts via its own distro unit or `service_mgr.rs`) | n/a |
| `cni.rs` | ✅ real, every tier (plugin binaries + flannel binary + flannel CNI plugin: package manager -> official prebuilt -> from-source, all needing `toolchain::ensure_go`; starts `flanneld` via `service_mgr.rs` + a vendored `run-flanneld.sh` wrapper -- see `vendor/README.md`) | n/a |
| `fetch.rs` | ✅ real for both `Source::Compile` (version-stamp + `cargo build` per layout) and `Source::Release` (GitHub Releases API resolution + asset download, confirmed against this repo's own real published release naming) | n/a |
| `targets/upstream.rs` | ✅ real -- **installs only `kube-apiserver`** now (2026-08-22, user direction: `nodescheduler`/`nodecontroller` are the scheduler/controller-manager, not upstream's binaries -- see `services.rs`), binary fetch + full flag-set construction + `service_mgr.rs` + a best-effort `/readyz` wait. **`K8S_VERSION` bumped `v1.33.13` -> `v1.34.11`**: `nodescheduler` hardcodes `resource.k8s.io/v1`, GA only from 1.34. `--runtime-config=api/all=true` was tried as a hedge and **removed** after it broke `rbac/bootstrap-roles` (`rbac.rs`'s whole finding depends on that PostStartHook succeeding) -- turning on every alpha API unconditionally is not a safe default; a GA API needs no flag at all once the version is right | n/a |
| `components.rs` | ✅ real (static table, mirrors `components.sh`) | n/a |
| `service_reconciler.rs` | ✅ real (thin verify -- second "kube-apiserver already does this" finding, same shape as `rbac.rs`) | n/a |
| `service_mgr.rs` | ✅ real, all three tiers (systemd -> OpenRC -> self-restart loop + cron `@reboot`), unit-tested. Wired into `containerd.rs`, `targets/upstream.rs`, `cni.rs`, and `services.rs`. | n/a |
| `services.rs` | ✅ real for all five, all wired into `run_all()`: `nodestore`, `nodescheduler`/`nodecontroller` (the default scheduler/controller-manager -- see `targets/upstream.rs`, using the `kube-scheduler.kubeconfig`/`kube-controller-manager.kubeconfig` `pki.rs` mints for exactly those identities), `nodelet`/`nodeproxy` (using `admin.kubeconfig`, matching current `bootstrap-source.sh` behavior) | ❌ not ported: a real per-node identity for `nodelet` (`system:node:<name>` via `nodecontroller`'s CSR-signing flow) -- using `admin.kubeconfig` isn't a regression from current behavior, but isn't tightened either |

**HTTP fetch is a real Rust client, not `curl`/`wget` subprocesses**
(decided 2026-08-22, user direction): `pkg::fetch_url` (every binary/
release download in `toolchain.rs`/`containerd.rs`/`cni.rs`/`fetch.rs`/
`targets/upstream.rs`) and `targets/upstream.rs`'s readyz wait both use
`ureq` (sync, rustls-backed, no OpenSSL/native-tls) instead of shelling out.
`kubectl` subprocess calls (`rbac.rs`/`manifests.rs`/`service_reconciler.rs`)
are unaffected -- `kubectl` *is* the client there, not a stand-in for an
HTTP GET this crate could trivially do itself.

**The service-supervision writer (`service_mgr.rs`) is done and wired into
`containerd.rs`, `targets/upstream.rs`, and `cni.rs`.** All three of
Phase 1's "start it as a service" gaps are closed. `cni.rs` gets there via
a vendored `run-flanneld.sh` wrapper rather than a Rust reimplementation of
its net-conf.json + ECMP-aware interface-detection logic — see
`vendor/README.md`'s entry for why that stays a shell script `service_mgr.rs`
points at, not Rust code this crate would have to keep running itself.
