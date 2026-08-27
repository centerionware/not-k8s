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

1. **One crate, `nodebootstrap`**, not two. It replaces the former
   `bootstrap-source.sh`/`bootstrap-release.sh` and the non-performance
   `deploy/lib/*.sh` tree —
   toolchain checks, containerd install, CNI/flannel install (skippable, so
   Cilium etc. can be used instead), per-component skip flags, build-from-
   source vs. fetch-a-release (tagged or latest), layout choice
   (combined default, split option) — **and** the PKI/RBAC/kubeconfig/
   `kubernetes`-Service-reconciler/CoreDNS+flannel-manifest half that was
   Group O's original scope.
2. **Drop k3s from `nodebootstrap` entirely.** The crate never installs or
   configures k3s. Its own PKI/RBAC/kubeconfig generation is tested against
   a **real upstream `kube-apiserver`** plus this projects own
   `nodescheduler` and `nodecontroller` services, not against k3s and not
   against `nodeapiserver`. This is what breaks the chicken-and-egg problem: the
   bootstrap artifacts (CA, certs, kubeconfig, RBAC objects, Service
   reconciler) can be proven correct against a real, spec-compliant
   apiserver **today**, independent of how far `nodeapiserver` itself has
   gotten, and that proof merges to `main` now.
3. **The apiserver binary `nodebootstrap` points at is a config choice**,
   not a hardcoded target. `main` gets `nodebootstrap` defaulting to the upstream
   `kube-apiserver` binary, while this project supplies the scheduler and
   controller-manager services as the interim control
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
                           #   stays the release workflow's job.
    components.rs          # per-component skip flags, mirrors
                            #   the archived deploy component table
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
    manifests.rs             # CoreDNS Deployment + Service, applied via
                              #   the generated kubeconfig once the
                              #   apiserver is up; can be disabled explicitly.
                              #   Flannel is NOT a manifest in this project
                              #   (it's cni.rs's host-level flanneld daemon,
                              #   same as today's cni.sh) -- corrected as of
                              #   this finding, the tree above previously
                              #   said otherwise.
    targets/
      upstream.rs             # installs/runs the upstream
                               #   kube-apiserver only; this project
                               #   supplies scheduler/controller services
                               #   against nodestore (main's default)
      nodeapiserver.rs         # added on the nodeapiserver branch once that
                                #   binary exists; becomes the default once
                                #   its own acceptance criteria are met
```

`targets/` is the seam that makes point 3 above real: everything above it
(PKI, RBAC, kubeconfig, Service reconciler, manifests) is target-agnostic;
only `targets/*.rs` knows how to install and start a specific apiserver
target.  Scheduler and controller services remain this projects own units.

## `nodebootstrap` as a `notk8s` applet and self-replacement

`nodebootstrap` is now shipped in both forms: the release pipeline publishes a
standalone installer binary, and the combined `notk8s` binary includes it as
the `nodebootstrap` applet plus the `bootstrap` alias.  The normal install is:

```bash
wget https://github.com/centerionware/not-k8s/releases/download/v0.7.0/notk8s-0.7.0-linux-aarch64-release
chmod +x notk8s-0.7.0-linux-aarch64-release
ln -s ./notk8s-0.7.0-linux-aarch64-release bootstrap
./bootstrap
```

The default command runs the complete bootstrap flow.  CRI is enabled by
default; `--without-cri` is the only CRI-selection flag, skipping containerd and CNI and selecting nodelet mock
runtime.  `--from-source` uses the same fetched toolchain fallbacks as the
former source script and can rebuild the installed binaries.  `--update` (or
`--release`) fetches release assets, and every installed service is restarted
when its unit is refreshed.  The combined applet stages its own executable,
so an already-installed release can rebuild or update itself.

Common post-install commands are:

```bash
./bootstrap --e2e
./bootstrap --e2e --only=node
./bootstrap --e2e --shard=1/5       # CI: one of five deterministic shards
KUBECONFIG=/path/to/cluster.kubeconfig ./bootstrap --e2e
```

`--e2e` is a read-only bootstrap applet mode. It does not re-run installation,
does not require k3s-specific paths or flags, and uses the Kubernetes Rust
client directly. It prefers `$KUBECONFIG` and otherwise discovers the admin
kubeconfig emitted under `/etc/nodebootstrap`. The former shell cases are
archived on `archive-shell-scripts-0.7.1`; their Rust replacements are added
to this registry under issue #242.

## Testing strategy

The bootstrap applet's `--e2e` mode is the long-term runner. It drives the
Kubernetes API directly through the Rust client so the same checks work on
any cluster bootstrapped by the applet, without k3s-only flags or shell
wrappers. The initial checks cover apiserver resource serving, node readiness,
and the apiserver's reachable `default/kubernetes` endpoint. The old shell
suite is no longer part of the build or release gate; each missing feature
assertion must be ported into this registry before it is considered restored.
GitHub Actions runs five independent bootstrap clusters: CSI/DRA cases are
balanced across shards 1-2, and all other cases are balanced across shards
1-5. `--only` is applied after the stable shard assignment, so a filtered CI
run tests the same shard as an unfiltered run.

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

The non-performance shell installer, deployment, diagnostic, and e2e files
were deleted after the CI/CD cutover. The exact pre-cutover tree is preserved
on `archive-shell-scripts-0.7.1` for reference. Performance helpers remain
until their planned 0.7.4 migration.

## Implementation status (updated as it lands)

All of Phase 1's modules have real logic now, each with an explicit,
documented scope cut rather than a silent gap -- see each module's own doc
comment for the specifics and what's queued next:

| Module | Primary path | Deepest fallback tier(s) |
|---|---|---|
| `pki.rs` | ✅ real (CA + static client certs, `rcgen`) | n/a -- no fallback tier, this is new code |
| `kubeconfig.rs` | ✅ real | n/a |
| `rbac.rs` | ✅ real -- thin verify, **plus two real supplemental RBAC grants** found live: (1) `system:kube-scheduler`'s built-in bootstrap role doesn't cover DRA (`resource.k8s.io` `deviceclasses`/`resourceclaims`/`resourceslices`), which `nodescheduler` watches unconditionally; (2) `nodecontroller` ran every controller as the single `system:kube-controller-manager` identity instead of real upstream's per-controller service-account impersonation -- **fixed in `nodecontroller` itself** (not bridged here): it now impersonates the real upstream `system:serviceaccount:kube-system:<controller-name>` per controller, so this module grants only the narrow `impersonate` verb needed for that, not `cluster-admin`. See `docs/E2E_FINDINGS.md` finding 22 and both findings in `rbac.rs`'s doc comment | n/a |
| `manifests.rs` | ✅ real (healthy CoreDNS Deployment + Service, configurable domain and IPv4/IPv6 service IPs, and explicit `--disable-dns`; flannel corrected out of scope, see its doc comment) | n/a |
| `toolchain.rs` | ✅ real, every tier (rust: package manager -> rustup; protoc: package manager -> official prebuilt -> from-source autotools build; C toolchain: package manager -> musl.cc static prebuilt -> from-source gcc+binutils build; Go: package manager -> official prebuilt -> from-source 3-stage bootstrap) | n/a |
| `containerd.rs` | ✅ real, every tier (package manager -> official prebuilt -> from-source, needs `toolchain::ensure_go`; config.toml + this project's 3 required patches; starts via its own distro unit or `service_mgr.rs`) | n/a |
| `cni.rs` | ✅ real, every tier (plugin binaries + flannel binary + flannel CNI plugin: package manager -> official prebuilt -> from-source, all needing `toolchain::ensure_go`; starts `flanneld` via `service_mgr.rs` and the Rust `flanneld` service applet) | n/a |
| `fetch.rs` | ✅ real for source/release/prebuilt selection, including the<br>standalone and combined CLI paths (version-stamp + `cargo build` per layout), `Source::Release` (GitHub Releases API resolution + asset download, confirmed against this repo's own real published release naming), and the prebuilt seam (`NOTK8S_COMBINED_PREBUILT` / per-component `NOTK8S_*_PREBUILT`, checked before `cfg.source` -- same precedence `nodelet-build.sh`'s `build_nodelet()` documents; added 2026-08-22 so `release.yml`'s e2e stage can stage one compiled artifact into 5 shards instead of recompiling per shard) | n/a |
| `targets/upstream.rs` | ✅ real -- **installs only `kube-apiserver`** now (2026-08-22, user direction: `nodescheduler`/`nodecontroller` are the scheduler/controller-manager, not upstream's binaries -- see `services.rs`), binary fetch + full flag-set construction + `service_mgr.rs` + a best-effort `/readyz` wait. **`K8S_VERSION` bumped `v1.33.13` -> `v1.34.11`**: `nodescheduler` hardcodes `resource.k8s.io/v1`, GA only from 1.34. `--runtime-config=api/all=true` was tried as a hedge and **removed** after it broke `rbac/bootstrap-roles` (`rbac.rs`'s whole finding depends on that PostStartHook succeeding). **`wait_for_nodestore`**: a real startup race found live -- `service_mgr.rs`'s `After=nodestore.service` only guarantees the unit started, not that its gRPC/TLS listener is accepting connections yet, and `rbac/bootstrap-roles` doesn't retry its own initial etcd connection failure. A hard TCP-connect wait before installing kube-apiserver closes it | n/a |
| `components.rs` | ✅ real (static table, mirrors `components.sh`) | n/a |
| `service_reconciler.rs` | ✅ real (thin verify -- second "kube-apiserver already does this" finding, same shape as `rbac.rs`) | n/a |
| `service_mgr.rs` | ✅ real, all three tiers (systemd -> OpenRC -> self-restart loop + cron `@reboot`), unit-tested. Wired into `containerd.rs`, `targets/upstream.rs`, `cni.rs`, and `services.rs`. | n/a |
| `services.rs` | ✅ real for all five, all wired into `run_all()`: `nodestore`, `nodescheduler`/`nodecontroller` (the default scheduler/controller-manager -- see `targets/upstream.rs`, using the `kube-scheduler.kubeconfig`/`kube-controller-manager.kubeconfig` `pki.rs` mints for exactly those identities), `nodelet`/`nodeproxy` (using `admin.kubeconfig`, matching current `bootstrap-source.sh` behavior) | ❌ not ported: a real per-node identity for `nodelet` (`system:node:<name>` via `nodecontroller`'s CSR-signing flow) -- using `admin.kubeconfig` isn't a regression from current behavior, but isn't tightened either |

Bootstrap installation choices are recorded one flag per line in
`/etc/nodebootstrap/flags` (or `NODEBOOTSTRAP_FLAGS_FILE`) and replayed on
later invocations, including upgrades. One-shot inspection/e2e controls and
control-plane removal and `--uninstall` are deliberately not persisted.
`--disable-dns` skips CoreDNS and its nodelet DNS configuration;
`--cluster-domain=NAME` wires the same domain into CoreDNS, nodelet Pod
DNS/search configuration, the apiserver service-account issuer, and the
apiserver serving certificate. `--cidr=CIDR` selects the IPv4 service range;
`--cidr6=CIDR` adds an optional IPv6 service range and updates the apiserver,
CoreDNS, and apiserver serving certificate consistently. `--uninstall`
removes nodebootstrap-managed host services, files, state, and tracked
packages.

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
Phase 1's "start it as a service" gaps are closed. `cni.rs`'s Rust
`flanneld` service applet rewrites net-conf.json, waits for the node PodCIDR,
and resolves the default interface on every supervised start.
