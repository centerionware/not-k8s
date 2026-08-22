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
    pki/                  # CA, serving cert, SA signing keypair, per-
                           #   component client certs (Group O's PKI half)
    kubeconfig.rs          # kubeconfig emission for kubectl and every
                            #   in-cluster component
    rbac/                  # the ~90 system: ClusterRoles/ClusterRoleBindings
                            #   from upstream bootstrappolicy
    service_reconciler.rs   # the `kubernetes` default Service + endpoint
                             #   reconciler
    manifests/               # CoreDNS + flannel manifests, applied via the
                              #   generated kubeconfig once the apiserver is up
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
   `targets/upstream.rs`, `pki/`, `kubeconfig.rs`, `rbac/`,
   `service_reconciler.rs`, `manifests/`. Each is its own branch/PR, own
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
