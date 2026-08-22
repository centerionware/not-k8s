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
    service_reconciler.rs   # the `kubernetes` default Service + endpoint
                             #   reconciler
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
   `service_reconciler.rs`, `manifests.rs`. Each is its own branch/PR, own
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
| `rbac.rs` | ✅ real (thin verify -- see the finding in its doc comment) | n/a |
| `manifests.rs` | ✅ real (CoreDNS only -- flannel corrected out of scope, see its doc comment) | n/a |
| `toolchain.rs` | ✅ real (rust, protoc: package manager -> official prebuilt) | ❌ not ported: gcc/go/protoc-from-source, musl.cc |
| `containerd.rs` | ✅ real (package manager -> official prebuilt; config.toml + this project's 3 required patches; systemd start/restart) | ❌ not ported: from-source containerd/runc build; OpenRC/non-systemd service writer |
| `cni.rs` | ✅ real (plugin binaries + flannel binary + CNI conf: package manager -> official prebuilt) | ❌ not ported: from-source builds; starting `flanneld` itself (needs a live kubeconfig + the service writer) |
| `fetch.rs` | ✅ real for `Source::Compile` (version-stamp + `cargo build` per layout) | ❌ not ported: `Source::Release` (GitHub Releases asset matching) |
| `targets/upstream.rs` | ✅ real (binary fetch + full flag-set construction, unit-tested) | ❌ not ported: starting the three binaries as supervised services (service writer, same gap as `containerd.rs`/`cni.rs`) |
| `components.rs` | ✅ real (static table, mirrors `components.sh`) | n/a |
| `service_reconciler.rs` | ❌ still a scaffold stub | -- |

**The recurring gap across five modules is one thing, not five:** a
service-supervision writer (systemd unit + OpenRC equivalent, matching
`deploy/lib/service-mgr.sh`'s `install_supervised_service`). Porting that
once unblocks actually starting containerd, flanneld, and the three
upstream control-plane binaries — the next PR to prioritize.
