# kubelet parity gap closure — working memory

Started 2026-07-30, rescoped 2026-07-30 (same day, scope expanded from "close
3 known gaps" to "100% kubelet parity, performance-focused"). Checkpoint
commit before this rescoping: `fdf003b`.

## Verified scope boundary (checked against kubernetes.io docs, not assumed)

Sources: [Kubernetes Components](https://kubernetes.io/docs/concepts/overview/components/),
[kubelet reference](https://kubernetes.io/docs/reference/command-line-tools-reference/kubelet/),
[Node-pressure eviction](https://kubernetes.io/docs/concepts/scheduling-eviction/node-pressure-eviction/),
[Graceful Node Shutdown](https://kubernetes.io/blog/2021/04/21/graceful-node-shutdown-beta/),
[Static Pods](https://kubernetes.io/docs/tasks/configure-pod-container/static-pod/).

**Confirmed genuinely NOT kubelet's job** (someone else's component, not a
nodelet gap):
- Pod scheduling/binding decisions → **kube-scheduler**.
- etcd storage, Raft quorum, peer election → **etcd**.
- ReplicaSet/Deployment/StatefulSet/Job/CronJob/HPA control loops → **kube-controller-manager**.
- Node lifecycle taints after missed heartbeats, cloud taint lifecycle → **kube-controller-manager** (nodelet already correctly does the one thing that *is* its job here: clearing the `node.cloudprovider.kubernetes.io/uninitialized` taint on itself — see `node.rs::clear_cloudprovider_taint`).
- Cloud load balancer / route provisioning → **cloud-controller-manager**.
- Admission control, webhooks, server-side apply field management → **kube-apiserver**.
- CSR approval / cluster CA signing → **kube-controller-manager** + **kube-apiserver**.
- Dynamic PV provisioning → external CSI provisioner sidecar (not kubelet); kubelet's actual job here is only the node-local `NodeStageVolume`/`NodePublishVolume` mount, which *is* in scope below.
- Service/Endpoints → **kube-proxy**'s job in stock Kubernetes; nodelet already reimplements this itself (`svc.rs`, nftables) as a deliberate architectural choice predating this doc — stays as-is, not touched by this pass.
- Windows node support — out of scope for a different reason: this project is Linux-only edge hardware by design (cgroup v2, `/proc`, Linux CRI sockets), not "someone else's job."

Everything else below **is** kubelet's job and is now in scope.

## Round 73: crash-loop backoff (2026-07-31, same day)

Closes the biggest finding from round 72's fresh gap re-audit — user
picked it directly over the PodResources API and
`allocatedResourcesStatus`, the only other candidates offered.

- Root cause, worth remembering: this codebase is entirely event-driven
  (`pods.rs`'s own doc comment: "no periodic relist and no per-second
  polling"). A container's `ContainerExited` CRI event only triggers
  `on_runtime_event()`, which re-reads status and writes it back — but
  that write is itself a Pod object modification, which the same
  controller's own `watcher()` stream observes as another `Apply` event,
  triggering another `reconcile()` → `ensure_pod()` → restart. This
  feedback loop has no natural rate limit at all: a container that exits
  in well under a second gets recreated as fast as the runtime can
  physically create+start it.
- New pure functions in `container_state.rs`: `crash_loop_backoff_secs()`
  (given the delay required before the restart attempt that just
  happened and how long ago that was, returns the delay required before
  the *next* one — doubles on a still-failing container up to a 5-minute
  cap, resets to a 10-second base after a 10-minute gap with no restart
  attempt, matching real kubelet's own `flowcontrol.Backoff` constants
  exactly) and `crash_loop_backoff_ready()` (whether enough time has
  elapsed since the last attempt).
- New `CriRuntime.restart_backoff: Mutex<HashMap<String, (u64, u64)>>`
  side table (`"sandbox_id/container_name" -> (last restart attempt unix
  seconds, backoff delay that was required before it)`), same
  lifecycle/pattern as the pre-existing `restart_counts` table — cleared
  alongside it on sandbox removal/recreation (`clear_restart_backoff()`).
- `ensure_container()`'s `NeedsRestart` path now checks
  `restart_backoff_ready()` before actually removing the stale container
  and recreating it — if not ready yet, it simply returns `Ok(())`,
  leaving the exited container in place until the next trigger (another
  status-write echo, or a genuine watch event) re-evaluates it, matching
  this controller's existing "react to the next edge, don't poll"
  posture rather than adding a new timer loop. **Scoped precisely**: only
  a genuine exit-triggered restart is subject to backoff — a
  resize-triggered restart (`ResizeDecision::RequiresRestart`, an
  operator-driven event on an otherwise-healthy running container) is a
  completely different kind of restart and must never be throttled by
  this, tracked via a separate `is_crash_restart` flag rather than
  reusing the existing `needs_restart` bool for both.
- **Documented, not silently cut**: the `CrashLoopBackOff` status string
  itself isn't reported — `ContainerStatus.lastState` isn't tracked at
  all in this codebase yet (a separate, larger pre-existing gap this
  round didn't take on), so a backing-off container's status still shows
  its real `Terminated` state (accurate exit code/reason) during the
  backoff window rather than kubectl's familiar `Waiting:
  CrashLoopBackOff` + `lastState: Terminated` pairing. The actual
  behavior fix (the restart *rate* itself) is real regardless.
- 10 new unit tests (`cri_tests/crash_loop_backoff.rs`): first-restart-
  is-immediate, backoff doubling, cap enforcement, reset-after-a-long-gap,
  no-reset-just-under-the-threshold, ready/not-ready boundary checks,
  and a saturating-subtraction no-panic check for a clock-looks-backwards
  edge case.
- New e2e test `test_crash_loop_backoff_throttles_immediate_restarts`
  (`lifecycle.sh`): a container that exits immediately (`exit 1`, no
  sleep at all) is given a 20-second window, then asserts its restart
  count is low (between 1 and 3) — without backoff, an immediate-exit
  tight loop could rack up dozens of restarts in that window; with the
  10-second base delay in effect, it can't.

## Round 72: fresh gap re-audit (2026-07-31, same day)

No code changed. Grepped the codebase directly for a handful of specific
kubelet features not yet checked in any prior round's audit, cross-checked
against kubernetes.io docs. Found 3 previously-untracked items:

1. **Crash-loop backoff** — the biggest finding. `ensure_container()`'s
   restart path (`restart_decision()` in `container_create.rs`) has no
   delay or attempt-count-based backoff of any kind: a container that
   keeps exiting gets recreated immediately on the very next reconcile.
   Because nodelet's architecture is event-driven (a `ContainerExited`
   CRI event triggers an immediate reconcile, not a polling interval),
   this isn't merely "no `CrashLoopBackOff` status string" — it's a
   genuine tight restart loop with no throttle, unlike real kubelet's
   10s/20s/40s/.../5m-capped exponential backoff (resetting after ~10
   minutes of stable running). Reclassified the existing "Restart-on-exit"
   line from ✅ to 🟡 rather than adding a separate bullet, since it's a
   real gap in an already-implemented feature, not a wholly missing one.
2. **PodResources API** — kubelet's own gRPC service (`List`/
   `GetAllocatableResources`/`Watch` on `/var/lib/kubelet/pod-resources/kubelet.sock`)
   isn't implemented. Real-world device-monitoring tooling (NVIDIA DCGM,
   Prometheus device exporters) depends on this socket directly. All the
   underlying state already exists in `cpu_manager.rs`/`memory_manager.rs`/
   `device_plugins.rs` — this would be a read-only gRPC projection of
   already-tracked allocation state, not new allocation logic.
3. **`allocatedResourcesStatus`** (ResourceHealthStatus, KEP-4680) — a
   device going unhealthy after allocation to a running container has no
   pod-visible signal; `device_plugins.rs` already tracks per-device
   health for capacity purposes but never reports it back per-container.
   Lower priority — still alpha/beta upstream, not GA.

Everything else checked directly against the API/CRI proto (pod-level
`spec.resources`, seccomp/AppArmor, `spec.os`, probe-level grace period,
image volume source, `Node.status.volumesInUse`/`.volumesAttached`) was
already tracked from earlier rounds or already closed — no other new
gaps found.

Ranked by value: crash-loop backoff (real resource-protection gap, not
just cosmetic), PodResources API (real external-tooling integration
value, low implementation risk since the state already exists), then
`allocatedResourcesStatus` (alpha/beta, lower urgency). Ask the user for
sequencing again before starting the next round.

## Round 71: image credential providers (2026-07-31, same day)

Closes the round 69 audit's other flagged gap: kubelet's
`--image-credential-provider-config`/`--image-credential-provider-bin-dir`
exec-plugin protocol for obtaining registry credentials dynamically (cloud
workload-identity federation for ECR/GCR/ACR, among others), including the
newer ServiceAccount token integration (`tokenAttributes`, beta/default-on
in k8s 1.34) that lets a provider request an audience-scoped, pod-bound
token instead of relying only on static `imagePullSecrets`.

- New module `credential_provider.rs`: parses a `CredentialProviderConfig`
  YAML (`NODELET_IMAGE_CREDENTIAL_PROVIDER_CONFIG`) listing providers
  (binary name resolved under `NODELET_IMAGE_CREDENTIAL_PROVIDER_BIN_DIR`,
  `matchImages` glob patterns, `defaultCacheDuration`, `args`, `env`,
  optional `tokenAttributes`). No gRPC involved here, unlike every other
  plugin protocol elsewhere in this codebase (CSI/device-plugin/DRA all
  register over a Unix socket) — matches kubelet's own design: a plain
  subprocess exec per pull, `CredentialProviderRequest` JSON on stdin,
  `CredentialProviderResponse` JSON on stdout.
- `image_matches_pattern()`: real kubelet's own documented glob rules —
  domain segments (split by `.`) must have equal count between pattern and
  image, `*` matches exactly one segment (never spans dots — `*.io` does
  NOT match `foo.k8s.io`), a pattern's port must match exactly if
  specified, and the pattern's path must be a prefix of the image's path.
  11 unit tests cover this, including the ECR-style multi-glob case and
  the "glob never spans segments" case explicitly.
- `CredentialProviders::resolve()`: 3-tier response caching (`Image`/
  `Registry`/`Global`, per the response's own `cacheKeyType`), with a
  30-second exec timeout and cache duration parsed from the response's
  `cacheDuration` (falling back to the provider's own
  `defaultCacheDuration`, then a 5-minute default) via a narrow
  `parse_go_duration()` (`"12h"`/`"15m"`/`"90s"` only — not a general
  Go-duration parser, documented as such).
- ServiceAccount token minting: `resolve_credential_provider_auth()` (in
  `container_support.rs`) fetches the live `ServiceAccount` object,
  checks `requireServiceAccount`/`requiredServiceAccountAnnotationKeys`
  are satisfied, then reuses the pre-existing `resolve_service_account_token()`
  (the same `TokenRequest` machinery projected `serviceAccountToken`
  volumes already use) to mint an audience-scoped token — only for
  providers that actually declare `tokenAttributes`; a provider that
  doesn't ask for one never sees one.
- `PodId` (the denormalized per-pod identity struct threaded through the
  CRI runtime's deep call chains) gained a `service_account_name` field so
  the credential-provider path has it without threading a full `&Pod`
  through every layer — the same established pattern as every other
  `PodId` field.
- **Two deliberate, documented scope-narrowing decisions**: (1) only the
  *first* matching provider (config order) is tried per image, not a full
  multi-provider merge like real kubelet — non-overlapping `matchImages`
  per cloud registry is the overwhelmingly common real-world shape; (2)
  `resolve_pull_auth()` tries `imagePullSecrets` first, only falling back
  to credential providers if no secret resolves — explicit pod-declared
  intent wins over automatic discovery, since getting that backwards
  would be the more surprising direction to be wrong in.
- 21 unit tests total (`credential_provider_tests/`): glob matching (11),
  `parse_go_duration()` edge cases (6, including the documented
  compound-form limitation), and `CredentialProviders::load()` (4: feature
  off via empty path, nonexistent path is an error, malformed YAML is an
  error not a panic, and a valid two-provider config correctly resolves
  `first_match()` for each).
- New e2e test file `credential_provider.sh`: automates the
  "feature stays off by default" regression check (the one this round
  most needed to guard — a config-loading bug silently switching every
  image pull onto a provider path would break every plain
  `imagePullSecrets`-only deployment); a real exec-plugin fetch (needing
  an actual cloud vendor binary and live registry) joins the existing
  `dra.sh`/`device_plugins.sh`-style manual-procedure group, for the same
  reason those can't be automated in this bash-only harness.

## Round 70: image GC watermark policy (2026-07-31, same day)

Closes a long-tracked partial gap: image GC previously swept *every*
unreferenced image on *every* GC cycle regardless of actual disk usage —
not real kubelet's own policy, which is purely reactive to disk pressure.

- 3 new config knobs, matching upstream's own flags: `NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT`
  (default 85, `--image-gc-high-threshold`), `NODELET_IMAGE_GC_LOW_THRESHOLD_PERCENT`
  (default 80, `--image-gc-low-threshold`), `NODELET_IMAGE_GC_MIN_AGE_SECS`
  (default 120/2m, `--image-minimum-gc-age`).
- New pure `disk_usage_percent()` (reuses the same `statvfs(2)`-backed
  measurement `DiskPressure` already makes against `disk_path`) and
  `should_start_image_gc()` in `gc.rs`: an unreferenced image is left
  entirely alone — no matter how long it's sat there — until usage
  crosses the high threshold.
- New pure `images_to_reclaim_space()`: once triggered, only images
  unreferenced for at least the minimum age are eligible, removed
  oldest-unreferenced-first (simulating freed space per removal against
  each image's own `size_bytes`, now added to `ImageRef`) until usage
  drops to the low threshold or nothing eligible remains.
- `CriRuntime` gained a new `image_unreferenced_since: Mutex<HashMap<String,
  u64>>` side table (image id -> unix seconds first observed
  unreferenced), rebuilt incrementally every `gc_unreferenced_images()`
  cycle — an id currently referenced is dropped from tracking (its clock
  resets if it becomes unreferenced again later, an acceptable
  simplification vs. upstream's own richer access-time bookkeeping,
  which nodelet has no equivalent in-memory image manager for).
- **Real behavior change, not just an addition**: this round's own
  automated e2e test (`test_unreferenced_image_gc`) that proved
  unconditional removal had to be rewritten, since that behavior is now
  intentionally gone — replaced with
  `test_unreferenced_image_is_not_removed_below_the_watermark`, which
  proves the *opposite*: a freshly-unreferenced image survives a full GC
  cycle untouched on a normal (non-full) disk. Actual watermark-triggered
  removal joins the existing manual-procedure group in `gc.sh`
  (`test_image_gc_watermark_removal_manual_procedure`) — genuinely
  filling a disk, or restarting nodelet with an artificially-low
  threshold, isn't something this suite does to a live node
  automatically, same reasoning as the pre-existing orphaned-sandbox/
  eviction manual procedures.
- 15 new unit tests (`gc_tests/image_gc_watermark.rs`): `disk_usage_percent()`'s
  edge cases (half/empty/full disk, zero total bytes, available exceeding
  total), `should_start_image_gc()`'s threshold comparison, and
  `images_to_reclaim_space()`'s full behavior matrix (too-young image not
  eligible, an untracked image treated as not-yet-eligible rather than an
  error, stopping once the low threshold is already met, single- and
  multi-image removal, oldest-first ordering, no eligible candidates
  removes nothing even under pressure, empty input).

## Round 69: subPath $(VAR) expansion (2026-07-31, same day)

A fresh gap re-audit (checked kubernetes.io docs + CRI/K8s API struct
fields directly, similar to rounds 58/65) confirmed round 65's audit had
already exhausted itself — every candidate it found is now closed — and
found 3 new candidates: `subPathExpr` (this round, the cheapest, an
older already-tracked gap), image GC watermark policy (tracked, still
open), and ServiceAccount token credential providers (newly found,
beta/default-on in k8s 1.34 — a bigger, more novel feature needing the
credential-provider exec-plugin protocol, deferred pending a scope
discussion rather than picked by default).

Closes `volumeMounts[].subPathExpr` — a container's own resolved env
vars (most commonly Downward API ones, e.g. `$(POD_NAME)`) can be
referenced inside a `subPathExpr` to construct a per-pod subdirectory
name; this was entirely unimplemented (the field was never read at all).

- New pure `expand_sub_path_expr()` in `volumes_pure.rs`: substitutes
  every `$(VAR)` reference against the container's own env, matching
  upstream's actual implementation (which works against any of the
  container's env, not just Downward API ones, despite that being the
  documented headline use case). `$$` is a literal `$`, matching
  upstream's own escaping rule. Returns `None` — dropping the mount
  entirely, not substituting anything — if a referenced variable isn't
  found or a `$(` is never closed, matching real kubelet's own
  "fail the container rather than mount a garbage path" posture.
- `build_mounts()` gained a new `envs: &[KeyValue]` parameter (the same
  already-resolved container env list `create_and_start_container()` has
  in scope right before building mounts — no new resolution step
  needed) and now expands `subPathExpr` before falling back to a plain
  `subPath`, for both `HostPath`- and `Image`-backed volumes alike
  (`subPathExpr` is a `VolumeMount`-level field, not tied to any
  particular volume kind).
- 7 new unit tests (`cri_tests/mounts.rs`): single and multi-reference
  expansion, literal-text interleaving, `$$` escaping, an unresolvable
  reference dropping the mount, an unclosed `$(` dropping the mount,
  `subPathExpr` winning over a plain `subPath` when both are somehow
  set, and the same expansion applying to image-backed volumes too.
- Genuinely automated e2e test
  (`test_sub_path_expr_expands_a_downward_api_env_var` in
  `deploy/lib/test/cases/volumes.sh`): a container with a Downward-API
  `POD_NAME` env var and a `subPathExpr: $(POD_NAME)` mount writes a
  marker, which the test reads back from the host at the *expanded*
  path (`<volume>/<real-pod-name>/marker`) — real proof expansion
  happened, not just that the pod ran.

## Round 68: swap support (memorySwap.swapBehavior) (2026-07-31, same day)

Closes the last of round 65's fresh-audit candidates — `memorySwap.swapBehavior`
(GA in k8s 1.34). CRI already had native support
(`LinuxContainerResources.memory_swap_limit_in_bytes`), never wired up.

- New `NODELET_MEMORY_SWAP_BEHAVIOR` config knob (`config.rs`), default
  `NoSwap` (matching upstream's own kubelet default) — `LimitedSwap` is
  the only other accepted value. `config.rs` also gained
  `memory_swap_bytes` (node total swap capacity), read once at startup
  from `/proc/meminfo`'s `SwapTotal` via new `detect_swap_bytes()` —
  deliberately the *total* capacity, not currently-free swap, so a
  container's computed share stays stable regardless of what else on the
  node happens to be using swap at any given moment. `0` on a swapless
  node or any read failure (no fallback guess, unlike
  `detect_memory_bytes()`'s 2GiB fallback — a wrong swap-capacity guess
  would actively mislead the `LimitedSwap` formula, whereas 0 correctly
  means "nothing to allocate" either way).
- New pure `container_swap_limit_bytes()` in `resources.rs`, wired into
  `linux_resources()`'s existing memory-limit computation:
  - **`NoSwap`**: every container with a memory limit gets its combined
    memory+swap ceiling pinned to exactly that limit (zero *additional*
    swap) — CRI's field (and the underlying OCI runtime-spec field it
    maps to) is documented as the *combined* memory+swap total, mirroring
    cgroup v1's `memory.memsw.limit_in_bytes` naming even under cgroup
    v2 (where the runtime derives `memory.swap.max` by subtracting
    `memory.max` internally) — setting it to exactly the memory limit is
    what actually yields `memory.swap.max = 0`. A container with no
    memory limit at all is left unconfigured (nothing to peg a combined
    value to).
  - **`LimitedSwap`**: implements upstream's KEP-2400 formula exactly —
    `swapLimit = (containerMemoryRequest / nodeTotalMemory) * nodeTotalSwap`,
    with the container's proportional share defined as zero whenever
    `request == limit` (Guaranteed-shaped) or there's no memory request
    at all (BestEffort-shaped) — between those two rules alone, only
    genuinely Burstable-shaped containers ever get a nonzero share,
    without needing to compute the pod's overall QoS class at all
    (matching upstream's own documented "only Burstable" restriction as
    an emergent property of the per-container formula, not a separate
    QoS check). **Known scope limitation**: nodelet has no
    `--system-reserved`/`--kube-reserved`-equivalent knob for swap, so
    the full raw `SwapTotal` is used as `nodeTotalSwap` with nothing
    withheld — same simplification already accepted for ephemeral-storage
    (round 48) and hugepages (round 60) capacity reporting.
  - `resize_decision()` now also compares `memory_swap_limit_in_bytes`
    between desired/actual (not just `memory_limit_in_bytes`) — catches
    a request-only in-place resize, which changes a `LimitedSwap`
    container's swap share even when its memory *limit* doesn't move.
- 11 new unit tests (`cri_tests/swap_limit.rs`): `NoSwap` pinning
  behavior (with/without a memory limit, ignores node swap capacity
  entirely), `LimitedSwap`'s Guaranteed/BestEffort zero-share cases,
  Burstable's proportional share (exact value checked), the
  no-memory-limit Burstable case granting nothing, swap-share clamping
  to the node's total swap, and zero-node-memory/zero-node-swap
  degenerate cases (no divide-by-zero panics).
- Genuinely automated e2e test
  (`test_no_swap_default_disables_swap_via_cgroup` in
  `deploy/lib/test/cases/resources.sh`): proves the *default* `NoSwap`
  behavior end to end — a memory-limited container's real
  `/sys/fs/cgroup/memory.swap.max` reads exactly `0`. `LimitedSwap`
  itself needs restarting nodelet with a different env var, so it gets a
  manual-spot-check note (`test_limited_swap_manual_note`), same pattern
  as `eviction.sh`'s pressure-based procedures.
- This closes every candidate round 65's fresh gap re-audit found.

## Round 67: emptyDir.sizeLimit enforcement (2026-07-31, same day)

Closes the last of round 65's fresh-audit candidates that wasn't swap
support — `emptyDir.sizeLimit` had been tracked as an open gap since
early rounds (a plain-disk `emptyDir` volume had no size cap at all,
unlike its `Memory`/`HugePages`-medium siblings, whose `sizeLimit` is
already a real kernel-enforced cap at mount time via rounds 30/61).

- `PodUsage` (`runtime/mod.rs`) gained `empty_dir_usage_bytes:
  HashMap<String, u64>`, populated in `pod_usage_from_sandbox_stats()`
  by measuring every immediate subdirectory of `VOLUME_ROOT/<uid>/volumes/`
  with the same `directory_usage_bytes()` walk round 49's ephemeral-storage
  total already uses — reported unconditionally for every materialized
  volume regardless of kind, keeping the runtime-side measurement fully
  decoupled from the Pod spec (the eviction-side check below is what
  decides which entries actually matter).
- New pure `empty_dir_size_limits()`/`first_empty_dir_over_limit()` in
  `eviction.rs`: the first finds every plain-disk `emptyDir` volume with
  `sizeLimit` set (explicitly excluding `Memory`/`HugePages`-medium ones
  — their cap is already kernel-enforced, nothing for measurement-based
  eviction to add), the second finds the first volume (if any) whose
  measured usage exceeds its own limit.
- `main.rs`'s eviction loop gained a new check, structured identically to
  round 49's whole-pod ephemeral-storage check and evaluated right after
  it (same "direct per-pod resource violation, checked ahead of general
  node pressure" reasoning) — a pod with even one emptyDir volume over
  its own `sizeLimit` is evicted immediately, with a reason message
  naming the specific offending volume.
- 12 new unit tests (both `cri`-gated and mock-only, since this logic
  lives in `eviction.rs`, not the CRI runtime): size-limit extraction
  (disk-backed reported, no-limit not reported, `Memory`/`HugePages`-medium
  never reported even with a limit set, non-emptyDir volumes ignored,
  multiple volumes each get their own entry) and violation detection
  (over/at/under limit, unmeasured usage never violates, only the
  actually-violating volume among several is returned).
- Genuinely automated e2e test
  (`test_pod_exceeding_an_empty_dir_size_limit_is_evicted` in
  `deploy/lib/test/cases/eviction.sh`), mirroring round 49's
  ephemeral-storage eviction test exactly: a pod with **no**
  ephemeral-storage limit at all, only a `1Mi` `emptyDir.sizeLimit`,
  writes an 8Mi file into it and is confirmed evicted — proving the
  per-volume check fires independently of the whole-pod one.
- **Still open**: swap support (`memorySwap.swapBehavior`, GA 1.34) —
  the last of round 65's audit candidates.

## Round 66: lifecycle.stopSignal (2026-07-31, same day)

Closes the cheapest of the 2 remaining candidates round 65's fresh gap
re-audit found: `lifecycle.stopSignal` (GA in k8s 1.33) — CRI's proto
already had direct native support (`ContainerConfig.stop_signal` at
creation, `ContainerStatus.stop_signal` for reading back the effective
signal), nobody had wired either side up.

- **Write side** (real functional behavior, not just status reporting):
  `container_create.rs`'s `create_and_start_container()` now sets
  `ContainerConfig.stop_signal` from `container.lifecycle.stopSignal`,
  via new pure `stop_signal_cri()` in `resources.rs`. k8s and CRI define
  exactly the same 65-signal set (confirmed against upstream docs — "one
  of sixty-five additional stop signals"), just spelled differently: CRI's
  generated enum strips the shared `SIGNAL_` prefix, and since proto
  identifiers can't contain `+`/`-`, real-time signal offsets become
  `PLUS`/`MINUS` words instead of the literal symbol (`SIGRTMIN+5` ->
  `Sigrtminplus5`). Translating is therefore purely a naming-convention
  exercise (same shape as round 59's `hugepage_cri_page_size()`) —
  `stop_signal_cri()` re-derives the proto's own constant name
  (`format!("SIGNAL_{normalized}")`) and lets prost-generated
  `Signal::from_str_name()` do the matching, rather than hand-maintaining
  a 65-entry lookup table.
- **Read side**: new pure `stop_signal_k8s()` (the exact inverse) maps
  CRI's `ContainerStatus.stop_signal` — the *effective* signal, as
  actually reported by the runtime — back to k8s's spelling for
  `containerStatuses[].stopSignal`. `ContainerRuntimeStatus` gained a new
  `stop_signal: Option<String>` field, threaded through `pods.rs`'s 3
  `ContainerStatus` construction sites (app/init/ephemeral). **Deliberately
  scoped to exited containers only**: `container_status_details()` (CRI's
  `ContainerStatus` RPC) is already called for every exited container
  (termination details), so reading `stop_signal` off that same response
  costs nothing extra; a still-*running* container's `stopSignal` isn't
  populated, since fetching it would mean a brand-new per-container RPC
  on every reconcile of a healthy pod — directly against this codebase's
  low-idle-cost design that's held throughout every prior round. A
  documented scope limitation, not an oversight.
- 7 new unit tests (`cri_tests/stop_signal.rs`): plain-name translation,
  real-time-signal `+`/`-` handling (both symbol and word spelling),
  unrecognized input, full round-trip for every tested signal,
  `RuntimeDefault` mapping to `None` not a sentinel string, an
  out-of-range CRI value also mapping to `None`.
- Genuinely automated e2e test
  (`test_lifecycle_stop_signal_is_honored_by_the_runtime` in
  `deploy/lib/test/cases/lifecycle.sh`): the container traps `SIGUSR1`
  (deliberately *not* the default `SIGTERM` a plain `sleep` would just
  silently die to) and writes a marker before exiting — real proof the
  configured signal was actually delivered, not just that the pod ran.
  Reads the marker off the host-materialized `emptyDir` directory (left
  in place after pod teardown, a pre-existing documented simplification)
  since the Pod object itself may already be gone from the apiserver by
  the time this checks, making status-field polling unreliable here.
- **Still open**: swap support (`memorySwap.swapBehavior`, GA 1.34) and
  `emptyDir.sizeLimit` enforcement, both surfaced/reconfirmed by round
  65's audit.

## Round 65: hostPath volumes (2026-07-31, same day)

Closes a long-standing gap this doc has tracked since early rounds:
`spec.volumes[].hostPath` was explicitly unsupported — a pod requesting
one got a `warn!` and nothing mounted at all. Found again in a fresh gap
re-audit (this round's own audit, not carried from round 58) — arguably
the single most commonly-needed volume type for a single-device/edge
kubelet target (host directories, device files, sockets), so a natural
pick once surfaced.

- `runtime/cri/volumes_resolve.rs`'s `resolve_volumes()` gained a
  `v.host_path` branch, reusing the existing `ResolvedVolume::HostPath`
  variant directly — unlike every other volume kind, this one resolves
  to the host's *own* existing path (`PathBuf::from(&hp.path)`), not
  something materialized under nodelet's `VOLUME_ROOT`. `subPath`/
  `readOnly` handling in `build_mounts()` needed zero changes since it
  already worked generically against any `ResolvedVolume::HostPath`.
- New pure `validate_host_path()` in `volumes_pure.rs` implements real
  kubelet's own `hostPath.type` semantics exactly: unset/`""` performs no
  check at all (the legacy default); `DirectoryOrCreate`/`FileOrCreate`
  create an empty directory (mode `0755`) / file (mode `0644`) only if
  nothing exists there yet, and perform no check if something already
  does; `Directory`/`File`/`Socket`/`CharDevice`/`BlockDevice` all
  require something of that exact kind to already exist, erroring
  otherwise (checked via `std::os::unix::fs::FileTypeExt`). A validation
  failure is logged and the volume skipped — nodelet's existing
  best-effort posture for every other unresolvable volume kind, not a
  hard pod-admission rejection (nodelet has no admission layer to do
  that from at all).
- 10 new unit tests covering every `type` value's create/no-create/
  wrong-kind/missing paths against a real synthetic scratch directory
  (the syscalls themselves aren't mockable, but the branching per `type`
  value is exactly what's worth verifying).
- 3 genuinely automated e2e tests in `deploy/lib/test/cases/volumes.sh`:
  a real proof-by-construction test that writes a marker file directly
  on the host filesystem (bypassing nodelet entirely) *before* the pod
  exists, reads it back from inside the container, and writes something
  back that must land on the host — proving a real bind mount, not a
  copy; a `DirectoryOrCreate` test proving the directory actually gets
  created; and a `Directory` (no `OrCreate`) test proving the *opposite*
  — that it does NOT create anything for a path that doesn't exist.
- **Still open, unchanged from before this round**: `emptyDir.sizeLimit`
  enforcement, subPath `$(VAR)` expansion.

## Round 64: DRA follow-ups — reservedFor gating + RPC batching (2026-07-31, same day)

Closes both known scope limitations round 63 flagged, and corrects a
factual error in round 63's own docs along the way.

- **Correction, not a new gap**: round 63's docs said real kubelet writes
  `ResourceClaim.status.reservedFor` and nodelet was missing that. This
  was wrong — `reservedFor` is written by the **scheduler** at bind time
  (the dynamicresources scheduler plugin), not by kubelet. Kubelet's
  actual job is to **gate** `NodePrepareResources` on this pod already
  being listed there, as a safety check against acting on a claim not
  (yet, or no longer) genuinely reserved for it. That gate — the real
  gap — is what this round adds: new pure `pod_is_reserved_for_claim()`
  checks `status.reservedFor` for an entry with `resource: "pods"`
  matching this pod's name *and* UID (UID, not just name, since a
  delete+recreate changes UID but could reuse the name). A claim not yet
  reserved isn't treated as an error — it's a normal timing window before
  the scheduler finishes binding — so `resolve_pod_claim_devices()` just
  skips it for that reconcile and retries on the next one.
- **RPC batching**: `dra.rs`'s `prepare()`/`unprepare()` now take a
  `&[ClaimRef]` slice instead of a single `ClaimRef`, issuing one
  `NodePrepareResources`/`NodeUnprepareResources` call covering every
  claim a given driver owns for this pod, rather than one call per
  (driver, claim) pair — matching how the real protocol's `claims` list
  field is meant to be used. Per-claim failures inside a batch (reported
  via the response's own `error` field) don't fail the whole batch; new
  pure `map_prepare_results()`/`map_unprepare_results()` do this
  per-claim-UID mapping and are unit-tested independent of any live
  driver connection.
- `runtime/cri/claims.rs`'s `resolve_pod_claim_devices()`/
  `unprepare_pod_claim_devices()` were restructured accordingly: resolve
  and reservedFor-check every claim first, then group by driver *across
  all* of the pod's claims (not per-claim) before issuing RPCs.
- 13 new unit tests: 6 for `pod_is_reserved_for_claim()` in
  `cri_tests/dra_claim_devices.rs` (no status, empty reservedFor,
  matching name+UID, wrong pod, matching name but stale UID, one match
  among several entries) and 7 for `map_prepare_results()`/
  `map_unprepare_results()` in a new `dra_tests/batch_results.rs`
  (every requested claim gets an entry, successful devices translate,
  a claim-level error doesn't fail the batch, a claim missing from the
  response is a synthetic error for prepare but a benign no-op for
  unprepare, empty claims list is a no-op).
- `deploy/lib/test/cases/dra.sh`'s manual-spot-check note updated to
  mention checking `status.reservedFor` (not just `status.allocation`)
  and verifying only one `NodePrepareResources` call per driver appears
  in the driver's own logs when a pod has multiple claims from the same
  driver.
- This closes both of round 63's flagged limitations. Still open, same
  as before: the DRA plugin proto (`proto/draplugin.proto`) was
  reconstructed from public documentation rather than vendored from
  upstream, so its exact wire format remains unverified against a live
  third-party driver; and there's still no genuinely automated e2e test
  (needs a real driver binary this bash-only harness can't stand up).

## Round 63: Dynamic Resource Allocation (2026-07-31, same day)

Implements the kubelet-side of DRA (`spec.resourceClaims`) — closes the
item round 58's re-audit found and explicitly flagged as "not necessarily
recommended" under the project's old single-node-only framing. That
framing changed after round 62 (see the scope note below round 61/62's
entries): the project is now meant to be a genuine multi-node-capable
kubelet replacement, which reopens DRA as a real target rather than a
flagged-for-completeness item.

**What's implemented** (the actual kubelet responsibilities — reading an
already-made allocation and asking the owning driver(s) to prepare/
unprepare it; nodelet never allocates a claim itself, that's the
scheduler's/a driver's own controller's job):
- `proto/draplugin.proto` — the kubelet<->DRA-driver `NodePrepareResources`/
  `NodeUnprepareResources` gRPC protocol, reconstructed from public
  documentation of `k8s.io/kubelet/pkg/apis/dra/v1beta1` (not vendored
  from the upstream Go module directly, which isn't available to generate
  Rust bindings from in this environment). **Field numbers/names are
  believed accurate but have not been validated against a live
  third-party DRA driver** — flagged explicitly in the proto file's own
  comment and in the new e2e test's manual-note skip message. Verify
  against a real driver before relying on this for third-party interop.
- `dra.rs` (new module) — `DraDrivers`: a driver-name -> endpoint registry
  (same shape as `runtime::csi::CsiDrivers`' endpoint map; no
  `ListAndWatch` inventory stream needed, unlike device plugins, since
  DRA drivers don't report inventory to kubelet at all) plus the
  `prepare()`/`unprepare()` RPC calls themselves.
- `plugin_registry.rs` — extended to a 3rd `PluginInfo.type`:
  `"DRAPlugin"` routes to `dra::DraDrivers`, reusing the exact same
  `GetInfo`/`NotifyRegistrationStatus` handshake CSI drivers (round 12/13)
  and device plugins (round 14) already use. `register_one()`/`run()`
  gained a 3rd `Arc<DraDrivers>` parameter.
- `runtime/cri.rs`:
  - `resolve_pod_claim_devices()` (new, called once per `ensure_pod()`
    reconcile, before any container is created — zero cost for a pod with
    no `resourceClaims`): resolves each `spec.resourceClaims[]` entry to
    its actual `ResourceClaim` object name (direct, or via
    `status.resourceClaimStatuses` for a template-based claim not yet
    materialized by the resource-claim controller in
    kube-controller-manager), fetches it, groups
    `status.allocation.devices.results` by driver name, and calls
    `NodePrepareResources` per driver — returning a
    pod-claim-name -> `PreparedPodClaim` map (CDI device IDs, split by
    which request produced them).
  - `unprepare_pod_claim_devices()` (new, called from `remove_pod()`,
    mirrors `unmount_csi_volumes()`'s exact placement/shape — re-derives
    from the Pod object rather than tracking prepared state separately):
    calls `NodeUnprepareResources` for every driver that had something
    prepared.
  - `create_and_start_container()` now builds each container's CRI
    `ContainerConfig.cdi_devices` from its `resources.claims[].{name,
    request}` entries (a field CRI's vendored proto already had — same
    "proto already supports it, nobody wired it up" shape as round 53's
    `Status` RPC, round 57's `Version` RPC, and round 59's
    `hugepage_limits`) — an unset `request` means the whole claim's
    devices, a set one filters to that specific request's devices
    (matching either the request name exactly or a
    `<request>/<subrequest>` prefix, same rule real kubelet's own
    matching uses for `firstAvailable` subrequests).
  - New pure functions (all unit-tested independent of any live claim/
    driver): `resource_claim_object_name()`, `allocated_devices_by_driver()`,
    `cdi_devices_for_container_claim()`.
  - `ensure_container()`/`ensure_ephemeral_container()`/
    `ensure_init_containers()`/`create_and_start_container()` all gained a
    new `claim_devices: &HashMap<String, PreparedPodClaim>` parameter,
    threaded from the single `resolve_pod_claim_devices()` call in
    `ensure_pod()`.
- 18 new unit tests: 5 in `dra_tests/claim_ref.rs` (`ClaimRef`->wire
  translation, driver registry register/deregister, proto-device
  translation) and 13 in `cri_tests/dra_claim_devices.rs` (claim-name
  resolution — direct/template/not-yet-resolved/direct-wins-over-status;
  driver grouping — unallocated/single/multi-driver/multi-device-same-driver;
  container-claim CDI matching — unknown claim/no-request-filter/specific-
  request/subrequest-prefix/no-match).

**What's NOT implemented** (deliberate scope boundaries, same "genuinely
not kubelet's job" reasoning as elsewhere in this doc):
- Writing this node into `ResourceClaim.status.reservedFor` — real
  kubelet does this as a still-in-use marker the claim-deallocation
  safety check depends on; nodelet only reads `status.allocation`, never
  writes back. A known scope limitation, not a "someone else's job" case
  — worth closing in a later round if DRA sees real use.
- Batching multiple claims into one `NodePrepareResources` call — the
  real protocol supports a claims list; nodelet calls it once per
  (driver, claim) pair. Correct, just not the most RPC-efficient shape;
  acceptable given DRA claim counts per pod are typically small.
- A genuinely automated e2e test: exercising a real
  `NodePrepareResources`/CDI-injection round-trip needs an actual DRA
  driver binary (a kubelet-plugin implementing the protocol) plus a
  `DeviceClass`/`ResourceSlice`/`ResourceClaim` the scheduler can actually
  allocate — none of which this bash-only harness can stand up. New
  `deploy/lib/test/cases/dra.sh` follows the exact same honest-limitation
  pattern `device_plugins.sh` already established (skip-with-manual-
  spot-check-instructions), plus a real, automated check that
  `plugin_registry.rs`'s registry directory now also watches for
  `DRAPlugin` registrations.

## Round 62: securityContext.supplementalGroupsPolicy (2026-07-31, same day)

Closes the `supplementalGroupsPolicy` item from round 58's re-audit
(GA in k8s 1.33): `securityContext.supplementalGroupsPolicy` was never
read at all before this — every pod effectively got the `Merge` behavior
regardless of what it actually asked for, with no way to opt into
`Strict`.

- CRI has direct native support for exactly this: both
  `LinuxSandboxSecurityContext.supplemental_groups_policy` (pod/sandbox
  level) and `LinuxContainerSecurityContext.supplemental_groups_policy`
  (container level) — no image inspection or `/etc/group` parsing needed
  on nodelet's side, the runtime does that itself.
- New pure `supplemental_groups_policy_cri(policy: Option<&str>)`
  translates the k8s API's `Option<String>` (`"Merge"`/`"Strict"`) to
  CRI's own `SupplementalGroupsPolicy` enum. `None`/unset or anything
  other than the literal `"Strict"` maps to `Merge`, matching both the
  k8s API's documented default and CRI's own proto doc comment
  ("If not specified, Merge is used") — fails open on garbage input
  rather than erroring, same posture as other enum-shaped translations.
- Wired into both `sandbox_config()` (which needed a new `pod_sc`
  parameter threaded through `run_sandbox()` and its 2 callers in
  `ensure_pod()`) and `linux_security_context()` (already had `pod_sc`).
  The 2 throwaway `sandbox_config()` calls used only to route
  `PullImageRequest.sandbox_config` pass `None` — irrelevant there, no
  actual container is created from that config.
- 6 new unit tests: 3 in `cri_tests/sandbox_config.rs` (no pod security
  context defaults to Merge, explicit Strict/Merge both wired through
  correctly) and 3 in `cri_tests/linux_security_context.rs` (default,
  explicit Strict, and an unrecognized value falling back to Merge).
- Genuinely automated e2e test
  (`test_supplemental_groups_policy_strict_ignores_image_group_membership`
  in `deploy/lib/test/cases/security.sh`): rather than depend on
  `$TEST_IMAGE`'s own baked-in `/etc/group` (fragile across images/tags),
  the test supplies its own `/etc/passwd`/`/etc/group` via a ConfigMap
  mounted with `subPath` — a portable, image-independent proof. A
  `testuser` (uid 2000, primary gid 3000) is listed as an extra member of
  `imagegroup` (gid 4000) in the supplied `/etc/group`, on top of an
  explicit `supplementalGroups: [5000]`. Two sibling containers (one
  `Merge`, one `Strict`) both run `id -G`: `Merge` must include both 4000
  (from the image's own group file) and 5000 (explicit); `Strict` must
  include 5000 but must NOT include 4000. Skips cleanly if the pod never
  reaches Running (older containerd without CRI's
  `SupplementalGroupsPolicy` field support).
- **Not translated**: CRI's `RuntimeHandlerFeatures.supplemental_groups_policy`
  capability-advertisement flag (from the runtime's `Status` RPC) isn't
  consulted before setting the field — same as every other CRI field
  translation in this codebase, a runtime that doesn't support it is
  expected to reject the RPC rather than nodelet pre-checking; there's
  also no equivalent Node-API-facing field to surface it on even if it
  were checked (`NodeRuntimeHandlerFeatures` only exposes
  `recursiveReadOnlyMounts`/`userNamespaces`).

## Round 61: HugePages emptyDir.medium volume support (2026-07-31, same day)

Closes the last of round 58's 3 HugePages pieces:
`emptyDir.medium: "HugePages"`/`"HugePages-<size>"` was never
implemented at all — such a volume was silently materialized on regular
disk, exactly like the unset default medium, giving no huge-page backing
whatsoever.

- `runtime/cri.rs`'s emptyDir-creation branch now mounts `hugetlbfs`
  (via `mount(8)`, the same host-tool approach round 30's tmpfs support
  already established — CRI's `Mount` struct only binds an *existing*
  host path, it has no concept of controlling the filesystem type
  backing it) whenever `medium` is `HugePages` or `HugePages-<size>`.
- New pure `is_hugepages_medium_empty_dir()`, mirroring
  `is_memory_medium_empty_dir()` (round 30).
- New pure `hugepages_medium_pagesize_option(medium)` converts
  `HugePages-<size>`'s k8s binary suffix (`Mi`/`Gi`/`Ki`) to hugetlbfs's
  own `pagesize=` mount option spelling — the same naming-convention-only
  translation (strip the trailing `i`) as round 59's
  `hugepage_cri_page_size()`, since the Linux kernel's mount-option
  parser already reads a bare `K`/`M`/`G` suffix as base-1024. A bare
  `HugePages` medium (no specific size) omits `pagesize=` entirely,
  letting the kernel use its own default huge page size.
- New pure `hugetlbfs_mount_args()`, mirroring `tmpfs_mount_args()`:
  builds `mount -t hugetlbfs [-o pagesize=<unit>[,size=<bytes>]] none
  <path>`, combining `pagesize=` and `sizeLimit`'s `size=` into a single
  `-o` option when both are present.
  `mount_hugetlbfs_empty_dir()` is best-effort, same graceful-degradation
  posture as tmpfs: a failure (e.g. no hugepages reserved, or hugetlbfs
  itself unavailable) is logged and falls back to the already-created
  plain-disk directory rather than failing the whole pod.
- `unmount_memory_backed_empty_dirs()` (round 30) is renamed
  `unmount_special_medium_empty_dirs()` and now also unmounts
  `HugePages`/`HugePages-<size>`-medium volumes on pod teardown, since a
  hugetlbfs mount is real reserved memory that must be given back, same
  reasoning as tmpfs.
- 12 new unit tests in a new `cri_tests/hugetlbfs_empty_dir.rs`: medium
  detection (none/Memory/bare `HugePages`/sized `HugePages-2Mi`,
  case-sensitivity), pagesize-option conversion (bare/2Mi/1Gi/64Ki), and
  mount-args construction (bare, sized, size-limit-only, size-limit
  combined with pagesize, zero/negative size-limit treated as unset).
- Genuinely automated e2e test
  (`test_empty_dir_medium_hugepages_is_backed_by_hugetlbfs` in
  `deploy/lib/test/cases/volumes.sh`): creates a pod with a
  `HugePages-2Mi` emptyDir (plus the matching `hugepages-2Mi` and
  `memory` resource limits real k8s validation requires alongside any
  HugePages-medium volume) and checks the host mountpoint's actual
  filesystem type via `stat -f`, same proof-by-construction approach as
  round 30's tmpfs test. Skips cleanly (not a hard failure) if the node
  has no 2Mi hugepages reserved, mirroring rounds 59/60's hugepages
  tests' skip conditions.
- This closes round 58's HugePages audit item entirely — all 3 pieces
  (container limits, capacity/allocatable reporting, emptyDir medium) are
  now implemented.

## Round 60: HugePages Node.status.capacity/.allocatable reporting (2026-07-31, same day)

Closes the second of round 58's 3 HugePages pieces:
`Node.status.capacity`/`.allocatable["hugepages-<size>"]` was never
populated at all — a node with hugepages reserved never advertised them as
schedulable capacity, so the scheduler had no way to know they existed.

- `node.rs::hugepages_capacity_map(base_path)` scans
  `/sys/kernel/mm/hugepages/hugepages-<size>kB/` (real path:
  `/sys/kernel/mm/hugepages`, parameterized purely for unit testing against
  a synthetic tree), reading each pool's `nr_hugepages` count and
  multiplying by its page size in bytes. Pool sizes with `nr_hugepages == 0`
  (unreserved) are omitted entirely rather than reported as zero, matching
  real kubelet: an unreserved size isn't schedulable capacity at all.
- New pure `hugepage_size_kb_to_k8s_suffix(size_kb)` mirrors real cadvisor's
  own naming: the largest whole binary unit that divides the pool's kB size
  evenly (2048kB → `"2Mi"`, 1048576kB → `"1Gi"`, an odd size falls back to
  `"Ki"`) — the inverse direction of round 59's
  `hugepage_cri_page_size()`.
- `capacity_map()` now merges `hugepages_capacity_map(HUGEPAGES_SYSFS_ROOT)`
  in alongside `cpu`/`memory`/`pods`/`ephemeral-storage`.
  `allocatable_map()` needed no changes: it already only touches `cpu` and
  `memory` explicitly and clones everything else through untouched, so any
  `hugepages-<size>` capacity entries pass straight through to allocatable
  unmodified — correct, since there's no `--system-reserved`/
  `--kube-reserved`-equivalent knob for hugepages (a pool is either reserved
  by the kernel/bootloader or it isn't; nothing to subtract).
- 7 new unit tests in a new `node_tests/hugepages_capacity_map.rs`: suffix
  conversion (2Mi/1Gi/64Ki), nonexistent root returns empty (not an error),
  a reserved pool reports `count * page_size` bytes exactly, an unreserved
  pool (`nr_hugepages == 0`) is omitted rather than reported as zero, and
  multiple simultaneously-reserved pool sizes are all reported.
- Genuinely automated e2e test
  (`test_node_reports_hugepages_capacity_when_reserved` in
  `deploy/lib/test/cases/node_status.sh`): reads real
  `/proc/sys/vm/nr_hugepages` on the test runner, skips cleanly if it's 0
  (nothing reserved to report), otherwise asserts the node's
  `status.capacity["hugepages-*"]` key is present with a positive byte
  count and that `status.allocatable` matches it exactly.
- **Still open** (the last of round 58's 3 HugePages pieces):
  `emptyDir.medium: "HugePages"`/`"HugePages-<size>"` volume support
  (closer in shape to round 30's tmpfs `emptyDir.medium: Memory` work — a
  real hugetlbfs mount, not a CRI or `/sys` read).

## Round 59: HugePages container limits (2026-07-31, same day)

Closes the cheapest of the 3 pieces round 58's re-audit found under
**HugePages**: container `resources.limits["hugepages-<size>"]` was never
translated to CRI at all — a pod requesting hugepages ran with no hugetlb
cgroup limit set whatsoever, silently ignoring the request.

- `runtime/cri.rs::linux_resources()` now populates
  `LinuxContainerResources.hugepage_limits` — a field CRI's vendored proto
  already had, just never read or written until now (same "proto already
  supports it, nobody wired it up" shape as round 53's `Status` RPC and
  round 57's `Version` RPC).
- New pure `hugepage_limits(limits: Option<&BTreeMap<String, Quantity>>)`
  extracts every `hugepages-<size>` key from a container's
  `resources.limits` and converts each to a CRI `HugepageLimit`.
- New pure `hugepage_cri_page_size(k8s_suffix: &str)` converts k8s's
  `Ki`/`Mi`/`Gi` binary suffix to CRI's `page_size` format (`"2MB"`,
  `"1GB"`, `"64KB"`) — despite looking decimal, this is a **naming
  convention only**: CRI's own proto doc comment confirms `page_size` is
  still parsed as base-1024, so this is a string-format translation, not a
  numeric unit conversion. The byte value itself (`limit`) is copied
  through unchanged. A suffix without the trailing `i` (malformed —
  real hugepage resource names always use the binary suffix, apiserver
  validation enforces this) is rejected rather than mistranslated.
- 7 new unit tests: 3 in `cri_tests/linux_resources.rs` (single size,
  multiple sizes, empty when absent) and 4 in a new dedicated
  `cri_tests/hugepage_cri_page_size.rs` (Mi/Gi/Ki conversion, malformed
  suffix rejection).
- Genuinely automated e2e test
  (`test_hugepages_limit_is_enforced_via_cgroup` in
  `deploy/lib/test/cases/resources.sh`): creates a pod with a
  `hugepages-2Mi` limit (plus the memory limit real k8s validation
  requires alongside any hugepages limit) and reads back
  `/sys/fs/cgroup/hugetlb.2MB.limit_in_bytes` from inside the container via
  a shared emptyDir, asserting it matches exactly. Skips cleanly (not a
  hard failure) if the node has no 2Mi hugepages reserved
  (`/proc/sys/vm/nr_hugepages`) or the hugetlb cgroup controller isn't
  enabled — genuinely outside nodelet's control, same reasoning as round
  45's cgroup-hierarchy tests' skip conditions.
- **Still open** (the other 2 of round 58's 3 HugePages pieces):
  `Node.status.capacity`/`.allocatable["hugepages-<size>"]` reporting
  (would read `/sys/kernel/mm/hugepages/`, no CRI RPC involved), and
  `emptyDir.medium: "HugePages"`/`"HugePages-<size>"` volume support
  (closer in shape to round 30's tmpfs `emptyDir.medium: Memory` work — a
  real mount, not a CRI concept).

## Round 58: fresh gap re-audit (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 22/27/35/39/45/50/54).
All 5 prior audit lists were fully closed as of round 57; user picked
another re-audit. No code changed this round — audit-only.

This pass checked newer/less-common resource and security-context
surface (hugepages, `supplementalGroupsPolicy`, Dynamic Resource
Allocation) rather than a specific feature area, since the bread-and-
butter CRI/lifecycle/volume/probe surface has now been swept
repeatedly across 7 prior audits. Found 3 previously-untracked items:

- **HugePages support is entirely missing** — confirmed via grep: zero
  references anywhere in this codebase to `hugepages`/`HugePage`
  besides the `hugepages-2Mi` string already used as an *error-path*
  test fixture in `resource_field_ref.rs` (round 44's "unsupported
  resource name" case — not real support). Three distinct pieces, of
  increasing cost: **(1)** container `resources.limits["hugepages-<size>"]`
  translation to CRI's `LinuxContainerResources.hugepage_limits` —
  CRI has direct, dedicated native support for this (`HugepageLimit`
  message, matching the `hugetlb.<size>.limit_in_bytes` cgroup file
  exactly), making this the cheapest slice, similar in shape to the
  existing cpu/memory limit translation in `linux_resources()`;
  **(2)** `Node.status.capacity`/`.allocatable["hugepages-<size>"]`
  reporting (would need reading `/sys/kernel/mm/hugepages/`, no CRI
  RPC involved); **(3)** the `emptyDir`-like hugepages volume medium
  (`volumes[].emptyDir.medium: "HugePages"`/`"HugePages-<size>"`) —
  the biggest and most CRI-adjacent-but-still-manual piece, closer in
  shape to round 30's tmpfs `emptyDir.medium: Memory` work (a real
  mount, not a CRI concept) than to the container-limit piece.
- **`securityContext.supplementalGroupsPolicy`** (GA 1.33) — controls
  whether `supplementalGroups`/fsGroup-derived groups *merge with* or
  *strictly replace* the groups the container image's own `/etc/group`
  would otherwise assign the container's user; nodelet's existing
  `linux_security_context()` sets `supplemental_groups` but never reads
  this policy field at all, so it always behaves as the implicit merge
  case regardless of what the pod spec asks for. Moderate value — a
  real, if narrow, security-hardening knob some workloads (especially
  ones with a restrictive base image `/etc/group`) depend on.
- **Dynamic Resource Allocation (`spec.resourceClaims`)** — zero
  references anywhere in this codebase. A newer (GA-track), genuinely
  complex feature: `ResourceClaim`/`ResourceClaimTemplate`/`DeviceClass`
  cluster objects, plus a kubelet-side gRPC "DRA plugin" protocol
  roughly analogous in shape to device plugins (round 14/21) but
  distinct in its own right (separate proto, separate
  allocate/prepare/unprepare lifecycle). Flagged for completeness, not
  necessarily recommended as a near-term target — likely a large,
  multi-round arc on its own, and the value-to-complexity ratio for a
  single-node edge kubelet (vs. a cluster with many nodes actually
  needing DRA's cross-node device-sharing story) is genuinely
  questionable; worth a deliberate go/no-go conversation before
  starting rather than defaulting to "implement it."

**Not re-flagging**: AppArmor profile / SELinux options (already a
documented, pre-existing gap in the `securityContext` responsibility
line, not new this round), and everything else already tracked below.

## Round 57: `ContainerStatus.containerID` scheme prefix (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-56). This closes
round 54's audit list entirely.

Before this round, `ContainerStatus.containerID`/`state.terminated.containerID`
reported CRI's bare container ID with no prefix at all — real kubelet
always formats these as `<runtimeName>://<id>` (e.g.
`containerd://1234abcd...`), built from CRI's own `Version` RPC (never
called anywhere in this codebase before this round — distinct from the
`Status` RPC round 53 already wired up).

- **`CriRuntime` gained a `runtime_name: String` field**, populated by
  a one-time `Version` RPC call in `connect()`. Best-effort: a failure
  falls back to `"unknown"` rather than failing `connect()` outright —
  a cosmetic formatting detail isn't worth blocking startup over,
  matching this file's existing posture toward non-essential CRI calls.
- **New pure `format_container_id(runtime_name, id) -> String`**
  (`"{runtime_name}://{id}"`), applied right where
  `ContainerRuntimeStatus.container_id` is populated from CRI's raw ID
  (both construction sites — app containers in `build_status()`,
  init/ephemeral in `build_labeled_container_statuses()`) — fixing it
  at the source means every downstream consumer of that one field
  (`ContainerStatus.containerID` *and* `state.terminated.containerID`,
  which both read the same `ContainerRuntimeStatus.container_id`) gets
  the prefix without needing its own formatting logic.
- No new env vars.
- 2 new unit tests (`cri_tests/format_container_id.rs`): the basic
  combination, and the `"unknown"` fallback name still producing a
  well-formed (if generic) ID rather than something broken.
- New e2e test (`deploy/lib/test/cases/lifecycle.sh`):
  `test_container_status_container_id_has_a_runtime_scheme_prefix` —
  asserts the real `containerID` contains `"://"` (not asserting a
  specific runtime name, since that varies by deployment).

**Confidence note**: the field-mapping logic is pure and unit-tested;
the e2e test is genuinely live proof of the scheme-separator shape,
though it doesn't pin an exact runtime name (correctly so — that's
deployment-specific, not something to hardcode an expectation about).

## Round 56: `PodStatus.hostIPs` (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-55). Offered the 2
remaining round-54 findings; user picked this one over the
`containerID` scheme prefix (which needs a new CRI RPC call and cached
state, more involved).

Before this round, only the singular `PodStatus.hostIP` was populated
— the plural `hostIPs` (dual-stack-ready) field was never set at all,
even though this same struct's `podIP`/`podIPs` singular/plural split
was already handled correctly.

- **`build_pod_status()`** now also sets `host_ips: Some(vec![HostIP {
  ip: host_ip.to_string() }])` alongside the existing `host_ip` field —
  real kubelet sets this unconditionally, even on a single-stack node
  (a one-element list containing the same address `hostIP` has), so
  there's no dual-stack detection needed, just mirroring the one
  address nodelet already has into the plural shape.
- No new env vars, no new proto surface — pure logic, one new field on
  an existing struct literal.
- 1 new unit test (`pods_tests/build_pod_status.rs`): `hostIPs[0].ip`
  matches the singular `hostIP` exactly.
- New e2e test (`deploy/lib/test/cases/lifecycle.sh`):
  `test_pod_status_reports_host_ips_plural` — asserts a real pod's
  `status.hostIPs[0].ip` equals its `status.hostIP`.

**Confidence note**: minimal, low-risk change — a single new field
derived directly from a value nodelet already computes, both unit- and
e2e-tested. High confidence.

## Round 55: `PodStatus.qosClass` (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-54). Offered the 3
round-54 findings; user picked this one — round 54's own prediction
that it'd be the cheapest, confirmed correct in practice.

Before this round, `PodStatus.qosClass` was never set at all —
`eviction::qos_class()` already computed exactly this value internally
(used for eviction ranking since round 7), it just never surfaced into
the pod's own status.

- **`QosClass` gained `as_str() -> &'static str`**, matching real
  kubelet's exact `PodQOSClass` API constants (`"BestEffort"`/
  `"Burstable"`/`"Guaranteed"`) verbatim.
- **`build_pod_status()`/`write_status()`** gained a `qos:
  crate::eviction::QosClass` parameter, set directly on
  `PodStatus.qos_class`. Computed once per reconcile via the existing
  `qos_class(&pod)` and threaded in from all 4 call sites that build a
  `Pod`-derived status (`reconcile()`, `schedule_retry()`,
  `on_runtime_event()` in `pods.rs`, and the static-pod mirror path in
  `static_pods.rs`) — the exact same "compute from the `Pod` object
  outside `build_pod_status()`, since the event-driven status path
  doesn't have one" pattern round 23 already established for
  `readiness_gates`.
- **Caught and fixed a real pre-existing test that was testing the
  wrong thing** during development: `server_owned_status_fields_do_not_force_a_patch`
  used to set `previous.qos_class` to prove a "server-owned field
  nodelet doesn't touch" doesn't force a spurious patch — but `qosClass`
  *is* now nodelet's own field, so that premise no longer holds. Fixed
  by switching the test to `nominatedNodeName` (a genuinely
  still-server-owned field — the scheduler's, set during preemption,
  which `build_pod_status()` never touches), preserving the test's
  actual intent rather than deleting the coverage.
- No new env vars, no new proto surface — pure logic, an existing
  computation just gets surfaced.
- 3 new unit tests: `eviction_tests/qos_class.rs` (+1: `as_str()`
  matches the real API constants exactly) and
  `pods_tests/build_pod_status.rs` (+1: all 3 QoS classes round-trip
  through `build_pod_status()` correctly, plus the fixed
  server-owned-field test above).
- New e2e test (`deploy/lib/test/cases/lifecycle.sh`):
  `test_pod_status_reports_qos_class` — a real pod with matching
  requests/limits on every resource (genuinely `Guaranteed`) asserts
  `status.qosClass` reads exactly `"Guaranteed"`, not just "present."

**Confidence note**: about as low-risk a round as this project gets —
an existing, already-tested pure computation surfaced into a field
that was previously always empty, with both unit and e2e coverage.
High confidence.

## Round 54: fresh gap re-audit (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 22/27/35/39/45/50). All
4 prior audit lists (rounds 35, 39, 45, 50) were fully closed as of
round 53; user picked another re-audit. No code changed this round —
audit-only.

This pass checked `PodStatus`'s own top-level fields directly against
the API type definition (rather than a specific feature area), plus
CRI's `Version` RPC (like round 50's `Status` RPC check, another
runtime-level CRI call that turned out to have never been called at
all). Found 3 previously-untracked items:

- **`PodStatus.qosClass` is never set** — real kubelet always reports
  this (`BestEffort`/`Burstable`/`Guaranteed`) on every pod status;
  `kubectl describe pod`, dashboards, and various controllers read it
  directly rather than recomputing QoS themselves. Nodelet already
  computes exactly this value internally (`eviction::qos_class()`, used
  for eviction ranking since round 7) — it's just never surfaced into
  `PodStatus` itself. Likely the cheapest of this round's 3 findings:
  no new computation needed, just threading an existing pure function's
  result into `build_pod_status()` the same way round 23 already
  threads `readiness_gates` in from `reconcile()` (which has the `Pod`
  object `build_pod_status()` itself doesn't).
- **`PodStatus.hostIPs` (plural, dual-stack) is never set** — only the
  singular `hostIP` is populated today. Real kubelet sets this
  alongside `hostIP` unconditionally, even on a single-stack node
  (`hostIPs: [{ip: <the same address hostIP has>}]`) — mirrors how
  `podIPs` (plural) already coexists with `podIP` (singular) in this
  same struct, which nodelet already gets right.
- **`ContainerStatus.containerID` is missing its `<runtime>://` scheme
  prefix** — real kubelet always formats this as `<runtimeName>://<id>`
  (e.g. `containerd://1234abcd...`), built from CRI's own `Version`
  RPC's `runtime_name` field; nodelet reports the bare CRI container ID
  with no prefix at all. Confirmed via grep: CRI's `Version` RPC
  (distinct from the `Status` RPC round 53 just wired up) has never
  been called anywhere in this codebase either. Tooling that parses
  this field's scheme to identify the runtime (or that expects the
  standard format at all) could misbehave against nodelet's bare IDs.
  **Not the same finding as round 52's `imageID`** — that field is
  *not* scheme-prefixed by real kubelet either (confirmed by checking
  upstream's own conversion code), so round 52's bare-digest
  implementation was already correct and needs no revisiting.

**Not re-flagging**: everything already tracked in the responsibility
list below, including the still-open low-priority items from rounds 35
(none left) and the `emptyDir.sizeLimit`/`subPath` items noted in
earlier rounds.

## Round 53: `Node.status.runtimeHandlers` (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-52). This closes
round 50's audit list entirely.

Before this round, `Node.status.runtimeHandlers` was never populated —
CRI's runtime-level `Status` RPC (distinct from `ContainerStatus`/
`PodSandboxStatus`, both used elsewhere already) had never been called
anywhere in this codebase.

- **New `PodRuntime::runtime_handlers()`** trait method (default:
  empty, mock has no real handlers to discover) and new
  `RuntimeHandlerInfo` type (`name`, `recursive_read_only_mounts`,
  `user_namespaces` — mirroring CRI's own `RuntimeHandler`/
  `RuntimeHandlerFeatures` messages, already vendored in the proto).
- **`CriRuntime`'s implementation** calls CRI's `Status` RPC once and
  maps the response's `runtime_handlers` through a new pure
  `runtime_handler_from_cri()`.
- **`node.rs`** gained `runtime_handlers_status()` (pure) and threaded
  a new `runtime_handlers: &[RuntimeHandlerInfo]` parameter through
  `build_status()`/`register()`/`push_status()` — the same plumbing
  shape `images`/`mounted_csi_volumes` already established in rounds
  33/34, extended rather than reinvented.
- **`main.rs`** fetches `runtime.runtime_handlers().await` before each
  node registration and each status push, same pattern as the existing
  `node_images()`/`mounted_csi_volumes()` calls right beside it.
- No new env vars.
- 7 new unit tests: `node_tests/build_status.rs` (+4: empty-list,
  name/features round-trip, the empty-string-means-default-handler
  convention, multiple handlers) and `cri_tests/runtime_handler_from_cri.rs`
  (+3: features carried through, missing features default to `false`
  rather than erroring, empty name preserved as the default-handler
  marker).
- New e2e test (`deploy/lib/test/cases/node_status.sh`):
  `test_node_status_reports_runtime_handlers` — checks the real node
  object reports at least one handler (not asserting a specific name,
  since that varies by containerd config); skips cleanly (not fails) if
  the list comes back empty, since whether a given containerd version's
  `Status` RPC actually populates `runtime_handlers` at all is outside
  nodelet's control.

**Confidence note**: the field-mapping logic is pure and thoroughly
unit-tested. The e2e test is genuinely live but its assertion is
necessarily loose (count > 0, not a specific handler) since the exact
contents depend on the containerd version/config this suite happens to
run against — consistent with this project's other "prove the wiring
works, not the exact upstream data" tests (`Node.status.images`, round
33, took the same approach).

## Round 52: `ContainerStatus.imageID` (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-51). Offered the 2
remaining round-50 findings; user picked this one — round 50's own
prediction that it'd be the cheapest of the three, confirmed correct
in practice: no new RPC, no new plumbing, just reading a field that
was already there.

Before this round, `pods.rs`'s 3 `ContainerStatus` construction sites
(app/init/ephemeral containers) all hardcoded `image_id: String::new()`.

- **`ContainerRuntimeStatus` gained `image_id: String`**, populated
  directly from CRI's own `Container.image_ref` (already present on the
  exact lightweight `Container` message `list_pod_containers()` already
  fetches every reconcile — a "digested reference to the image in use,"
  e.g. `docker.io/library/nginx@sha256:...`) at both existing
  construction sites in `runtime/cri.rs` (`build_status()`'s
  app-container loop, `build_labeled_container_statuses()`'s
  init/ephemeral one). No new RPC call needed at all — this data was
  already being fetched and simply never read.
- `pods.rs`'s 3 `ContainerStatus` sites now copy `c.image_id` straight
  through to `imageID` instead of hardcoding empty.
- No new env vars, no new proto surface.
- 2 new unit tests (`pods_tests/build_pod_status.rs`): the field flows
  through correctly, and an empty runtime-reported value is still
  reported as empty rather than fabricated.
- New e2e test (`deploy/lib/test/cases/lifecycle.sh`):
  `test_container_status_reports_a_real_image_id` — asserts a real
  pod's `containerStatuses[0].imageID` is non-empty after reaching
  `Running` (not asserting an exact digest format, which varies by
  registry/runtime).

**Confidence note**: this is about as low-risk a round as this project
gets — a direct field copy from data already fetched every reconcile,
both unit- and e2e-tested. High confidence.

## Round 51: imagePullPolicy enforcement (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-50). Offered the 3
round-50 findings; user picked this one — the highest-value find, and
arguably higher priority than a typical missing-field gap since it cuts
against this project's own edge/offline-capable design goal.

Before this round, `create_and_start_container()` called CRI's
`PullImage` unconditionally for every container, every time, regardless
of `imagePullPolicy` — `Never` would silently pull anyway instead of
refusing, and `IfNotPresent` always contacted the registry even when
the image was already cached.

- **New pure `image_tag(image) -> Option<&str>`**: the mutable tag on
  an image reference, `None` for a bare digest reference (`repo@sha256:...`)
  or an untagged repo name. Only looks at the segment after the last
  `/`, so a registry host:port (`myregistry.io:5000/nginx:1.25`) is
  never mistaken for a tag separator.
- **New pure `effective_pull_policy(policy, image) -> &str`**: an
  explicit `Always`/`IfNotPresent`/`Never` always wins; otherwise real
  kubelet's own default-policy heuristic — `Always` for an untagged or
  `:latest`-tagged image (a floating reference that could have changed
  since it was last pulled), `IfNotPresent` for anything else,
  including a digest reference (immutable by definition, so there's
  nothing to gain from re-checking the registry every time).
- **`create_and_start_container()`** now calls CRI's `ImageStatus` RPC
  (vendored in the proto, never called anywhere in this codebase
  before this round) to check local presence whenever the policy isn't
  `Always` (skipping that extra round-trip entirely when it's going to
  pull unconditionally anyway), then: `Never` without local presence
  now `bail!`s a real, descriptive error instead of pulling — surfacing
  as the same retry-and-report path every other `ensure_pod()` failure
  already uses, no new failure mechanism needed; `IfNotPresent` skips
  `PullImage` entirely when already cached; `Always` keeps its previous
  unconditional-pull behavior (containerd itself still no-ops if the
  digest is already current — the *network round-trip* to check is
  what actually changes here, not the create-time outcome for `Always`
  specifically).
- No new env vars, no new proto surface — `ImageStatusRequest`/
  `ImageStatusResponse` were already vendored, just unused.
- 8 new unit tests (`cri_tests/image_pull_policy.rs`): `image_tag()`'s
  tag/digest/untagged/registry-port-vs-tag-separator cases, and
  `effective_pull_policy()`'s explicit-policy-always-wins,
  unset-defaults-to-`Always`-for-untagged/`:latest`,
  unset-defaults-to-`IfNotPresent`-for-a-specific-tag-or-digest, and an
  unrecognized-string-falls-back-to-the-heuristic defensive case.
  (Caught and fixed a real design bug during development: the first
  implementation treated a digest reference the same as "untagged" —
  defaulting it to `Always` — before a failing test caught that a
  digest is immutable and should behave like a specific tag,
  `IfNotPresent`, instead.)
- New e2e test (`deploy/lib/test/cases/lifecycle.sh`):
  `test_image_pull_policy_never_fails_when_image_is_absent` —
  deliberately uses a *real, valid, pullable* image/tag this suite
  never otherwise references (rather than a fake nonexistent tag,
  which wouldn't distinguish "`Never` correctly refused to pull" from
  "`Never` was ignored but the pull failed anyway because the tag
  doesn't exist upstream either") — if `Never` isn't honored, this pod
  would actually succeed in pulling and reach `Running`, so the test is
  a real, structural proof, not just an "eventually stays Pending"
  check. A companion manual-note documents why proving
  `IfNotPresent`'s "skip the round-trip" behavior specifically (as
  opposed to `Never`'s "refuse entirely," which the automated test
  above already covers live) needs reading nodelet's own logs, which
  this suite doesn't do.

**Confidence note**: both new pure functions are thoroughly unit-tested
(including a design bug the tests themselves caught before merge). The
e2e test is genuine, live, structural proof for `Never`'s refusal
behavior specifically; `IfNotPresent`'s registry-round-trip-skipping
behavior is unit-tested only, not proven live end-to-end (documented as
a manual spot-check instead, consistent with several other CRI-detail
behaviors in this codebase that need log access to observe).

## Round 50: fresh gap re-audit (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 22/27/35/39/45). All 3
prior audit lists (rounds 35, 39, 45) were fully closed as of round 49;
user picked another re-audit. No code changed this round — audit-only.

This pass checked image lifecycle (pull policy enforcement, status
reporting) and CRI's runtime-level `Status` RPC (never called at all so
far — every other round's CRI calls are per-container/per-sandbox/
per-image, not this one). Found 3 previously-untracked items:

- **`imagePullPolicy` is completely unenforced** — by far this round's
  highest-value finding, and arguably higher-priority than a typical
  "unset field" gap because it cuts against this project's own core
  design goal (edge/offline-capable operation, per `README.md`).
  `create_and_start_container()` calls CRI's `PullImage` unconditionally
  for every container, every time, regardless of `container.image_pull_policy`
  — the comment there ("idempotent; containerd no-ops if present")
  is true for the *container creation* outcome, but not for the
  network round-trip itself: `Never` should refuse to pull at all
  (failing the container if the image isn't already cached, not
  silently pulling anyway), and `IfNotPresent` should skip the pull
  entirely when already cached, rather than still contacting the
  registry to check. On a genuinely offline edge device, this could
  mean a pod that should start immediately from a pre-loaded image
  instead blocks or fails on an unreachable registry call it should
  never have made. The real default-policy heuristic when
  `imagePullPolicy` is unset (`Always` for an untagged or `:latest`-
  tagged image, `IfNotPresent` otherwise) also isn't implemented.
- **`ContainerStatus.imageID` is always the empty string** —
  `pods.rs`'s 3 `ContainerStatus` construction sites all hardcode
  `image_id: String::new()`. CRI's own `ContainerStatus.image_ref`
  ("digested reference to the image in use") is already available on
  the exact struct `container_status_details()` (used elsewhere for
  exit-code/reason/termination-message reporting) returns — this is
  purely a "read a field that's already there" fix, not new plumbing.
  Real value: lets a user confirm exactly which image digest is
  actually running, catching `:latest`-tag drift between nodes.
- **`Node.status.runtimeHandlers` unreported** — real kubelet calls
  CRI's runtime-level `Status` RPC once and reports the discovered
  RuntimeClass handlers it returns; nodelet never calls this RPC at
  all (confirmed: zero references to `StatusRequest` anywhere in
  `runtime/cri.rs`, unlike `ContainerStatusRequest`/
  `PodSandboxStatusRequest`, both used elsewhere already). Used by
  newer RuntimeClass-aware tooling to validate a handler actually
  exists on a node before scheduling to it. Moderate value, low
  implementation cost (one new RPC call, no new proto needed — CRI's
  `RuntimeHandler`/`RuntimeHandlerFeatures` messages are already
  vendored in `proto/cri.proto`).

**Not re-flagging**: `terminationMessagePolicy: FallbackToLogsOnError`,
`emptyDir.sizeLimit` (disk-backed), `subPath`/`subPathExpr`, raw-block
`volumeDevices` (checked — only ever copied through for ephemeral
containers, never actually translated into a CRI mount for regular
containers, but this is the *same* pre-existing gap as `hostPath`
volumes: nodelet has no in-tree raw-block-device support at all, not a
new finding this round), and the still-open probe-level/env-var items
from round 35 — all already tracked in the responsibility list below.

## Round 49: local ephemeral storage, slice 2 — eviction signal (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-48). This closes the
local-ephemeral-storage arc rounds 48-49 opened, and with it, round
45's audit list entirely.

Before this round, a pod that filled up its own emptyDir/ConfigMap/
Secret volumes or its container's own writable layer well past any
`ephemeral-storage` limit it declared was never evicted for it —
nothing measured per-pod ephemeral-storage usage at all, and
`eviction.rs` never checked a pod's own limit against anything.

- **`PodUsage` gained `ephemeral_storage_usage_bytes: Option<u64>`**,
  computed as the sum of two sources that together cover everything
  nodelet can see: **new `writable_layer_bytes()`** sums CRI's own
  per-container `ContainerStats.writable_layer.used_bytes` (the piece
  containerd tracks, not nodelet); **new `directory_usage_bytes()`**
  recursively walks nodelet's own materialized volume directory
  (`VOLUME_ROOT/<uid>/volumes` — emptyDir/ConfigMap/Secret/downwardAPI/
  projected content nodelet itself writes, which containerd's stats
  never account for). **Known scope limitation, documented on the
  field itself**: container log file size (`/var/log/pods/...`) isn't
  included — real kubelet's own measurement does include it; nodelet's
  doesn't walk that directory yet.
- **New pure `ephemeral_storage_limit_bytes(pod) -> Option<u64>`**
  (`eviction.rs`): sums every container's own
  `resources.limits["ephemeral-storage"]`, same pattern as
  `qos_class()`'s own per-container resource summation. `None` when no
  container sets one at all — kept distinct from `Some(0)` (an explicit
  zero limit, which any real usage would immediately violate), so the
  caller never confuses "not configured" with "configured to zero."
- **New pure `exceeds_ephemeral_storage_limit(usage, limit) -> bool`**:
  `false` whenever either input is unknown — never guesses a violation
  from missing data.
- **`eviction_loop()`** (`main.rs`) now checks every pod's own
  ephemeral-storage usage against its own limit *first*, independent of
  and ahead of the existing `MemoryPressure`/`DiskPressure`/
  `PIDPressure`-gated path — a direct per-pod limit violation is
  conceptually the same kind of trigger as an individual container
  getting OOM-killed for exceeding its own memory limit, regardless of
  the node's overall memory state, so it doesn't wait for general node
  pressure to be active. The pod-evicting logic itself (`Evicted`
  status patch + zero-grace delete) was pulled into a shared
  `evict_pod()` helper, reused by both this new path and the existing
  pressure-ranked one — no duplicated eviction mechanics.
- Excludes `is_critical()` pods from this new check too, matching this
  project's existing conservative "never touch critical pods" stance
  documented at the top of `eviction.rs` (a deliberate consistency
  choice, not an oversight — real kubelet's own local-storage eviction
  is less consistently protective of priority than this project's
  other eviction paths already are).
- No new env vars, no new proto surface (CRI's `writable_layer` field
  was already vendored in the proto, just unread until now).
- `PodUsage` gained `#[derive(Default)]` (all fields already supported
  it) to keep the ~8 existing test-file literal constructions
  (`stats_tests/`, `prom_metrics_tests/`) working via `..Default::default()`
  rather than touching every one's field list by hand.
- 13 new unit tests: `eviction_tests/ephemeral_storage.rs` (7: no-limit
  vs. explicit-zero-limit, multi-container summation, the `exceeds`
  matrix including both "unknown" cases) and `cri_tests/directory_usage_bytes.rs`
  (4: nonexistent path, flat sum, recursion, empty dir — real
  filesystem I/O against real tempdirs, same style
  `write_volume_dir.rs` already uses).
- New e2e test (`deploy/lib/test/cases/eviction.sh`):
  `test_pod_exceeding_its_own_ephemeral_storage_limit_is_evicted` —
  genuinely automatable (unlike this file's existing node-pressure
  tests, which stay manual-procedure-only since they'd need faking a
  node-wide threshold): a pod with a 1Mi `ephemeral-storage` limit
  writes 8Mi into a plain emptyDir via `dd`, and the test asserts it
  actually gets evicted, with no artificial pressure needed since this
  trigger is independent of node-level pressure entirely.

**Confidence note**: the limit/violation-check logic is pure and
thoroughly unit-tested; `directory_usage_bytes()` is exercised against
a real filesystem. The e2e test is genuine, live, unconditional proof
(the only ephemeral-storage/eviction-related e2e test in this codebase
that doesn't need a manual procedure). The one real caveat is the
documented log-file-size gap in usage measurement — a pod whose
*only* excess usage is in its logs, not its volumes or writable layer,
won't be caught by this yet.

## Round 48: local ephemeral storage, slice 1 — capacity/allocatable (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-47). Local ephemeral
storage was the last open item from round 45's audit; user picked
starting the arc over a fresh re-audit.

**Scope of this slice**: `Node.status.capacity`/`.allocatable`
reporting only. Explicitly **out of scope for this round** (left for a
follow-up slice, matching the resize arc's rounds 42/43 pattern): an
eviction-manager signal for `nodefs`/`imagefs` disk pressure tied to
this resource, and any request/limit enforcement on it.

Before this round, `ephemeral-storage` appeared nowhere in
`Node.status.capacity`/`.allocatable` at all — `capacity_map()` only
ever reported `cpu`/`memory`/`pods`.

- **New `ephemeral_storage_capacity_bytes(cfg) -> u64`**: reuses
  `metrics.rs`'s existing `read_disk_info(&cfg.disk_path)` — the exact
  same `statvfs(2)` call `DiskPressure` already makes — rather than
  adding any new syscall plumbing. Real kubelet's own
  `ephemeral-storage` capacity is likewise just the total size of the
  filesystem backing its root dir. `0` on a read failure (an
  unreadable/misconfigured `disk_path`), matching `read_disk_info()`'s
  own "fail open to unknown, never assume pressure" contract — reported
  as a real `0`, not an omitted field.
- **`capacity_map()`** gained the new `"ephemeral-storage"` key.
  **`allocatable_map()`** needed no code change at all: it only ever
  touches the `"cpu"`/`"memory"` keys explicitly, so `ephemeral-storage`
  (like `pods`) passes through from capacity untouched — correct
  default behavior, since this project has no
  `--system-reserved`/`--kube-reserved`-equivalent knob for this
  resource (documented, not a code gap).
- No new env vars — reuses the existing `NODELET_DISK_PATH`-configured
  `disk_path` that already backs `DiskPressure`.
- 4 new/updated unit tests (`node_tests/capacity_map.rs`): the "exactly
  three keys" test became "exactly four", a new positive-byte-count
  check against the real filesystem (`cfg().disk_path` is `/tmp` in
  tests, which always exists), and a new "unreadable path fails open to
  0" case. `node_tests/build_status.rs`'s two capacity-length
  assertions (no-extra-capacity, device-plugin-resources-added) bumped
  by one each to account for the new key.
- New e2e assertions (`deploy/lib/test/cases/node_status.sh`, extending
  the existing capacity test rather than adding a new one): a real
  positive `status.capacity.ephemeral-storage` byte count, and
  `status.allocatable.ephemeral-storage` equaling capacity exactly (no
  reservation applied).

**Confidence note**: `ephemeral_storage_capacity_bytes()` is pure and
unit-tested against both a real path (this sandbox's own `/tmp`) and a
deliberately-broken one; the e2e assertions are genuine live proof
against whatever `NODELET_DISK_PATH` the real deployment actually uses.
High confidence — this slice is a small, direct reuse of already-
validated machinery (`DiskPressure`'s own `statvfs(2)` read, live since
an earlier round), not new ground.

## Round 47: startup probe failure restart (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-46). Offered the 2
remaining round-45 findings; user picked this one over local ephemeral
storage (the bigger, likely multi-round item).

Before this round, `probe_container()`'s startup-probe loop retried
*forever* until it eventually passed — there was no
`failureThreshold`-triggered restart the way a liveness failure gets
one, even though real kubelet kills and restarts the container
(subject to `restartPolicy`) once a startup probe fails past its own
`failureThreshold`, exactly like a liveness failure.

- **`ProbeTracker.failures`** made `pub` (was private, only `passing`
  was exposed). Needed because a startup probe's tracker starts at
  `passing: false` and — unlike the liveness loop's
  `was_passing && !tracker.passing` edge-detection, which relies on
  starting `true` — never has a "flip to failing" transition to detect
  a second time; the loop needs the raw consecutive-failure count
  directly instead.
- **The startup-probe loop** now checks `tracker.failures >=
  timing.failure_threshold.max(1)` on every non-passing iteration; on
  hitting it, calls `runtime.restart_container()` (threading a probe-
  level `terminationGracePeriodSeconds` override the same way round 44's
  liveness path already does — `probe_grace_period_seconds()` needed no
  changes at all, it was already generic over which probe called it)
  and resets the tracker fresh, then **keeps looping** rather than
  returning. This matters because `probe_container()` is spawned once
  per container for its *entire lifetime*
  (`ensure_probe_supervisor()`), so after a restart-triggering failure
  the same task must keep re-attempting startup probing against the
  newly-recreated container instance, not give up.
- No new env vars, no new proto surface — pure logic change in
  `probes.rs` plus one field visibility change.
- 2 new unit tests: `probes_tests/tracker.rs` (the new public
  `failures` field tracks the consecutive streak correctly, resetting
  on success) and a new integration-style case in
  `probes_tests/supervisor.rs` proving the *whole* `probe_container()`
  loop restarts past threshold, keeps retrying, and recovers once the
  (simulated) recreated container starts passing — not just the pure
  counter in isolation.
- New e2e test (`deploy/lib/test/cases/probes.sh`):
  `test_startup_probe_failure_past_threshold_restarts_the_container` —
  a marker file that's never created (so the startup probe can only
  ever fail) with `failureThreshold: 2`; a nonzero restart count is
  direct, structural proof the new restart path fired, not just
  "eventually reaches some state."

**Confidence note**: the core logic change is small and pure
(`tracker.failures` comparison), and both the integration-style unit
test and the e2e test are genuine, live proof of the whole path
end-to-end — high confidence overall.

## Round 46: CSI ephemeral (inline) volumes (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-45). Offered the 3
round-45 findings; user picked this one — expected to be the cheapest,
since the CSI Node-service plumbing already exists end-to-end for the
PVC path.

Before this round, `volumes[].csi` specified directly (not via a PVC
or the generic `ephemeral` PVC-templated form, round 31) was never
resolved at all — `resolve_volumes()` never branched on `v.csi`, so it
fell through to the generic "volume type not supported yet" warning,
even though `volume_source_type()`'s own diagnostic match arm already
knew the name `"csi"` existed.

- **`CsiDrivers::mount()`/`unmount()`** (`runtime/csi.rs`) gained an
  `ephemeral: bool` parameter. The CSI spec itself says ephemeral
  inline volumes never go through `NodeStageVolume`/`NodeUnstageVolume`
  regardless of what the driver otherwise reports supporting, and have
  no attach/`VolumeAttachment` concept at all — `ephemeral: true` skips
  the staging block entirely rather than relying on the driver's own
  `STAGE_UNSTAGE_VOLUME` capability answer, which is specifically
  *wrong* to consult for this volume kind. The 2 existing call sites
  (PVC, generic ephemeral PVC) pass `false` — unchanged behavior.
- **New pure `csi_ephemeral_volume_handle(pod_uid, volume_name) ->
  String`**: since there's no PV/PVC to derive a real `volume_id` from,
  nodelet mints its own (`"<pod_uid>-<volume_name>"`) — stable across
  reconciles, unique across pod recreations under the same name (keyed
  by UID, not name), unique within one pod (keyed by volume name too).
- **New `resolve_csi_ephemeral_source()`**: checks the driver is
  configured (same `driver_configured()` check the PVC path uses),
  resolves `nodePublishSecretRef` (a `LocalObjectReference`, always in
  the pod's own namespace — converted to the `SecretReference` shape
  `resolve_csi_secret_ref()` already expects rather than duplicating
  that fetch logic), and builds a `CsiVolumeSource` with empty
  `node_stage_secrets`/`publish_context` (neither concept applies to
  the inline form). Wired into `resolve_volumes()`'s existing kind-
  by-kind `else if` chain (`self.csi.mount(&source, &vol_dir, &id.uid,
  true)`) and `unmount_csi_volumes()`'s teardown loop (re-deriving the
  same synthetic handle rather than needing a second side table).
- No new env vars, no new proto surface — reuses the CSI Node-service
  gRPC client and driver-discovery machinery built in rounds 12/13/19
  entirely as-is.
- 3 new unit tests (`cri_tests/csi_ephemeral_volume_handle.rs`): the
  basic combination, distinct volumes in one pod get distinct handles,
  and the same volume name across different pods (different UIDs) gets
  distinct handles too — the actual property the "keyed by UID" design
  choice depends on.
- New e2e test (`deploy/lib/test/cases/csi_pvc.sh`):
  `test_csi_ephemeral_inline_volume_is_mounted` — genuinely automated
  (checks the real host mount directory exists after `RunPodSandbox`),
  gated behind a new `TEST_CSI_INLINE_DRIVER` env var (same pattern as
  the existing PVC test's `TEST_CSI_STORAGE_CLASS`) since it needs a
  real CSI driver registered against this node; skips cleanly without
  one, matching every other CSI test in this file.

**Confidence note**: same tier as the rest of this project's CSI work
(rounds 12/13/19/34) — the driver-agnostic parts (`csi_ephemeral_volume_handle()`,
the `resolve_csi_ephemeral_source()` field-mapping logic) are unit-
tested with solid confidence, but the actual `mount()`/`unmount()` call
against a live driver honoring `ephemeral: true` correctly is
unvalidated against a real CSI driver in this sandbox — same caveat
every prior CSI round has carried.

## Round 45: fresh gap re-audit (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 22/27/35/39). Both prior
audit lists (round 35's, round 39's) were fully closed as of round 44;
user picked another re-audit rather than pausing. No code changed this
round — audit-only.

This pass checked a different set of areas than rounds 22/27/35/39
covered: volume source kinds beyond the already-tracked ones (grepping
`volume_source_type()`'s full match arm list against what
`resolve_volumes()` actually handles), the startup-probe supervisor
loop's failure handling, and local ephemeral-storage's presence (or
absence) anywhere in the resource/eviction story. Also explicitly
re-checked several previously-plausible candidates and confirmed they're
**already implemented**, not gaps: `Node.status.nodeInfo` (architecture/
kernelVersion/osImage/containerRuntimeVersion/kubeletVersion — all
real, `node.rs::system_info()`), `MemoryPressure`/`DiskPressure`/
`PIDPressure` node conditions (all three present, `node.rs`), and
`preStop`'s `sleep` action (`run_lifecycle_hook()` already handles
`exec`/`httpGet`/`sleep`, just not the deprecated `tcpSocket`).

Found 2 previously-untracked items, plus generalized an already-noted
detail (round 44's `ephemeral-storage` `"0"` stub) into its own tracked
gap. Not re-flagging `subPath $(VAR) expansion` (`subPathExpr`) —
already a tracked gap in the Volumes list below, just confirmed still
open, not newly found this round.

- **CSI ephemeral (inline) volumes** (`volumes[].csi` directly, not
  PVC/`ephemeral`-templated) — `volume_source_type()`'s diagnostic
  match arm knows the name `"csi"` exists, but `resolve_volumes()`
  itself never branches on `v.csi` at all; it falls straight through to
  the generic "volume type not supported yet" warning. Distinct from
  the CSI PVC path (round 12/13) and generic ephemeral volumes (round
  31, PVC-templated) — this is the *inline* form real-world drivers
  like `secrets-store-csi-driver` use to mount secrets from
  Vault/AWS Secrets Manager/etc. directly into a pod with no PVC at
  all. **Likely low implementation cost**: the actual CSI Node-service
  plumbing (`NodePublishVolume`, driver discovery, `csi.rs`) already
  exists end-to-end for the PVC path — this would mostly be a new,
  smaller resolution function building a `NodePublishVolumeRequest`
  straight from the volume's own `CSIVolumeSource` fields
  (`driver`/`volumeAttributes`/`nodePublishSecretRef`), skipping the
  PVC/PV lookup entirely. High real-world value relative to effort.
- **Startup probe failure doesn't trigger a restart** — `probe_container()`'s
  startup-probe loop (`probes.rs`) retries *forever* until it passes;
  there's no `failureThreshold`-triggered kill/restart the way a
  liveness failure gets. Real kubelet kills and restarts the container
  (subject to `restartPolicy`) once a startup probe fails past its own
  `failureThreshold`, exactly like a liveness failure — a container
  stuck in a bad boot state today just polls forever instead of ever
  being retried. Directly relevant to round 44's own
  `probe_grace_period_seconds()` work, which was explicitly scoped to
  liveness only because this code path didn't exist yet — this finding
  is that path's actual absence, made concrete.
- **Local ephemeral storage isn't tracked anywhere** — no
  `ephemeral-storage` in `Node.status.capacity`/`.allocatable`, no
  request/limit enforcement, no eviction-manager signal for it at all
  (`eviction.rs`/`node.rs` both have zero references). First surfaced
  implicitly by round 44's `resolve_resource_field_ref()`, which had to
  resolve `limits.ephemeral-storage`/`requests.ephemeral-storage` to a
  documented `"0"` stub rather than a real value — this finding
  generalizes that into its own tracked gap: real kubelet also
  evicts pods under `nodefs`/`imagefs` disk pressure using this same
  resource accounting, tying into this project's own pre-existing
  eviction-manager story (rounds 7, 26, 28) the same way `oom_score_adj`
  (round 28) did.

## Round 44: env resourceFieldRef + probe terminationGracePeriodSeconds (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-43). Round 39's audit
was fully closed after round 43; the only known open items left from
any prior audit were round 35's 2 lowest-priority findings. User picked
both together for one round.

**Env `valueFrom.resourceFieldRef`** — before this round,
`resolve_env_var_source()`'s `resource_field_ref` branch unconditionally
`bail!`ed "not supported yet," a distinct code path from the
already-supported downwardAPI-*volume* form of the same concept.

- **New `node_cpu_millicores: i64` field on `CriRuntime`** (threaded
  through `connect()` from `cfg.cpu_cores`, same pattern as round 28's
  `node_memory_bytes`) — the fallback value for `limits.cpu` when a
  container has no CPU limit set.
- **New pure `resolve_resource_field_ref(reference, resources,
  node_cpu_millicores, node_memory_bytes) -> Result<String>`**:
  `limits.cpu`/`limits.memory` fall back to the *node's own capacity*
  when the container has no such limit — real kubelet's own documented
  Downward API behavior (an unset limit means "the whole node," not
  zero/unbounded). `requests.cpu`/`requests.memory` fall back to the
  container's own limit first (the general request-defaults-to-limit
  rule), then to the node's capacity. `limits.ephemeral-storage`/
  `requests.ephemeral-storage` resolve to `"0"` rather than bailing —
  nodelet doesn't track or enforce ephemeral-storage at all (a
  separate, pre-existing gap), so there's nothing truthful to report.
  An unrecognized `resource` name is still a real error, not silently
  zero.
- **New pure `format_resource_field_value(raw, divisor) -> String`**:
  ceiling-divides the raw value (millicores for CPU, bytes for memory)
  by the divisor (also converted to the same raw unit) and prints a
  plain integer — this single formula reproduces real kubelet's
  well-known "CPU limit env var reports whole cores, rounded up"
  quirk (default CPU divisor is `"1"` = 1000 millicores) *and* the
  common JVM-heap-sizing pattern (`divisor: 1Mi` on a memory
  reference) identically, with no special-casing needed between the
  two resource kinds.
- `resolve_env_var_source()` gained a `container: &Container` parameter
  so it can read the container's own `resources` — the one other call
  site (`resolve_container_env()`) already had it in scope.
- 11 new unit tests (`cri_tests/resource_field_ref.rs`): the rounding
  formula directly, `limits.cpu` with/without a divisor, the
  node-capacity fallback (with and without a container `resources`
  block at all), `requests.cpu`'s limit-then-node-capacity fallback
  chain, the JVM-heap-sizing memory-divisor case, `ephemeral-storage`
  resolving to `"0"`, and an unsupported resource name still erroring.
- New e2e test (`deploy/lib/test/cases/resources.sh`):
  `test_env_resource_field_ref_reports_the_containers_own_limits` —
  `kubectl exec` reads both env vars directly out of the running
  container, proving the whole-cores-rounded-up and the Mi-divisor
  cases live, not just in isolation.

**Probe-level `terminationGracePeriodSeconds`** — before this round,
`restart_container()` (the liveness-probe-triggered kill path) always
used a hardcoded `10` for `StopContainer`'s timeout, honoring neither
the pod's own `terminationGracePeriodSeconds` nor a probe's own
override.

- **`PodRuntime::restart_container`** gained a `grace_period_seconds:
  i64` parameter; `CriRuntime`'s implementation now passes it straight
  through to `StopContainerRequest.timeout` instead of the old
  hardcoded `10`.
- **New pure `probes::probe_grace_period_seconds(probe,
  pod_grace_period_seconds) -> i64`**: the probe's own override if set
  (and non-negative), else the pod's own — matching real kubelet's
  documented rule exactly ("If this value is nil, the pod's
  `terminationGracePeriodSeconds` will be used").
- `pods.rs::ensure_probe_supervisor()` computes the pod's own grace
  period (same default-30 logic `runtime/cri.rs`'s
  `termination_grace_seconds()` already has, duplicated in this
  non-`cri`-gated file rather than exposed across the feature
  boundary — a small, deliberate duplication) and threads it through
  `probes::spawn()` → `probe_container()`, which resolves the
  effective per-probe value right before calling `restart_container()`.
  **Scoped to liveness probes only this round** — the startup-probe
  loop has no failure-threshold-triggered restart at all yet (a
  separate, pre-existing simplification: it just retries forever until
  it eventually passes), so there's no live startup-probe code path
  needing this fix yet either.
- 5 new unit tests: `probes_tests/grace_period.rs` (4 cases for the
  pure function) plus a new integration-style case in
  `probes_tests/supervisor.rs` proving the probe's own override (5)
  wins over the pod's (30) through the *whole* `probe_container()`
  loop, not just the pure function in isolation; the existing liveness
  test also gained an assertion that the pod's own value flows through
  correctly when no override is set.
- New e2e test (`deploy/lib/test/cases/probes.sh`):
  `test_liveness_probes_own_grace_period_overrides_the_pods` — a
  container that traps and ignores `SIGTERM` (so it can only actually
  die via the grace-period `SIGKILL`) with a probe override of 3s vs.
  the pod's 60s; the test's own `wait_until` bound (40s) would time out
  and fail if the pod's grace period leaked through instead of the
  probe's — genuine proof by construction, not just "eventually
  restarts."

No new env vars, no new proto surface — both fixes are pure
logic/plumbing.

**Confidence note**: both new pure functions are thoroughly unit-tested.
Both e2e tests are genuinely live proof — the resourceFieldRef one
reads real env var values out of a running container; the grace-period
one is constructed so a wrong value would cause an observable test
failure (a timeout), not just an unverified assumption.

## Round 43: in-place pod vertical scaling, slice 2 — status reporting (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-42). Offered the
deferred half of round 42's arc alongside round 35's 2 leftovers; user
picked finishing the resize arc's status reporting.

Round 42 built the mechanism (detect + apply a resource change) but
deliberately deferred reporting it back to the API. This round closes
that gap: `containerStatuses[].resources`/`.allocatedResources` and a
`PodResizeInProgress` condition.

- **Two new side tables on `CriRuntime`**, same key (`restart_count_key`)
  and lifecycle as the existing `container_resources` (round 16):
  `applied_resources: HashMap<String, ResourceRequirements>` — the
  container's *original* k8s-native `ResourceRequirements` last
  successfully applied (create, or a successful in-place resize).
  Tracking the k8s-native struct directly (rather than reverse-
  converting `container_resources`'s CRI-form `LinuxContainerResources`
  back into `Quantity` strings) avoids a lossy round trip.
  `spec_resources: HashMap<String, BTreeMap<String, Quantity>>` — the
  current pod spec's own `resources.requests`, refreshed on *every*
  `ensure_container()` call regardless of whether a resize succeeded,
  failed, or wasn't needed. Nodelet has no admission/deferral layer at
  all, so this always just mirrors the live spec rather than some
  separately-gated "accepted" value.
- **`ContainerRuntimeStatus`** gained `resources: Option<ResourceRequirements>`
  and `allocated_resources: Option<BTreeMap<String, Quantity>>`, read
  from the two side tables above in `build_status()`'s app-container
  loop. Scoped to app containers only this round — init/ephemeral
  containers get `None` for both (they don't get a resize decision at
  all yet either, matching round 42's own scope).
- **`pods.rs::build_pod_status()`** copies these straight onto
  `ContainerStatus.resources`/`.allocatedResources`, and computes a new
  `PodResizeInProgress` condition: `True` when any app container's
  actual `resources.requests` doesn't yet match its own
  `allocatedResources` (a resize was requested but hasn't landed),
  `False` otherwise — including when neither is tracked at all (never
  misread absence as "in progress"). Added to `OWNED_CONDITION_TYPES`
  (round 23's foreign-conditions carry-forward set) since nodelet now
  owns writing this condition too.
- **Deliberately not implemented**: `PodResizePending` (reasons
  `Deferred`/`Infeasible`) — nodelet has no admission/node-fitting
  check that could ever cause it to *defer* a resize (it always
  attempts to apply one immediately in the same reconcile the spec
  change was noticed in), so there's no real state for this condition
  to represent. Documented as an intentional non-goal, not an
  oversight.
- No new env vars, no new proto surface.
- 4 new unit tests (`pods_tests/resize_status.rs`): fields copied
  through correctly, matching resources means not-in-progress,
  mismatched resources means in-progress, and the no-data-at-all case
  correctly defaults to not-in-progress rather than misreading absence.
- Extended round 42's existing e2e test
  (`test_in_place_resize_updates_memory_limit_without_restarting`,
  `deploy/lib/test/cases/resources.sh`) rather than writing a new pod:
  after the resize lands, polls `containerStatuses[0].resources.limits.memory`
  until it reflects the new limit, then asserts `PodResizeInProgress`
  reads `False` — genuinely automated, reusing the same pod/patch this
  test already exercises.

**Confidence note**: the condition/field-copying logic is pure and
unit-tested with solid confidence. The e2e extension is genuinely live
proof but only exercises the "resize completes successfully" path —
a resize that fails partway (leaving `PodResizeInProgress` stuck
`True` indefinitely, since nodelet has no separate retry/backoff logic
for a failed `UpdateContainerResources` beyond the next unrelated
reconcile) is unit-tested only, not proven live.

## Round 42: in-place pod vertical scaling, slice 1 (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-41). Offered the
last high-value round-39 item (this one, flagged as likely needing its
own multi-round arc) alongside round 35's 2 leftovers and a fresh
re-audit; user picked starting the resize arc.

Before this round, editing a running pod's CPU/memory did **nothing at
all** — `ensure_container()`'s `AlreadyRunning` branch just returned
`Ok(())` unconditionally, never comparing the live container against
the current pod spec.

**Scope of this slice**: the actual functional mechanism — detecting a
resource change on an already-running container and either applying it
live or restarting the container, per `resizePolicy`. Explicitly
**out of scope for this round** (left for a follow-up): reporting
`containerStatuses[].resources`/`.allocatedResources` back to the API,
and the `PodResizePending`/`PodResizeInProgress` pod conditions. This
mirrors round 39's own framing of the finding as multi-part.

- **New `ResizeDecision` enum + pure `resize_decision(desired, actual,
  resize_policies) -> ResizeDecision`** (`NoChange`/`UpdateInPlace`/
  `RequiresRestart`) — compares only the pod-spec-derived fields
  (`cpu_shares`/`cpu_quota`/`cpu_period`/`memory_limit_in_bytes`),
  deliberately never `cpuset_cpus`/`cpuset_mems` (CPU/Memory Manager
  own those independently — round 16/18 — and a change there must
  never be mistaken for a resize request). A resource's own
  `resizePolicy.restartPolicy` (default `NotRequired` per the API's own
  documented default, when unspecified) decides `UpdateInPlace` vs.
  `RequiresRestart`.
- **Reused, not duplicated, existing state**: rather than adding a new
  side table, this reads `CriRuntime::container_resources` — already
  tracked per container since round 16 for CPU Manager's shared-pool
  `UpdateContainerResources` refresh — as the "last applied resources"
  baseline to diff the freshly-recomputed `linux_resources()` against.
- **`ensure_container()`** restructured: `AlreadyRunning` no longer
  short-circuits unconditionally. `NoChange` still returns immediately
  (identical to before, no regression to the common case);
  `UpdateInPlace` calls the existing `UpdateContainerResources` RPC
  (mutating only the 4 resize-relevant fields on a clone of the
  recorded resources, preserving whatever CPU/Memory Manager cpuset
  assignment is already there) and records the new baseline;
  `RequiresRestart` now funnels into the *same* restart machinery
  `RestartDecision::NeedsRestart` already uses (bump restart count,
  release devices, remove the container, fall through to recreate) —
  no new restart mechanism needed. A resize-required restart is applied
  regardless of `restartPolicy` (it's a distinct, deliberate user
  action, not a crash-restart decision).
- **Known simplification, documented rather than hidden**: nodelet has
  no admission layer (a pre-existing architectural boundary — see the
  CSI/RuntimeClass/sysctls notes above), so it doesn't reject a resize
  that would change a pod's QoS class (e.g. removing a Guaranteed
  container's limit) the way real kubelet's admission validation does;
  it simply applies whatever the (already-accepted-by-the-apiserver)
  pod spec says.
- 8 new unit tests (`cri_tests/resize_decision.rs`): identical-is-no-
  change, cpu/memory changed with no policy (defaults to
  `UpdateInPlace`), explicit `NotRequired`, explicit
  `RestartContainer` for both cpu and memory, an unrelated resource's
  `RestartContainer` policy not forcing a restart, and — the
  correctness-critical one — a `cpuset_cpus` difference alone being
  `NoChange`.
- New e2e test (`deploy/lib/test/cases/resources.sh`):
  `test_in_place_resize_updates_memory_limit_without_restarting` —
  uses `kubectl exec` (the streaming server, see `streaming.sh`) to
  read the container's own live `/sys/fs/cgroup/memory.max` before and
  after `kubectl patch pod ... --subresource resize`, and asserts the
  container's own restart count stayed `0` throughout — genuine live
  proof, not a status-string check. Skips (not fails) if the node uses
  cgroup v1, or if the cluster's kubectl/apiserver doesn't support the
  `resize` subresource at all (needs `InPlacePodVerticalScaling`, GA in
  Kubernetes 1.33 — this project targets recent Kubernetes but the
  exact apiserver version in any given deployment may vary).

**Confidence note**: `resize_decision()` is pure and thoroughly unit-
tested, including the cpuset-non-interference case that was the
trickiest correctness risk in this design. The e2e test is genuinely
live proof (a real container's own cgroup file, read via real
`kubectl exec`+`patch --subresource resize`) but exercises only the
memory/`UpdateInPlace` path — the `RequiresRestart` path and CPU
resizing are unit-tested only, not yet proven end-to-end against a
real cluster.

## Round 41: securityContext.sysctls (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-40). Offered the 3
remaining round-39 candidates (in-place resize, sysctls, round 35's
2 leftovers); user picked `sysctls` — smaller and self-contained,
compared to resize's likely multi-round scope.

Before this round, `spec.securityContext.sysctls` was read nowhere —
CRI's `LinuxPodSandboxConfig` has a dedicated `sysctls` map field
sitting right next to `cgroup_parent`/`overhead`/`security_context`
(all already populated by `sandbox_config()`), completely unread.

- **New pure `pod_sysctls(pod_sc: Option<&PodSecurityContext>) ->
  HashMap<String, String>`** — flattens `spec.securityContext.sysctls`
  (a `Vec<Sysctl{name, value}>`) into the map CRI wants. A later
  duplicate `name` in the list simply overwrites an earlier one in the
  resulting map; the apiserver's own validation already rejects
  duplicate sysctl names within a single Pod, so this never has to
  arbitrate a real conflict itself.
- **`sandbox_config()`** gained a `sysctls: &HashMap<String, String>`
  parameter, set directly on `LinuxPodSandboxConfig.sysctls`.
  `ensure_pod()` computes it once (alongside the existing hostname
  resolution) and threads it through `run_sandbox()`. The two other
  `sandbox_config()` call sites (`PullImageRequest`/
  `CreateContainerRequest`'s own context-only `sandbox_config` field)
  pass an empty map — unaffected, same reasoning as `hostname` in
  round 38 and the namespace options in round 40: only the real
  `RunPodSandbox` call needs the real value.
- No CRI-level enforcement of which sysctls are "namespaced" (safe)
  vs. host-wide (unsafe, real kubelet gates the latter behind a node
  allowlist) — that validation is the apiserver's job upstream
  (`PodSecurityPolicy`/admission), and nodelet has no admission layer
  at all (documented architectural boundary, not new to this round).
  An unsupported/unknown sysctl simply surfaces as a real
  `RunPodSandbox` error from the CRI runtime itself, same as any other
  sandbox-creation failure.
- No new env vars, no new proto surface — pure logic plus one widened
  function signature in `runtime/cri.rs`.
- 5 new unit tests: `cri_tests/pod_sysctls.rs` (3 cases: no security
  context, no sysctls field, translation with 2 entries) and
  `cri_tests/sandbox_config.rs` (+2: pass-through, empty-map default).
- New e2e test (`deploy/lib/test/cases/security.sh`):
  `test_sysctls_are_applied_to_the_sandbox` — sets
  `net.ipv4.ip_unprivileged_port_start` (namespaced, safe without
  `hostNetwork`/`privileged`) and reads its live value back from
  `/proc/sys` inside the container, a real structural proof rather
  than a status-string check; skips (not fails) if the CRI runtime
  doesn't reach `Running` at all, since sysctl namespacing support can
  vary by kernel/runtime version.

**Confidence note**: `pod_sysctls()` is pure and fully unit-tested; the
e2e test is genuinely live proof (a real container's own `/proc/sys`
read), though it only exercises one specific sysctl — the general
mechanism (an arbitrary name/value map reaching CRI unmodified) doesn't
depend on which sysctl was chosen, so this is representative, not a
narrow special case.

## Round 40: hostPID/hostIPC/shareProcessNamespace (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-39). Offered round
39's 4 new findings; user picked bundling `hostPID`/`hostIPC`/
`shareProcessNamespace` together — same `NamespaceOption` struct, same
`PodId`/`sandbox_config()` plumbing, and it includes a real correctness
fix (not just a missing-feature gap).

Before this round, `PodId`/`sandbox_config()` only ever set
`NamespaceOption.network` (plus `userns_options`) — `pid`/`ipc` were
never touched at all, meaning containers silently got containerd's own
CRI-level default for an unset `pid` field: `POD` (every container
shares one PID namespace) — the **opposite** of real Kubernetes' actual
default (`CONTAINER`-scoped, i.e. every container gets its own PID
namespace unless `shareProcessNamespace: true`).

- **New `PodId` fields**: `host_pid: bool`, `host_ipc: bool`,
  `share_process_namespace: bool` — read straight off `pod.spec` in
  `pod_id()`, same pattern as the existing `host_network`/`host_users`
  fields.
- **New pure `pid_namespace_mode(host_pid, share_process_namespace) ->
  NamespaceMode`**: `hostPID` wins outright (`Node`), then
  `shareProcessNamespace` (`Pod`), otherwise `Container` — this is the
  actual fix, always producing an explicit answer instead of leaving the
  field unset.
- **`sandbox_config()`** now always builds the `linux` block (previously
  conditional on `host_network || userns_mapping.is_some()`) and always
  sets `namespace_options.{network,pid,ipc}` explicitly: `network` as
  before; `pid` via the new `pid_namespace_mode()`; `ipc` is `Node` when
  `hostIPC` else `Pod` (IPC has no `CONTAINER`-scope concept in the
  Kubernetes API at all — containers in a pod always share it unless
  `hostIPC` opts into sharing the host's, so there's no unset-vs-set
  ambiguity to fix here the way there was for `pid`).
- **`linux_security_context()`** (the per-container
  `LinuxContainerSecurityContext` builder) gained a `pid_mode:
  NamespaceMode` parameter and now sets the *container's own*
  `namespace_options.pid` too, mirroring real kubelet's own behavior of
  setting this on every container, not just the sandbox — belt-and-
  suspenders against relying on any given CRI runtime's own
  sandbox-inheritance behavior for a field this consequential.
  `network`/`ipc` are deliberately left unset at the container level,
  matching upstream and the CRI proto's own documented rationale (no
  `CONTAINER`-scope concept exists for either).
- Both `run_sandbox()`'s call (sandbox creation) and
  `create_and_start_container()`'s call (every container, app/init/
  ephemeral — it's the single shared creation path per round 24's
  design) now thread the resolved mode through.
- 15 new unit tests: `cri_tests/pid_namespace_mode.rs` (4 precedence
  cases for the pure function), `cri_tests/pod_id.rs` (+2: read-through
  and default-false), `cri_tests/sandbox_config.rs` (+5: default
  container/pod split, `hostPID`, `hostIPC`, `shareProcessNamespace`,
  `hostPID` winning over `shareProcessNamespace` — plus 2 existing
  "linux is none" tests updated since `linux` is no longer ever `None`),
  `cri_tests/linux_security_context.rs` (+2: pid mode carried through,
  container-scoped default).
- 3 new e2e tests (`deploy/lib/test/cases/security.sh`), all genuinely
  automated and structural (not just status-string checks):
  `test_containers_get_isolated_pid_namespaces_by_default` (a second
  container's own shell reports pid 1 — proof it's really its own
  isolated namespace, not shared), `test_share_process_namespace_puts_every_container_in_one_pid_namespace`
  (with `shareProcessNamespace: true`, the second container's shell is
  *not* pid 1, since the first container already holds it in the now-
  shared namespace), `test_host_pid_sees_host_processes` (`hostPID:
  true` sees far more than its own 1-2 processes via `/proc`).
  `hostIPC` stays unit-tested only — no simple, portable shell-level
  IPC-namespace probe available in a minimal Alpine image without
  extra tooling (`ipcs` et al.), a documented scope limitation rather
  than a skipped correctness check.

**Confidence note**: `pid_namespace_mode()` is pure and thoroughly
unit-tested; the 3 e2e tests are genuinely live structural proof (a
real container's own pid, not just a status field). High confidence
overall for `hostPID`/`shareProcessNamespace`; `hostIPC`'s CRI wiring
follows the identical code path as the tested `pid`/`network` fields
but its own live behavior is unvalidated end-to-end (unit-tested only,
per the note above).

## Round 39: fresh gap re-audit (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 22/27/35). Round 35's own
list was down to its 2 lowest-priority items; user picked another
re-audit rather than one of those. No code changed this round —
audit-only.

This pass grepped for a different set of fields/behaviors than rounds
22, 27, or 35 covered: Linux namespace sharing (`hostPID`/`hostIPC`/
`shareProcessNamespace`), `securityContext.sysctls`, and — prompted by
noticing `ensure_container()`'s `AlreadyRunning` branch just returns
`Ok(())` unconditionally, never comparing the container's current
resources against the pod spec's — in-place pod vertical scaling
(the "resize" feature, GA in 1.33). Found 4 previously-untracked items:

- **In-place pod vertical scaling** (`resize` subresource, GA 1.33) —
  by far the highest-value find. `ensure_container()`'s `restart_decision()`
  match only ever looks at CRI *state* (running/exited/missing), never
  at whether the live container's actual resources still match
  `container.resources` in the `Pod` object nodelet was just handed.
  Today, editing a running pod's CPU/memory request or limit does
  **nothing at all** — not even the pre-1.27 "recreate the container"
  fallback behavior kubelet itself used to have. CRI's
  `UpdateContainerResources` RPC (already used elsewhere in this
  codebase, by CPU Manager's shared-pool cpuset refresh) is exactly
  the mechanism a real fix would reuse for `resizePolicy: NoRestart`
  resources; a `RestartRequired` resource would still need the
  create/remove/create path a real restart already takes elsewhere.
  Real kubelet also reports `containerStatuses[].resources` (what's
  actually running) and `.allocatedResources` (what's been admitted)
  separately, and a `PodResizePending`/`PodResizeInProgress` condition —
  none of which nodelet's `pods.rs::build_pod_status()` currently sets.
  A substantial, multi-part feature, not a small polish item.
- **`hostPID`/`hostIPC`** — `PodId`/`sandbox_config()` only ever set
  `NamespaceOption.network` (plus `userns_options`); `pid`/`ipc` are
  never touched at all. CRI's `NamespaceOption` message has dedicated
  `pid`/`ipc` fields sitting right next to `network` — real kubelet
  sets `NamespaceMode::Node` for either when the matching `spec.hostPID`/
  `spec.hostIPC` is `true`. A real, common debugging/monitoring
  workload pattern (host-PID sidecars) is currently silently ignored
  rather than honored or rejected.
- **`shareProcessNamespace`** — closely related to the above but a
  distinct field: `NamespaceMode::Pod` for the PID namespace so every
  container in the pod can see each other's processes (the well-known
  "sidecar that `kill -SIGHUP`s the main container's process" or
  "debug container attaches via `/proc`" pattern). The CRI proto's own
  comment on `NamespaceOption.pid` is worth noting directly: *"the CRI
  default is POD, but the v1.PodSpec default is CONTAINER"* — meaning
  nodelet's current total silence on this field isn't a neutral no-op,
  it's actively relying on whatever containerd's own CRI-level default
  is (POD-shared), which is the **opposite** of real Kubernetes' actual
  default (CONTAINER-scoped, i.e. every container gets its own PID
  namespace unless `shareProcessNamespace: true`). This is a
  correctness bug hiding inside an audit finding, not just a missing
  feature — worth flagging as higher-urgency than a typical "unset
  field" gap.
- **`securityContext.sysctls`** — CRI's `LinuxPodSandboxConfig` has a
  dedicated `sysctls` map field (`map<string, string>`) sitting right
  next to the fields nodelet already populates (`cgroup_parent`,
  `overhead`, `security_context`) — completely unread today. Moderate
  value: a real, if less common, per-pod kernel-tuning mechanism
  (e.g. `net.core.somaxconn` for high-throughput services).

**Not re-flagging**: everything rounds 22/27/35 already found and this
project has since closed, plus the 2 items still open from round 35
(env `resourceFieldRef`, probe-level `terminationGracePeriodSeconds`) —
still real, still open, just lower value than the 4 above.

## Round 38: spec.hostname/subdomain/setHostnameAsFQDN (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-37). Offered the
remaining round-35 candidates; user picked `spec.hostname`/`subdomain`/
`setHostnameAsFQDN` — the highest-value remaining item, a simple,
common override for friendlier in-container hostnames.

Before this round, `sandbox_config()` always set the CRI sandbox's
`hostname` field to the pod's own name, unconditionally, never honoring
an explicit `spec.hostname` override, `spec.subdomain`, or
`setHostnameAsFQDN`.

- **New `resolve_pod_hostname(hostname, subdomain, set_hostname_as_fqdn,
  pod_name, namespace, cluster_domain) -> Result<String>`** (pure) —
  mirrors real kubelet's `GeneratePodHostNameAndDomain` +
  `ShouldSetHostnameAsFQDN`: `spec.hostname` overrides the short
  hostname (default the pod name); `setHostnameAsFQDN` only takes effect
  when `spec.subdomain` is also set (matching upstream — there's no
  domain to form an FQDN from otherwise), producing
  `<hostname>.<subdomain>.<namespace>.svc.<cluster-domain>` as the
  sandbox's actual hostname instead of just the short name. Linux's
  `sethostname(2)` rejects anything over `HOST_NAME_MAX` (64 bytes) —
  this returns `Err` in that case rather than silently truncating,
  matching real kubelet's own hard failure here; `ensure_pod()`'s
  existing error path (log + scheduled retry) handles it with no new
  failure mechanism needed, the same way a repeatedly-failing pod
  behaves upstream too.
- **`sandbox_config()`** gained a `hostname: &str` parameter (used
  verbatim for non-host-network pods; host-network pods still always
  get an empty hostname — unchanged, runc rejects setting one when
  sharing the host UTS namespace). `ensure_pod()` computes the resolved
  hostname once via `resolve_pod_hostname()` and threads it through
  `run_sandbox()` into the real `RunPodSandbox` call. The two other
  `sandbox_config()` call sites (`PullImageRequest`/`CreateContainerRequest`'s
  own `sandbox_config` field, used only as pull/create context, not the
  actual sandbox) keep passing the pod's own name — unaffected by this
  change, since the real sandbox hostname is only meaningful at
  `RunPodSandbox` time.
- No new env vars, no new CRI/proto surface — pure logic plus one
  new/one widened function signature in `runtime/cri.rs`.
- 6 new unit tests (`cri_tests/pod_hostname.rs`): default-to-pod-name,
  explicit override, subdomain-alone-is-a-no-op, setHostnameAsFQDN-
  without-subdomain-is-a-no-op, the full-FQDN case, and the >64-byte
  rejection. `cri_tests/sandbox_config.rs` gained a case confirming
  `sandbox_config()` itself just uses whatever hostname string it's
  given (the override logic lives entirely in `resolve_pod_hostname()`).
- New e2e tests (`deploy/lib/test/cases/dns.sh`):
  `test_spec_hostname_overrides_the_container_hostname` (a container
  running `hostname` reports the overridden name, not the pod name) and
  `test_set_hostname_as_fqdn_reports_the_full_fqdn` (with `subdomain` +
  `setHostnameAsFQDN: true`, `hostname` reports the full
  `<name>.<subdomain>.<namespace>.svc.cluster.local` FQDN) — both
  genuinely automated, no special infrastructure needed.

**Confidence note**: `resolve_pod_hostname()` is pure and thoroughly
unit-tested, and the two e2e tests are genuinely live proof (a real
container's own `hostname` output, not just a status field). High
confidence overall — this is a small, self-contained, well-specified
piece of upstream behavior with no ambiguous edge cases left unhandled.

## Round 37: ConfigMap/Secret live-update (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-36). Offered the
remaining round-35 candidates; user picked ConfigMap/Secret live-update
— the well-known "edit a ConfigMap, the mounted file updates live"
behavior several controllers (cert-manager, external-secrets) actively
rely on for auto-rotation.

Real kubelet's config-map-manager watches every ConfigMap/Secret
referenced by a volume on this node and refreshes the bind-mounted host
file content within seconds of a change — no pod or container restart
needed, since the mount is already a live bind-mount of the host path
nodelet itself materializes content into. Before this round,
`resolve_volumes()` only ever materialized ConfigMap/Secret volume
content once, at pod (re)creation time; the only way to pick up a change
was deleting and recreating the pod.

- **Two new cluster-wide watch streams** (`Api::<ConfigMap>::all`,
  `Api::<Secret>::all`) added as additional `tokio::select!` branches in
  `PodController::run()`, alongside the existing node-scoped Pod watch
  and the runtime event channel. ConfigMaps/Secrets have no
  `spec.nodeName`-equivalent field to scope a watch by, so these are
  necessarily cluster-wide — still purely edge-driven (fires only on a
  genuine object change), not a poll.
- **New `referenced_configmap_names(pod)`/`referenced_secret_names(pod)`**
  (pure functions) — scan `pod.spec.volumes` for direct
  `configMap.name`/`secret.secretName` sources plus `projected` volumes'
  `sources[].configMap.name`/`sources[].secret.name`. Deliberately do
  NOT look at `envFrom`/`valueFrom.configMapKeyRef`/`secretKeyRef` —
  those are captured once at container start and never refreshed by
  real kubelet either, so extending live-update to env vars would be
  incorrect, not just extra scope.
- **`on_referenced_object_changed(namespace, name, kind)`**: on a
  ConfigMap/Secret `Apply`/`InitApply` event, lists this node's pods in
  that namespace (`fieldSelector spec.nodeName=...`, the same pattern
  the pod watch itself uses), filters to the ones whose volumes
  reference the changed object, and calls the controller's existing
  `reconcile(pod)` for each match — reusing the fully-idempotent
  `ensure_pod()` → `resolve_volumes()` re-materialization path that
  every normal reconcile already goes through. No new runtime-side code
  needed: overwriting the host file content in place is sufficient
  because the container's mount is already live.
- No new env vars, no new CRI/proto surface — this is a `pods.rs`-only,
  control-plane-side change.
- 5 new unit tests (`pods_tests/referenced_names.rs`): direct ConfigMap
  volume reference, direct Secret volume reference, both `projected`
  volume source kinds, an env-only reference correctly NOT counted, and
  the no-volumes-at-all empty case.
- New e2e test (`deploy/lib/test/cases/volumes.sh`):
  `test_configmap_volume_updates_live_without_pod_restart` — creates a
  pod with a ConfigMap volume, confirms the initial content, patches the
  ConfigMap via `kctl apply`, then polls the host-materialized file
  directly (same trick every other volume test in this file uses) until
  it reflects the new value, and asserts the app container's own
  `restartCount` stayed at 0 throughout — genuine proof no restart was
  needed, not just that the content eventually changed.

**Confidence note**: the reference-scanning logic and the watch-to-
reconcile wiring are both straightforward and covered by real automated
tests (unit + a genuinely live e2e proof, no special infrastructure
needed). The cluster-wide watch scope (rather than a filtered/indexed
one) is a deliberate, documented simplification — acceptable because
ConfigMap/Secret changes are rare events, not a hot path, and the
existing per-pod `reconcile()` call is already idempotent and cheap to
invoke redundantly.

## Round 36: native sidecar containers (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-35). Offered the 3
implementable round-35 candidates; user picked native sidecar containers
— highest real-world value (the modern GA replacement for manually-
injected service-mesh/logging sidecars).

Real kubelet semantics for `initContainers[].restartPolicy: "Always"`:
runs in order like a regular init container, but doesn't block later
init/app containers on its own *exit* — only on reaching "Started"
(post-startup-probe, or just running if no startup probe) — then keeps
running and restarting for the pod's whole lifetime like a normal
container, its own readiness folding into the pod's overall
`Ready`/`ContainersReady`.

- **New `sidecar_init_decision()`** (pure, mirroring `init_container_decision()`/
  `restart_decision()`'s existing pattern) — `Create` (never created),
  `NeedsRestart` (exited — restart it, but don't block the sequence on
  the restart), `Started` (running, or any other transient state —
  already satisfied, proceed). `ensure_init_containers()`'s loop now
  checks `container.restart_policy == Some("Always")` first and routes
  through this instead of the regular `InitContainerDecision` matrix.
- **`CriRuntime` gained a `sidecar_names: Mutex<HashMap<String, HashSet<String>>>`
  side table** (`sandbox_id -> sidecar init container names`, same
  lifecycle/reason as `pod_uids`/`restart_policies` — event-driven status
  callers have no `Pod` object) — `build_labeled_container_statuses()`
  reads it to set a new `ContainerRuntimeStatus.is_restartable_sidecar`
  flag.
- **`pods.rs::build_pod_status()`**: a sidecar's `initContainerStatuses`
  entry now gets real probe-based readiness (the same `container_ready()`
  closure app containers use), not just "is it running" (what a regular
  init container still gets, matching upstream — it's already done and
  gone by the time anything would check). The pod's `all_ready`
  computation now also requires every sidecar to be ready, alongside app
  containers — a regular init container's readiness still never affects
  it.
- **`ensure_probe_supervisor()`** now includes sidecar init containers in
  the same container list passed to `probes::spawn()` — no changes
  needed to `probes.rs`/`restart_container()` themselves, since neither
  distinguishes init from app containers by anything other than name.
- **`graceful_stop_containers()`** no longer blanket-excludes
  init-labeled containers — a still-*running* init-labeled container can
  only be a sidecar (a regular one blocks progression until it exits, so
  it's never concurrently running at teardown time), and now gets the
  same preStop-hook + graceful `StopContainer` treatment app containers
  get (the preStop lookup now also checks `spec.initContainers`, not just
  `spec.containers`). **Documented simplification**: real kubelet stops
  sidecars strictly *after* every app container has fully stopped; this
  stops everything in one pass instead — not perfectly ordered, but every
  container still gets its own graceful shutdown.
- 9 new unit tests: `cri_tests/sidecar_init_decision.rs`'s 4 decision
  cases, `pods_tests/build_pod_status.rs`'s 5 readiness-folding cases
  (regular init container never affects Ready; a running sidecar with no
  probe supervisor keeps Ready True; a sidecar failing its readiness
  probe flips Ready False; a sidecar whose startup probe hasn't passed
  yet isn't ready even while running; a sidecar passing its probe keeps
  Ready True).

663 tests passing with `--features cri` (up from 654), 223 mock-only (up
from 218 — `pods.rs`'s readiness-folding tests aren't `cri`-gated,
`sidecar_init_decision()`'s are). `deploy/lib/test/cases/lifecycle.sh`
gained two real automated tests: a sidecar that never exits reaches
Running (structural proof it didn't block the app container the way a
regular init container would), and a crash-looping sidecar's own
`restartCount` (not the app container's) climbs above zero.

**Confidence note**: the core decision logic (`sidecar_init_decision()`)
and the readiness-folding logic are both pure and unit-tested with solid
confidence; the two e2e tests are genuinely automated (no special
infrastructure needed, just a real CRI runtime) — real, live proof of
the core mechanics. The one deliberately-scoped-down piece is teardown
ordering (sidecars-stopped-last), documented above rather than silently
thin.

## Round 35: fresh gap re-audit (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 22/27, offered alongside
pause since round 27's list was fully closed as of round 34); user
picked another re-audit. No code changed this round — audit-only.

This pass grepped the codebase against a different set of candidate
fields/behaviors than rounds 22 or 27 covered (pod lifecycle/hostname/env
resolution edges, rather than CRI struct fields or the CLI reference).
Found 5 previously-untracked items:

- **Native sidecar containers** (`initContainers[].restartPolicy: "Always"`,
  GA since 1.29) — `init_container_decision()`/`ensure_init_containers()`
  key every decision off the *pod's* `restartPolicy`, never looking at a
  per-container `restart_policy` on an init container itself (the field
  exists on `k8s_openapi`'s `Container` type — used for regular
  containers too, where it's always unset — but nodelet never reads it
  for init containers specifically). A sidecar-marked init container
  should start before later init/app containers rather than blocking on
  its own exit, and should restart on its own like a normal container for
  the pod's whole lifetime. **High real-world value** — this is the
  modern, GA replacement for the old "hack a sidecar into every pod
  manually" pattern (Istio/Envoy injection, log shippers, etc. increasingly
  rely on it) and is completely unimplemented: every init container is
  treated as one-shot regardless of this field.
- **ConfigMap/Secret live-update** — `resolve_volumes()` materializes
  ConfigMap/Secret volume content exactly once, at pod (re)creation time.
  Real kubelet's config-map-manager watches the referenced objects and
  refreshes mounted volume content within seconds of a change, with no
  pod restart needed (the well-known "edit a ConfigMap, the mounted file
  updates live" behavior most operators expect and many controllers
  — e.g. cert-manager, external-secrets — actively rely on for
  auto-rotation). **High real-world value**, not implemented at all —
  currently the only way to pick up a ConfigMap/Secret change is deleting
  and recreating the pod.
- **`spec.hostname`/`spec.subdomain`/`setHostnameAsFQDN`** —
  `sandbox_config()` always sets the CRI sandbox's `hostname` to the pod's
  own name, never honoring an explicit `spec.hostname` override,
  `spec.subdomain` (combined with hostname for headless-Service DNS), or
  `setHostnameAsFQDN` (whether `hostname -f` inside the container reports
  the full `<hostname>.<subdomain>.<ns>.svc.<cluster-domain>` FQDN instead
  of just the short hostname). Moderate value — a common, simple
  override for friendlier in-container hostnames.
- **`valueFrom.resourceFieldRef` in container env vars** — a distinct
  code path from the already-tracked downwardAPI-volume `resourceFieldRef`
  gap: `resolve_env_value()`'s (env var resolution, not the volume path)
  own `resourceFieldRef` branch explicitly `bail!`s "not supported yet" —
  at least visible rather than silently dropped, but a real gap for any
  container reading its own actual CPU/memory limit via an env var
  (a common init-script pattern, e.g. `-Xmx$(MEM_LIMIT)`-style JVM
  tuning).
- **Probe-level `terminationGracePeriodSeconds` override** (added ~1.25)
  — a `livenessProbe`/`startupProbe` can specify its own grace period for
  the container kill it triggers, distinct from the pod's own
  `terminationGracePeriodSeconds`. `probes.rs` doesn't read or apply
  this at all; a liveness-probe-triggered restart always uses whatever
  the general container-stop path already uses. Lower value — a niche
  override most workloads never set.

All five added to the responsibility list at their appropriate sections.
Ranked roughly by value for the next round to pick from: native sidecar
containers, ConfigMap/Secret live-update, `spec.hostname`/`subdomain`/
`setHostnameAsFQDN`, then the two lower-priority items (env
`resourceFieldRef`, probe-level `terminationGracePeriodSeconds`).

## Round 34: `Node.status.volumesInUse`/`.volumesAttached` (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-33) — but with an extra
step first this time: before writing any code, explicitly surfaced to
the user that for CSI volumes (this project's only real PVC path, since
round 12), the modern external-attacher + `VolumeAttachment` mechanism
(round 19) is what real `kube-controller-manager`'s attach/detach logic
actually relies on today — these two fields mainly matter for the
*legacy* in-tree volume plugin path, which this project has never
supported (`hostPath` is explicitly out of scope everywhere else in this
doc), and getting the exact volume-naming scheme right with no real A/D
controller available to test against risked producing plausible-looking
but wrong data, which is worse than reporting nothing. Asked whether to
implement a best-effort version anyway or treat "document why this is
skipped" as the round's outcome instead; user chose to implement it.

- **`PodRuntime` trait gained `fn mounted_csi_volumes(&self) -> Vec<(String, String)>`**
  (default: empty, same mock-default pattern as `device_plugin_capacity()`)
  — `(driver, volume_handle)` pairs for every CSI volume currently
  mounted by a pod on this node. **`CsiDrivers` already tracked exactly
  this** (`refs`, the per-node-per-pod mount reference-counting round 12
  built for `NodeUnstageVolume` timing) — `CsiDrivers::mounted_volumes()`
  just exposes it, no new tracking needed.
- **`node.rs` gained `csi_unique_volume_name()`** — real kubelet's own
  scheme (`pkg/volume/util`'s `GetUniqueVolumeName`):
  `kubernetes.io/csi/<driver>^<volume_handle>`. `build_status()` now sets
  `volumes_in_use` (the plain string list) and `volumes_attached`
  (`AttachedVolume{name, device_path: ""}` — nodelet only ever
  filesystem-mounts CSI volumes via Stage/Publish, never raw block mode,
  so there's no real device path to report).
- 3 new unit tests (`node_tests/build_status.rs`): empty case, a single
  mounted volume's exact naming scheme, multiple volumes.

654 tests passing with `--features cri` (up from 651), 218 mock-only (up
from 215 — `csi_unique_volume_name()`/`build_status()`'s handling isn't
`cri`-gated, only the real CSI mount-tracking is).
`deploy/lib/test/cases/csi_pvc.sh` extended: its existing (already-
infrastructure-gated) CSI test now also checks for a
`kubernetes.io/csi/` entry in `Node.status.volumesInUse` while the pod's
volume is mounted — a warning, not a hard failure, since the exact
string match is the piece this round is least confident about; a new
manual-note test describes the full live spot-check (confirming the
entry both appears and disappears at the right times, and that a real
A/D controller if present doesn't misbehave because of it).

**Confidence note**: unlike most rounds in this series, this one is
**explicitly, deliberately lower-confidence by design**, not just by
sandbox limitation — `CsiDrivers::mounted_volumes()`'s plumbing reuses
already-tested reference-counting state, and `csi_unique_volume_name()`'s
naming scheme is unit-tested against the documented upstream convention,
but whether a real attach/detach controller (or any other consumer of
these fields) is actually satisfied by nodelet's version has never been
checked against one, and this doc says so plainly rather than implying
otherwise. Scoped to CSI volumes only, matching this project's CSI-first
design throughout.

This closes the last remaining candidate from round 27's fresh gap
re-audit — no specific known gap is currently tracked as of this round.

## Round 33: `Node.status.images` (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-32). Offered the 2
remaining round-27 candidates (both explicitly lowest-priority); user
picked `Node.status.images` over `volumesInUse`/`volumesAttached`.

- **`PodRuntime` trait gained `async fn node_images(&self) -> Result<Vec<NodeImage>>`**
  (default: empty, matching `device_plugin_capacity()`'s existing
  mock-default pattern) — a new `NodeImage { names: Vec<String>,
  size_bytes: u64 }` type in `runtime/mod.rs`, kept independent of the
  generated CRI proto type so the trait itself doesn't need a `cri`
  feature bound.
- **`CriRuntime::node_images()`** calls CRI's `ListImages` (already used
  by `gc_unreferenced_images()`, round 4) and combines each image's
  `repo_tags`/`repo_digests` into the single `names` list
  `Node.status.images[].names` expects, via a new pure
  `node_image_from_cri()`.
- **`node.rs` gained `select_node_images()`** — sorts largest-first and
  caps at `NODE_STATUS_MAX_IMAGES = 50`, matching real kubelet's own
  `--node-status-max-images` default (sizing/ordering is `node.rs`'s job,
  not the runtime's, so `node_images()` itself just reports everything
  it has). `build_status()`/`register()`/`push_status()` all gained an
  `images: Vec<NodeImage>` parameter; both call sites in `main.rs` fetch
  it from the runtime right before calling either.
- 4 new unit tests (`node_tests/build_status.rs`): empty-list case,
  largest-first ordering, names/size round-trip, the 50-image cap.

651 tests passing with `--features cri` (up from 647), 215 mock-only (up
from 211 — `node.rs`'s new logic isn't `cri`-gated, only
`CriRuntime::node_images()`'s real implementation is).
`deploy/lib/test/cases/node_status.sh` gained a **genuinely automated**
test — every other test in this suite that runs a pod already pulls
`$TEST_IMAGE` onto the node, so this just confirms it shows up in
`Node.status.images` with a real nonzero `sizeBytes`, no extra
infrastructure needed.

**Confidence note**: real automated e2e coverage, no manual-note
placeholder — same tier as round 32's image volume source test, a rare
case among the volume/image-adjacent rounds in this effort.

This closes the second-to-last round-27 candidate; only
`volumesInUse`/`volumesAttached` (explicitly the lowest-priority item,
legacy in-tree attach/detach coordination this CSI-first project doesn't
otherwise need) remains untouched from that audit.

## Round 32: image volume source (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-31). Offered the 3
remaining round-27 candidates (all explicitly lower-priority); user
picked image volume source over `Node.status.images` and
`volumesInUse`/`volumesAttached`.

`volumeSource.image` (KEP-4639, beta) turned out to have direct native
CRI support — unlike `emptyDir.medium: Memory` or generic ephemeral
volumes, this isn't "kubelet materializes it, CRI just bind-mounts"; CRI's
`Mount` message has a dedicated `image`/`image_sub_path` field pair
specifically for this, mutually exclusive with `host_path`. Kubelet's own
job is just to `PullImage` the reference (respecting `imagePullSecrets`,
same as any container image) and pass the runtime's resolved
`image_ref` through — the runtime does the actual mounting.

- **`resolve_volumes()`'s return type widened** from
  `HashMap<String, PathBuf>` to `HashMap<String, ResolvedVolume>`, a new
  enum with `HostPath(PathBuf)` (every volume kind before this round) and
  `Image { image_ref: String }` (this round) — kept as a single map
  rather than a second one threaded through 5 function signatures
  separately, since every consumer already just looks a volume name up
  once. `build_mounts()` now matches on the variant: `HostPath` builds
  the existing plain bind mount; `Image` sets `Mount.image` (leaving
  `host_path` empty, per the proto's mutual-exclusivity contract),
  `readonly: true` (image volumes are always read-only, per the KEP),
  and `image_sub_path` from the container's own `volumeMounts[].subPath`
  (the same field regular volumes already use to select a subdirectory —
  for an image volume it selects a path *within* the mounted image
  instead, not a second field on `ImageVolumeSource` itself).
- **`pull_image_volume()`** calls `PullImage` reusing `resolve_pull_auth()`
  (so the pod's own `imagePullSecrets` apply, same as container images),
  and uses the runtime's own resolved `PullImageResponse.image_ref` —
  not the raw `spec.reference` — matching CRI's documented contract.
- The fsGroup-application loop now skips `Image` entries (they're
  read-only OCI content with no host directory of nodelet's own to chown
  — matches upstream, fsGroup doesn't apply to image volumes either).
- `resolve_volumes()` gained a `pull_secrets: &[String]` parameter
  (already computed earlier in `ensure_pod()`, just not previously
  threaded this far).
- 3 new unit tests (`cri_tests/mounts.rs`): image mount sets `Mount.image`
  with an empty `host_path` and `readonly: true`; `subPath` becomes
  `image_sub_path`, not a joined host path; no `subPath` leaves
  `image_sub_path` empty.

647 tests passing with `--features cri` (up from 644), 211 mock-only
(unchanged — this feature is entirely `cri`-gated, same as every other
real-container-materialization feature). `deploy/lib/test/cases/volumes.sh`
gained a **genuinely automated** test — unlike CSI/ephemeral-volume
rounds, this needs no external StorageClass/provisioner/CSI driver at
all, since any pullable OCI image works as the volume reference; reuses
`$TEST_IMAGE` for both the container and the image volume, and proves
both that the mount has real content (`ls -A /img`) and that it's
genuinely read-only (a write attempt must fail) — using this suite's
established file-based reporting trick rather than `kubectl exec` (the
one piece of the streaming server this suite already flags as its least
confident, per round 6's notes), so this test's own confidence isn't
diluted by an unrelated dependency.

**Confidence note**: real automated e2e coverage, no manual-note
placeholder needed — a rare case among the CSI/volume-adjacent rounds in
this parity effort. Only remaining unknown: whether a given CRI runtime's
*version* actually implements `Mount.image` at all (containerd ≥ 2.0 with
the ImageVolume feature) — the test's own skip message calls that out
specifically if it fails.

## Round 31: generic ephemeral volumes (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-30). Offered the 3
remaining round-27 candidates; user picked generic ephemeral volumes —
highest real-world value of the three.

`spec.volumes[].ephemeral` behaves exactly like a normal
`persistentVolumeClaim` reference once its backing PVC exists — the PVC
itself is auto-created (and owned) by the **ephemeral-volume controller**,
a `kube-controller-manager` component, at the deterministic name
`<pod name>-<volume name>`. Same "not kubelet's job" boundary already
established for dynamic provisioning and CSI attach coordination
(rounds 12, 19) — nodelet never creates that PVC, only reads it once the
controller has.

- **New pure helpers**: `ephemeral_pvc_name(pod_name, volume_name)` (the
  documented `<pod name>-<volume name>` convention) and
  `pvc_owned_by_pod(pvc, pod_uid)` — the safety check
  `EphemeralVolumeSource`'s own API doc comment describes: a same-named
  PVC that isn't actually owned by this pod (checked by UID, not just
  name) is never used, even if bound and otherwise valid, to avoid
  adopting an unrelated volume by mistake (e.g. a naming collision or a
  leftover PVC from a previous pod).
- **`resolve_ephemeral_source()`** does the ownership check, then
  delegates to the existing `resolve_csi_source()` for everything past
  it — reusing all of CSI's Node-service mount/driver-resolution/
  secrets-handling machinery from rounds 12/13/19 rather than
  duplicating any of it. Unlike `resolve_csi_source()` (used for a direct
  `persistentVolumeClaim` reference, where a missing PVC usually means a
  real misconfiguration worth surfacing as an error), a missing PVC here
  is the *expected*, normal state immediately after pod creation — the
  controller hasn't gotten to it yet — so this checks existence itself
  and treats "doesn't exist yet" as a graceful retry, not a warning-level
  error.
- **`unmount_csi_volumes()`** (teardown) extended to also recognize
  `.ephemeral` volumes, computing the same deterministic claim name.
- 5 new unit tests: `cri_tests/ephemeral_volume.rs`'s naming-convention
  case and the ownership-check's trusted/no-owner/wrong-uid/
  multiple-owners cases.

644 tests passing with `--features cri` (up from 639), 211 mock-only
(unchanged — this reuses the entirely-`cri`-gated CSI module).
`deploy/lib/test/cases/ephemeral_volumes.sh` added — same infrastructure
dependency `csi_pvc.sh` already has (`TEST_CSI_STORAGE_CLASS`, a working
external-provisioner and CSI driver), *plus* a cluster whose
`kube-controller-manager` actually runs the ephemeral-volume controller;
skips cleanly with a specific reason if the expected PVC never appears at
all (controller not enabled) versus never becomes Bound (provisioner
issue) versus the pod never mounting it (nodelet-side issue) — three
distinct, diagnosable failure points.

**Confidence note**: `ephemeral_pvc_name()`/`pvc_owned_by_pod()` are pure
and unit-tested with solid confidence. Unvalidated live, same as every
CSI-adjacent round since 12: no real CSI driver or ephemeral-volume
controller available in this sandbox to prove the end-to-end flow.

## Round 30: `emptyDir.medium: Memory` (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-29). Offered the 4
remaining round-27 candidates; user picked `emptyDir.medium: Memory` —
highest real-world value of the remaining options.

Real kubelet mounts tmpfs directly on the host path it hands the
container runtime as a bind-mount source for a `Memory`-medium
`emptyDir` — this isn't a CRI-level concept at all (CRI's `Mount` struct
only binds an *existing* host path; it has no filesystem-type control),
matching the same "kubelet materializes it, CRI just bind-mounts what's
already there" pattern this project's ConfigMap/Secret/downwardAPI/
projected volumes (and now the `terminationMessagePath` file, round 24)
already use.

- **New pure helpers**: `is_memory_medium_empty_dir()` (checks
  `.medium == Some("Memory")`) and `tmpfs_mount_args()` (builds
  `mount -t tmpfs [-o size=<bytes>] tmpfs <path>`'s arguments — no
  `-o size=` at all when `sizeLimit` is unset, matching tmpfs's own
  kernel default rather than nodelet inventing a cap upstream doesn't
  impose in that case either).
- **`mount_tmpfs_empty_dir()`** shells out to `mount(8)` — the same
  "use the host's own tools rather than raw syscalls" approach `svc.rs`
  already takes for `nft`. Best-effort: a failure (no root, no tmpfs
  support) is logged and the already-created plain-disk directory is
  used as a fallback rather than failing the whole pod, the same
  graceful-degradation posture used throughout this codebase.
- **`unmount_memory_backed_empty_dirs()`** is new, called from
  `remove_pod()` — a *real* gap this round had to close that plain-disk
  `emptyDir` never needed: a tmpfs mount is actual RAM that must be
  given back on pod teardown, unlike a plain directory (left in place
  today regardless of medium, a separate pre-existing simplification).
  Re-derives volume names/paths from the Pod object rather than tracking
  mount state separately, the same approach `unmount_csi_volumes()`
  already takes.
- 8 new unit tests (`cri_tests/tmpfs_empty_dir.rs`): medium detection
  (including case-sensitivity and the empty-string-vs-unset distinction),
  mount-args construction with/without a size limit, zero/negative size
  limits treated as unset.

639 tests passing with `--features cri` (up from 631), 211 mock-only
(unchanged — this entire feature is `cri`-gated, consistent with every
other real-container-materialization feature in this codebase).
`deploy/lib/test/cases/volumes.sh` gained a real automated test: creates
a pod with a `Memory`-medium `emptyDir`, then checks the host
mountpoint's actual filesystem type via `stat -f -c %T` — genuine proof
of tmpfs, not just that the pod started successfully.

**Confidence note**: the pure medium-detection/mount-arg-construction
logic is unit-tested with solid confidence. The actual `mount`/`umount`
subprocess calls are unvalidated in this sandbox (no root/`CAP_SYS_ADMIN`
available to actually mount tmpfs here) — the e2e test is the real proof
once run against a live cluster with appropriate privileges, same
confidence tier as the cgroup-v2-dependent tests in `resources.sh`.

## Round 29: gRPC probes (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-28). Offered the 3
remaining implementable round-27 candidates; user picked gRPC probes —
highest real-world value of the three (a genuinely common probe type for
cloud-native gRPC services).

- **Vendored `proto/health.proto`** (`grpc/grpc-proto`, Apache-2.0,
  unmodified — no gogoproto to strip, same as `csi.proto`) and wired it
  into `build.rs` alongside the other CRI-adjacent protos, generating a
  real `grpc.health.v1.HealthClient` — the same "vendor + `tonic_prost_build`"
  pattern every other gRPC integration in this codebase already uses,
  rather than hand-rolling raw HTTP/2 framing.
- **`probes.rs` gained `ProbeCheck::Grpc { port, service }`** (parsed
  from `probe.grpc` in `probe_check()`) and `check_grpc()` — dials the
  container's port, calls `Health/Check` with the (optional) service
  name, and treats `ServingStatus::Serving` as passing. **`cri`-gated**
  (the generated client needs `tonic`'s transport, a `cri`-only
  dependency) with a `#[cfg(not(feature = "cri"))]` stub that always
  returns `false` — a mock-only build has no real containers to dial
  anyway, same treatment `exec` probes effectively get without a real
  runtime.
- 5 new unit tests: `probes_tests/check_extraction.rs`'s
  port/service-name parsing (with and without an explicit service name),
  `probes_tests/network_checks.rs`'s failure-path cases (port `0`,
  nothing listening, a real TCP listener that never speaks HTTP/2 gRPC —
  proving this doesn't confuse a bare TCP accept with a real health
  check, unlike `tcp_socket` probing which intentionally does just
  that). **Positive-path (a real `Health/Check` exchange) isn't unit-tested**:
  `tonic`'s server codegen isn't compiled anywhere in this workspace
  (`.build_server(false)` on every vendored proto — nodelet is only ever
  a CSI/device-plugin/CRI *client*), and standing one up just for this
  test would need a workspace-wide feature change; documented rather
  than silently skipped.

631 tests passing with `--features cri` (up from 626), 211 mock-only (up
from 206 — the extraction/failure-path tests aren't `cri`-gated even
though `check_grpc`'s real implementation is). `deploy/lib/test/cases/probes.sh`
gained a manual-note test — this suite's `TEST_IMAGE` (busybox-style)
doesn't speak gRPC at all and no gRPC server image is bundled, so the
live positive-path proof needs a real workload (e.g. etcd, which exposes
the standard health-checking protocol out of the box) — same
"can't automate without real infra" limitation the CSI/device-plugin
rounds have carried since 12.

**Confidence note**: `probe_check()`'s parsing and `check_grpc()`'s
failure paths (timeout, connection refused, non-gRPC listener) are real
and unit-tested with solid confidence — genuine coverage, not simulated.
The one real gap is the success path: no real gRPC health server was
available in this sandbox to prove a passing check actually flips
`Ready`, unlike round 23/24/26/28's fully-automated e2e proofs.

## Round 28: `oom_score_adj` (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-27). Offered the 4
implementable items round 27's audit found; user picked `oom_score_adj`
— highest real-world value, and the one that ties directly into this
project's existing eviction-manager story (rounds 7, 26).

- **`eviction.rs` gained `oom_score_adj(qos, container_memory_request_bytes,
  node_memory_capacity_bytes) -> i64`** — real kubelet's own formula
  (`pkg/kubelet/qos/policy.go`'s `GetContainerOOMScoreAdjust`):
  `Guaranteed` always `-998`, `BestEffort` always `1000`, `Burstable`
  scaled by `1000 - (1000 * request / capacity)` and clamped to `[2, 999]`
  so it never overlaps `Guaranteed`'s protected range or reaches
  `BestEffort`'s certain-death value. A degenerate `capacity <= 0` falls
  back to `999` rather than dividing by zero.
- **`runtime/cri.rs::linux_resources()` gained `qos`/`node_memory_bytes`
  parameters** and now sets `LinuxContainerResources.oom_score_adj` on
  every container. Computed **per container**, using that container's own
  memory *request* (not limit, not the pod's aggregate) — matching
  upstream's own per-container computation exactly, which is why these
  params thread through the existing per-container `linux_resources()`
  call rather than being computed once per pod. `CriRuntime` gained a
  `node_memory_bytes: i64` field (threaded through `connect()`, sourced
  from `cfg.memory_bytes` — the same detected `/proc/meminfo` value
  `Node.status.capacity.memory` already uses, not re-read independently).
- 11 new unit tests: `eviction_tests/oom_score_adj.rs`'s fixed-value/
  scaling/clamping/degenerate-capacity cases, `cri_tests/linux_resources.rs`'s
  wiring cases (Guaranteed/BestEffort get their fixed values; Burstable
  uses the container's own request, not its limit).

626 tests passing with `--features cri` (up from 615), 206 mock-only (up
from 198 — `eviction.rs`'s new function isn't `cri`-gated, `linux_resources()`'s
wiring tests are). `deploy/lib/test/cases/resources.sh` gained two real
automated tests: a BestEffort pod reads its own `/proc/self/oom_score_adj`
and asserts `1000`; a Guaranteed pod asserts `-998` — genuinely
automatable without any cgroup-v2 dependency (unlike this file's other
tests), since `oom_score_adj` is a per-process kernel value readable
regardless of cgroup version.

**Confidence note**: like rounds 23/24/26, this has genuinely **high**
live-validation confidence once run — the e2e tests need no special
infrastructure, cgroup version, or manual spot-check, just a running CRI
runtime.

## Round 27: fresh gap re-audit (2026-07-31, same day)

Explicitly asked again (same pattern as round 22, offered as this
round's option since every round-22 finding closed as of round 26); user
picked another re-audit. No code changed this round — audit-only.

Grepped the codebase against a wider set of candidate kubelet features
than round 22 covered (that pass used the CLI reference doc; this pass
looked at specific PodSpec/CRI fields directly) and found 7 items not
previously tracked anywhere in this doc:

- **gRPC probes** (`probe.grpc`, GA since 1.27) — `probes.rs::probe_check()`
  handles `httpGet`/`tcpSocket`/`exec` but falls through to
  `ProbeCheck::None` for a `grpc` probe (the standard
  `grpc.health.v1.Health/Check` protocol many cloud-native services
  already expose). High real-world value — a genuinely common probe type
  this silently can't check at all.
- **`oom_score_adj`** — real kubelet sets CRI's
  `LinuxContainerResources.oom_score_adj` per QoS class (Guaranteed:
  `-998`, BestEffort: `1000`, Burstable: scaled by memory request) so the
  kernel OOM killer's own choices are QoS-aware. Not set at all —
  `linux_resources()` never touches this field. High value specifically
  *because* this project already has its own eviction-manager story
  (rounds 7, 26): without this, a real kernel OOM kill (which can happen
  faster than `eviction_loop()`'s own check interval reacts) has zero
  signal about which process should die first, undermining the
  QoS-protection guarantee eviction.rs otherwise provides.
- **`emptyDir.medium: Memory`** (tmpfs-backed emptyDir) — `resolve_volumes()`
  only checks `v.empty_dir.is_some()`, ignoring `.medium` entirely; a
  `Memory`-medium `emptyDir` gets materialized on regular disk exactly
  like the default, losing both the performance characteristic and the
  "counts against the pod's memory limit" semantics upstream gives it.
- **Generic ephemeral volumes** (`volumeSource.ephemeral`, GA since 1.23)
  — a volume that auto-creates (and owns the lifecycle of) a
  `PersistentVolumeClaim` from an inline template, then behaves exactly
  like a normal PVC reference. Not implemented at all — `resolve_volumes()`
  has no `ephemeral` branch, so a pod using one gets the generic
  "volume type not supported yet" warning and no mount.
- **`Node.status.images`** — real kubelet reports up to the 50 largest
  cached images (names + sizes), which the scheduler's `ImageLocality`
  scoring plugin uses. Not populated. Lower value for nodelet's
  single-node-per-cluster target — there's no second node for image
  locality to meaningfully differentiate between.
- **`Node.status.volumesInUse`/`.volumesAttached`** — coordination fields
  for the legacy in-tree attach/detach controller path. Not populated.
  Low value: this project's actual PVC story is CSI-first (rounds 12,
  13, 19), and CSI's own attach coordination (round 19) doesn't read
  these fields at all.
- **Image volume source** (`volumeSource.image`, OCI-artifact volumes,
  KEP-4639, still beta) — mounting an OCI image/artifact as a read-only
  volume. Not implemented. Newer/beta feature, lower priority given its
  immaturity upstream too.

All seven added to the responsibility list at their appropriate section
so they show up in the normal ✅/🟡/❌ scan future rounds already do.
Ranked roughly by value for the next round to pick from: `oom_score_adj`,
gRPC probes, `emptyDir.medium: Memory`, generic ephemeral volumes, then
the three lower-priority items (image volume source, `Node.status.images`,
`volumesInUse`/`volumesAttached`).

## Round 26: eviction priority-tiebreaking (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-25). Offered the last
remaining round-22 candidate (eviction priority-tiebreaking) alongside a
fresh gap re-audit and pause; user picked closing out the round-22 list.

Real kubelet's `rankMemoryPressure`/`rankDiskPressureFunc` comparator
chains order eviction candidates by (exceeds-requests, then `Priority`,
then usage) — nodelet's existing eviction ranking (QoS class, then usage)
was missing the priority step entirely. Turned out to be a small, clean
addition: `pod.spec.priority` is *already a resolved numeric value* by
the time nodelet ever sees the Pod object — the apiserver's own Priority
admission controller resolves `priorityClassName` into this field at
admission time — so no `PriorityClass` object lookup was needed at all,
unlike most of this round's siblings which needed real new
infrastructure.

- **`eviction.rs` gained `pod_priority()`** (`pod.spec.priority`,
  defaulting to `0` to match upstream's own default for a pod with no
  priority class) and `eviction_rank()` — the sort key
  `pick_eviction_candidate()`'s `min_by_key` now uses:
  `(qos_class, priority, Reverse(usage))`. QoS class still outranks
  priority (a Burstable pod is never evicted before a BestEffort one no
  matter how low its priority), and priority now outranks usage as the
  tiebreaker within a QoS class (a low-priority pod using little memory
  is evicted before a high-priority pod using a lot, matching upstream's
  own ordering) — a real behavior change from before this round, where
  usage alone broke every tie.
- 6 new unit tests (`eviction_tests/pick_candidate.rs`): lower-priority-
  evicted-first, priority-beats-usage-as-tiebreaker, QoS-class-still-
  outranks-priority, equal-priority-falls-back-to-usage-unchanged,
  unset-priority-defaults-to-zero.

615 tests passing with `--features cri` (up from 610), 198 mock-only (up
from 193 — `eviction.rs` isn't `cri`-gated). `deploy/lib/test/cases/eviction.sh`
gained a manual-note test alongside the existing eviction manual
procedure, describing the live-cluster spot-check for the priority
tiebreak specifically — same "needs artificial pressure, can't safely
automate on a live node" limitation the base eviction procedure already
has (round 7).

This closes every candidate round 22's fresh gap re-audit found. No
specific known gap is currently tracked (the checkpoint API was
explicitly flagged not recommended, and remains so).

## Round 25: user namespaces (`spec.hostUsers: false`) (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-24). Offered the 2
remaining round-22 candidates; user picked user namespaces over eviction
priority-tiebreaking.

- **New `userns.rs`** (`cri`-gated): `UsernsAllocator` — a simple
  fixed-length exclusive-range allocator (base UID/GID + length + max
  slots, all configurable), keyed by pod uid. `allocate()` is idempotent
  (a pod whose sandbox already exists gets the same range back on a later
  reconcile, matching every other per-pod resource claim's pattern in
  this codebase); `None` (pool exhausted) is a graceful-degradation case,
  not a hard failure — the pod still runs, just without a private user
  namespace, same posture CPU/Memory Manager already have for their own
  exhaustion cases. **Simplified vs. upstream's own `usernsManager`**:
  every pod gets the *same* fixed-length range (default 65536, covering a
  container's whole possible UID/GID space) rather than upstream's
  variable-length pool sized to what the pod's image actually needs, and
  allocation is in-memory only (a nodelet restart loses which ranges were
  claimed — self-heals as still-running pods reconcile again, but a
  narrow window exists where a restarted nodelet could theoretically
  double-allocate a range two different still-running pods are both
  using; documented, not hidden, same caveat class as `plugin_registry.rs`'s
  in-memory device/CSI state).
- **`PodId` gained `host_users: bool`** (`spec.hostUsers`, default `true`
  if unset — matching upstream's own default of "no user namespace").
  **`sandbox_config()` gained a `userns_mapping: Option<(u32, u32)>`
  parameter** (kept the function pure/side-effect-free for testability —
  the actual allocation happens in `run_sandbox()`, which is the only
  caller that needs the real range); when `Some`, sets
  `LinuxSandboxSecurityContext.namespace_options.userns_options` to a
  `POD`-mode `UserNamespace` with a single UID and single GID `IDMapping`
  each covering the whole allocated range (container ID 0 → host
  `host_id_base`, length `length`) — the same "remap everything into one
  block" approach upstream itself uses for the common case.
- **`run_sandbox()`** calls `self.userns.allocate(&id.uid)` when
  `!id.host_users`, before building the sandbox config — pod uid, not
  sandbox id, since the sandbox doesn't exist yet at allocation time and
  pod uid is stable across reconciles/retries the way a freshly-generated
  sandbox id wouldn't be.
- **`CriRuntime` gained a `userns: UsernsAllocator` field**, released
  alongside `pod_uids`/`restart_policies` on pod removal and orphaned-
  sandbox GC (looked up via the existing `pod_uids` sandbox-id→pod-uid
  table at those two call sites) — **not** released on the
  stale-sandbox-recreate path, since that's the *same* pod uid getting a
  fresh sandbox moments later and `allocate()`'s idempotency means it'll
  get the identical range back anyway.
- **New config**: `NODELET_USERNS_BASE_UID` (default `100000`, matching
  the conventional `/etc/subuid` starting offset most rootless-container
  tooling already uses), `NODELET_USERNS_LENGTH` (default `65536`),
  `NODELET_USERNS_MAX_PODS` (default `1024`, bounding the allocator, not
  a real OS limit).
- 13 new unit tests: `userns_tests/allocation.rs`'s disjoint-range/
  idempotent-reallocation/exhaustion/release/slot-reuse cases,
  `cri_tests/sandbox_config.rs`'s userns-mapping-forces-a-linux-block/
  correct-`IdMapping`-values/no-mapping-means-no-`userns_options` cases.

610 tests passing with `--features cri` (up from 600), 193 mock-only
(unchanged — `userns.rs` itself is entirely `cri`-gated, matching
`cpu_manager.rs`/`memory_manager.rs`'s precedent: user namespaces are a
CRI-level Linux sandbox concept with no mock-runtime equivalent).
`deploy/lib/test/cases/security.sh` gained a real automated test: a pod
with `hostUsers: false` writes `/proc/self/uid_map` to a shared
`emptyDir`, and the test asserts it does **not** show the host's own full
identity range (`"0 0 4294967295"`) — genuine proof a user namespace is
actually in effect, not just that the field round-tripped through
config. No special infrastructure needed beyond a CRI runtime whose
version actually supports `userns_options` (containerd ≥ 1.7 / runc with
the appropriate build); this suite can't independently verify runtime
version support, so the test's own failure message calls that out as a
specific thing to check if it fails.

**Confidence note**: `UsernsAllocator`'s allocation logic is pure and
unit-tested with solid confidence. Not validated live in this sandbox:
no CRI runtime with real user-namespace support was available to build
this against, so the actual `RunPodSandboxRequest` wire behavior (does
the specific `containerd`/`runc` combination in a real deployment honor
`userns_options` the way this round assumes) is unverified outside the
e2e test's manual-invocation path — same class of caveat every
CSI/device-plugin round has carried since 12.

## Round 24: `terminationMessagePath`/`terminationMessagePolicy` read-back (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-23). Offered the 3
remaining round-22 candidates; user picked this one — the highest-value
of the three.

Investigating this surfaced a bigger pre-existing gap than round 22's
note implied: it's not just the termination *message* that was never
read back — `pods.rs::build_pod_status()` never built a `terminated`
`ContainerState` for regular or init containers **at all**. A container
that had exited (whether genuinely done, or just between crash-loop
restarts) was always reported `Waiting: ContainerCreating`, forever —
only ephemeral/debug containers (round 8) ever got a `terminated` state,
and even that was hardcoded `exitCode: 0` regardless of reality.
Implementing termination-message read-back required building real
terminated-state reporting first, since the message is a field *on* that
state.

- **Real terminated-state reporting** (`ContainerRuntimeStatus` gained
  `exit_code: Option<i32>`, `reason: String`, `finished_at: Option<Timestamp>`,
  `termination_message: String`) — `runtime/cri.rs::build_status()` now
  fetches each **exited** (not just non-running — genuinely `CONTAINER_EXITED`,
  distinct from "created but never started") container's full CRI
  `ContainerStatus` (exit code, reason, finished_at), regardless of
  `restartPolicy` — a deliberate widening from the old "only for
  `restartPolicy: Never`, only once everything's exited" gate, since a
  crash-looping `Always`-restart container's last exit reason is real
  operational value (`kubectl describe` should say *why* it died last),
  not just Job-completion bookkeeping. Still bounded to non-running
  containers only, so a healthy steady-state pod pays zero extra RPCs —
  same low-idle-cost posture this codebase has held throughout. `reason`
  falls back to `"Completed"`/`"Error"` (matching real kubelet) when CRI
  didn't report one (e.g. `"OOMKilled"` when it did).
- **`pods.rs`** gained a shared `container_state()` helper (replacing
  separately-duplicated running/waiting logic in both the app-container
  and init-container builders) that now also produces `terminated` when
  `exit_code.is_some()` — `Some` means "has genuinely exited at least
  once," `None` means "never started," the actual distinction that was
  previously missing entirely.
- **`terminationMessagePath` mount + read-back**: `create_and_start_container()`
  now bind-mounts an empty host file (`termination_message_host_path()`,
  under the same per-pod-UID `VOLUME_ROOT` tree every other host-materialized
  volume already uses) at the container's `terminationMessagePath`
  (default `/dev/termination-log`, matching apiserver defaulting) for App
  and Init containers — the same host-bind-mount approach real kubelet
  itself uses, not a CRI-level concept. `build_status()` reads it back
  (`read_termination_message()`, capped at `MAX_TERMINATION_MESSAGE_BYTES`
  = 4096, keeping the *last* bytes if larger) for every exited container
  and populates `ContainerStatus.state.terminated.message`.
- **`CriRuntime` gained a `pod_uids: Mutex<HashMap<String, String>>`**
  side table (`sandbox_id -> pod uid`, same lifecycle as the existing
  `restart_policies` table) — `status()`'s event-driven path only gets
  namespace+name, no `Pod` object, but needs the pod uid to find a
  container's termination-log host file.
- **Deliberately not implemented, documented not hidden**:
  `FallbackToLogsOnError` (reading the container's log tail when the
  termination-log file is empty and the exit code is nonzero) — nodelet
  always behaves as if policy were `File`. For `FallbackToLogsOnError`
  pods this is a strict subset of correct behavior (message may be empty
  in cases upstream would populate it from logs) but never wrong or
  misleading. Avoided real complexity (CRI log format tail-parsing,
  already used elsewhere for `kubectl logs` but not wired to this path)
  for a policy variant most workloads don't set.
- 12 new unit tests: `pods_tests/build_pod_status.rs`'s `container_state()`
  cases (never-run stays waiting, exited reports terminated with its exit
  code, a currently-running container ignores a stale exit code, message
  population/non-population), `runtime/cri_tests/termination_message.rs`'s
  `read_termination_message()` cases (missing file, small file, oversized
  file truncated to the last `MAX_TERMINATION_MESSAGE_BYTES`, empty file)
  and `termination_message_host_path()`'s path shape.

600 tests passing with `--features cri` (up from 590), 193 mock-only (up
from 188 — `pods.rs`'s `container_state()` tests aren't `cri`-gated;
`termination_message.rs`'s file-reading tests are, hence the different
deltas). `deploy/lib/test/cases/lifecycle.sh` gained two real automated
tests (no infrastructure needed): one creates a `restartPolicy: Never`
pod that exits 3 and asserts `containerStatuses[0].state.terminated.exitCode`
is `3` with a non-empty `reason`; the other has a container write to
`/dev/termination-log` before exiting and asserts the exact string
round-trips into `state.terminated.message`.

**Confidence note**: like round 23, this has genuinely **high**
live-validation confidence once run — both new e2e tests are fully
automated proofs, not manual-note placeholders, since termination
messages and exit-code reporting need no special infrastructure to
exercise for real.

## Round 23: pod `readinessGates` (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-22). Offered the four
implementable items round 22's audit found (checkpoint API excluded as
not recommended); user picked `readinessGates` — highest real-world value
of the four (lets external controllers, e.g. service mesh sidecars, gate
a pod's `Ready` condition on their own conditions).

Implementing this surfaced a real pre-existing correctness issue, not
just a missing feature: `pods.rs::write_status()` sends the whole
`PodStatus` via JSON Merge Patch (RFC 7386), which replaces the
`conditions` array *wholesale* rather than merging it element-by-element
(unlike the apiserver's typed strategic-merge-patch semantics for this
field, which nodelet doesn't use here). `build_pod_status()` previously
built exactly 4 conditions (`Initialized`/`PodScheduled`/`ContainersReady`/
`Ready`) and nothing else — meaning any condition an external controller
had set (which is the entire point of `readinessGates`) would be silently
deleted on nodelet's very next status write, including the one a gate is
trying to read. Fixing this was required for `readinessGates` to
functionally work at all, not optional polish.

- **`pods.rs::build_pod_status()`** gained a `readiness_gates: &[String]`
  parameter (extracted from `pod.spec.readinessGates` by new
  `readiness_gate_types()`, called at every `write_status()` call site —
  `reconcile()`, `schedule_retry()`, `on_runtime_event()`, and
  `static_pods.rs`'s mirror-pod path). `Ready`'s computation becomes
  `all_ready && gates_ready`, where `gates_ready` checks each gate's named
  condition is `True` in `prev`'s conditions (`condition_is_true()`) — a
  gate with no matching condition at all counts as not-satisfied, matching
  upstream. `ContainersReady` is unaffected by gates, computed exactly as
  before.
- **Foreign-condition carry-forward**: any condition in `prev.conditions`
  whose type isn't one of nodelet's own 4 (`OWNED_CONDITION_TYPES`) is now
  copied into the new `conditions` array being written — the actual fix
  for the JSON-Merge-Patch clobbering issue above. This is a real,
  independently-valuable correctness fix beyond `readinessGates` itself:
  *any* other controller setting *any* pod condition on a node nodelet
  manages was previously at risk of having it silently deleted.
- 9 new unit tests (`pods_tests/build_pod_status.rs`): satisfied/
  unsatisfied/missing gate cases, multiple-gates-all-required, foreign-
  condition carry-forward, no-duplicate-owned-conditions, and
  `readiness_gate_types()`'s extraction.

590 tests passing with `--features cri` (up from 581), 188 mock-only (up
from 179 — `pods.rs` isn't `cri`-gated, so this round's tests run in both
builds). `deploy/lib/test/cases/readiness_gates.sh` added — genuinely
automatable without any real infrastructure (the test itself plays the
"external controller" role via `kubectl patch --subresource=status`):
creates a pod with a `readinessGates` entry, confirms `Ready` stays
`False` while the gate condition is unset or explicitly `False` even
though `ContainersReady` is `True`, patches the gate condition to `True`,
confirms `Ready` flips, and confirms the gate condition itself survives
nodelet's subsequent status reconciles (direct proof of the
foreign-condition carry-forward fix, not just that `Ready` happened to
flip once).

**Confidence note**: unlike most rounds in this series, this one has
**high** live-validation confidence — the e2e test doesn't need a real
external controller or any hardware, so it's a genuine end-to-end proof
rather than a manual-note placeholder, once run against a live cluster.

## Round 22: fresh gap re-audit (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-21): every item
explicitly tracked on the responsibility list's checkbox log was closed
as of round 21, so the options offered were a fresh re-audit against
kubernetes.io docs or pausing; user picked the re-audit. No code changed
this round — audit-only, documenting new findings for the user to pick
from next.

Cross-referenced the kubelet CLI reference
(`kubernetes.io/docs/reference/command-line-tools-reference/kubelet/`)
against the current codebase (`grep` for each candidate feature's
expected code path, not just doc-reading) and found five items not
previously tracked anywhere in this doc:

- **`terminationMessagePath`/`terminationMessagePolicy`** — the fields
  are copied through `ephemeral_to_container()`'s struct conversion (a
  plain k8s API type copy, not CRI behavior), but nodelet never actually
  reads the file back out of the container's filesystem after it exits,
  nor honors `FallbackToLogsOnError`. `ContainerStatus.state.terminated.message`
  is always empty. Moderate value — Jobs commonly rely on this for
  `kubectl describe` to surface a short failure reason.
- **Pod `readinessGates`** — not implemented at all; nodelet's readiness
  computation only looks at container readiness probes, never consults
  `status.conditions` for gate-named conditions an external controller
  may have already set.
- **User namespaces** (`spec.hostUsers: false`) — a newer (beta as of
  1.30) isolation feature; not read or translated to CRI's
  `UserNamespace`/`Uids`/`Gids` fields at all.
- **Eviction priority-tiebreaking** — real kubelet breaks ties within a
  QoS class by `PriorityClass` before usage; nodelet already protects
  `system-*-critical` pods outright but doesn't otherwise rank by
  priority, purely by memory usage. A refinement to the existing 🟡
  Eviction entry, not a wholly new capability.
- **Checkpoint API** — a CRIU-based forensic/debugging endpoint (still
  alpha upstream), not implemented. Flagged but **not recommended** —
  CRIU is a real external dependency (kernel + userspace tooling) beyond
  anything else this project needs, for a niche debugging feature with
  low value on nodelet's edge-device target.

All five added to the responsibility list at their appropriate section
(pod lifecycle, security context, node-pressure eviction, kubelet HTTP
server) rather than only in this round's own notes, so they show up in
the normal ✅/🟡/❌ scan future rounds already do.

Everything else checked against the reference (in-place pod resize,
swap, sysctls, checkpointing generally, cert rotation, subPath, seccomp/
AppArmor, pod-level resources) was already tracked from earlier rounds —
no other new gaps found.

## Round 21: device plugin `GetPreferredAllocation`/`PreStartContainer` (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-20). Offered device
plugin polish, a fresh gap re-audit, or pause; user picked device plugin
polish — the last item on the tracked list.

- **`GetPreferredAllocation`**: `DevicePlugins::allocate_preferring()`
  fetches each plugin's `DevicePluginOptions` once (via
  `GetDevicePluginOptions`, right after registering — new `PluginState`
  fields `pre_start_required`/`get_preferred_allocation_available`, set
  by `watch_once()` before its `ListAndWatch` loop starts). When a plugin
  supports it, `allocate_preferring()` offers the full healthy-unallocated
  candidate list and lets the plugin choose; the response is only trusted
  if `is_valid_preferred_allocation()` accepts it — exactly `count` IDs,
  no duplicates, every one currently healthy and unallocated. Anything
  else (missing/malformed response, RPC failure, or a genuine race lost
  against a concurrent allocation in the gap between snapshotting
  candidates and the plugin's response coming back) falls back to
  nodelet's own `pick_devices_preferring()` selection — the plugin never
  gets blind trust for something that could double-allocate a device.
- **`PreStartContainer`**: called with the final device IDs right after
  `Allocate()` succeeds (matching upstream's ordering — reset-before-use
  semantics), for plugins that set `pre_start_required`. A failure here
  releases the devices and fails the whole allocation, the same treatment
  an `Allocate()` failure already gets.
- 7 new unit tests (`device_plugins_tests/preferred_allocation.rs`):
  `is_valid_preferred_allocation()`'s accept case plus every rejection
  case (wrong count, duplicates, unknown ID, unhealthy, already
  allocated, zero-count edge case).

581 tests passing with `--features cri` (up from 574), 179 mock-only
(unchanged — entirely `cri`-gated).
`deploy/lib/test/cases/device_plugins.sh` gained a new manual-note test
describing the spot-check needed on real hardware — reference plugins
like nvidia-device-plugin don't set these options by default, so even
genuine device-plugin hardware access wouldn't automatically exercise
either RPC; a plugin specifically implementing them is required either
way, same "can't be automated in bash without real infra" limitation
every device-plugin-adjacent round has carried since 14.

**Confidence note**: `is_valid_allocation()`'s validation logic is pure
and unit-tested with solid confidence. Not validated live: no real
device plugin implementing either RPC exists in this sandbox (no
hardware at all, as every device-plugin round has noted) — the actual
`GetPreferredAllocation`/`PreStartContainer` gRPC call paths are
unexercised outside compilation and the manual-note's description.

This closes the "device plugins' GetPreferredAllocation/PreStartContainer"
framing that's been the last tracked item since round 14 — the kubelet
parity gap-closure effort has now covered every explicitly-scoped item
identified since the round 1 rescoping.

## Round 20: Topology Manager `restricted` multi-node spread (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-19). Offered Topology
Manager's multi-node `restricted` allowance, device plugin polish, a
fresh gap re-audit, or pause; user picked Topology Manager.

Rounds 17/18 both documented `restricted` as behaving identically to
`single-numa-node` (reject if no single node satisfies every hint
provider together) — a deliberately simpler stand-in for upstream's own
multi-node combination search, which needs each hint provider to
generate genuinely joint multi-node hints (e.g. "these two nodes
together, split") evaluated across provider combinations. This round
gives `restricted` a **real, bounded** relaxation instead of upstream's
exact algorithm: `topology::spread()` — when `align()` finds no single
common node, each hint provider independently gets its own
lowest-numbered eligible node rather than all providers being forced
onto one. `None` (still a hard reject) only when some provider's hint
set is completely empty — a resource that can't be placed anywhere on
the node at all, the one case `restricted` and `single-numa-node` still
agree on. `single-numa-node` itself is unchanged — never falls back to
spread, matching upstream's own strict single-node-only semantics for
that policy.

- **`topology.rs` gained `spread(hints: &[BTreeSet<u32>]) -> Option<Vec<u32>>`**
  — one entry per hint, in the same order passed in, each the lowest node
  in that hint's own set.
- **`runtime/cri.rs::create_and_start_container()`'s Topology Manager
  block restructured**: the single `aligned_numa_node: Option<u32>` used
  uniformly by CPU/Memory/device allocation is now three independent
  preferences — `cpu_preferred_node`, `memory_preferred_node`,
  `device_preferred_nodes: HashMap<String, u32>` (one entry per requested
  extended resource) — populated together (all pointing at the same node)
  when `align()` succeeds, or independently via `spread()` under
  `restricted` when it doesn't. `HintKind` (a small local enum: `Cpu` /
  `Memory` / `Device(String)`) tracks which hint in the `Vec` came from
  which provider so `align()`'s/`spread()`'s node results can be zipped
  back to the right preference variable. `CpuManager::allocate_preferring()`/
  `MemoryManager::allocate_preferring()`/`DevicePlugins::allocate_preferring()`
  call sites now each pass their own resource-specific preferred node
  instead of one shared value — a real correctness fix on top of the new
  capability: previously a `BestEffort`-policy container with, say, a
  memory hint but no CPU hint would've had no preferred node computed at
  all for memory even when one was knowable in isolation; now each
  provider's own preference is used whenever it's known, independent of
  whether the *others* aligned.
- 4 new unit tests (`topology_tests/hints_and_align.rs`):
  `spread()`'s empty-input, independent-per-hint-node,
  lowest-node-preference, and empty-hint-is-infeasible cases.

574 tests passing with `--features cri` (up from 570), 179 mock-only
(unchanged — entirely `cri`-gated).
`deploy/lib/test/cases/topology_manager.sh` extended: a new automated
test proves `restricted` policy doesn't spuriously reject a pod on this
sandbox's single-NUMA-node host either (mirroring the existing
`single-numa-node` non-rejection test — `align()` alone should already
satisfy it there, `spread()` never needs to trigger), plus a new
manual-note test describing the real multi-NUMA hardware spot-check
needed to prove `spread()` actually triggers and places resources
correctly (this sandbox has only one NUMA node, so the interesting
"CPU fits node 0, device/memory only fits node 1" case is inherently
unautomatable here, same limitation every NUMA-adjacent round since 17
has carried).

**Confidence note**: `spread()` itself is pure and unit-tested with solid
confidence. What's not validated live, same as every topology round
before it: this sandbox has exactly one NUMA node, so the actual
`align()`-fails-but-`spread()`-succeeds branch (the entire point of this
round) has never executed against real divergent-NUMA hardware — only
proven correct by direct unit test and by the "still doesn't
spuriously reject on one node" e2e check.

## Round 19: CSI attach coordination (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-18). Offered "CSI
Controller service (attach/detach)", device plugin polish (`GetPreferredAllocation`/
`PreStartContainer`), extending Topology Manager's `restricted` policy to
the multi-node case, or pause; user picked CSI's Controller service — the
one remaining entirely-missing CSI capability.

Re-verified against kubernetes-csi docs before implementing (same
"checked, not assumed" posture as the scope boundary above) and found the
framing in the AskUserQuestion offer was imprecise: **real kubelet never
calls `ControllerPublishVolume`/`ControllerUnpublishVolume` itself** —
those are the **external-attacher** sidecar's job, watching
`VolumeAttachment` objects that **kube-controller-manager**'s
AttachDetachController creates/deletes. This is the same "not kubelet's
job" boundary already established for dynamic provisioning
(external-provisioner) elsewhere in this doc — implementing the Controller
service client itself would have been scope creep into another
component's responsibility, not a nodelet gap.

What **is** kubelet's (and so nodelet's) job on the node side of attach,
and what this round actually implements:
- Check `CSIDriver.spec.attachRequired` for the volume's driver — defaults
  to "attach required" if the object is missing or the field is unset
  (matching upstream's own default).
- If attach isn't required (the common case for node-local/edge storage —
  `csi_pvc.sh`'s existing coverage), behavior is unchanged from round 13.
- If attach **is** required, list `VolumeAttachment` objects and find the
  one matching `(attacher == driver, nodeName == this node,
  source.persistentVolumeName == the bound PV)`. Not yet found, or found
  but `status.attached == false`: `Ok(None)` (same "retry next reconcile"
  treatment every other not-ready-yet PVC/PV state already gets, logged
  with which of the two it is). Found and attached: thread
  `status.attachmentMetadata` through as `publish_context` on both
  `NodeStageVolume` and `NodePublishVolume` (previously hardcoded to
  empty) — some drivers require this (e.g. a device path chosen at attach
  time) for Stage/Publish to succeed at all.
- **`CsiVolumeSource` gained a `publish_context: HashMap<String, String>`
  field**; `resolve_csi_source()` in `runtime/cri.rs` computes it via two
  new pure helpers (`attach_required()`, `find_volume_attachment()`,
  `attachment_publish_context()`) plus a `driver_requires_attach()`
  cluster lookup.
- **`CriRuntime` gained a `node_name` field** (threaded through
  `connect()`) — needed to match `VolumeAttachment.spec.nodeName` against
  this node; nothing in `resolve_csi_source()` previously needed to know
  the node's own name.
- `runtime/csi.rs`'s module doc comment corrected: it previously implied
  the Controller service was "out of scope, not kubelet's job" without
  distinguishing *calling* it (still correctly out of scope) from
  *consuming what it produces* (the actual gap, now closed).

11 new unit tests (`runtime/cri_tests/csi_attach.rs`): `attach_required()`'s
missing-object/unset-field/explicit-true/explicit-false cases,
`find_volume_attachment()`'s match/no-match/empty-list cases,
`attachment_publish_context()`'s no-status/not-attached/attached-empty/
attached-with-metadata cases. 570 tests passing with `--features cri` (up
from 559), 179 mock-only (unchanged — entirely `cri`-gated).
`deploy/lib/test/cases/csi_attach.sh` added — provisions a PVC against an
attach-requiring StorageClass, waits for the pod to reach Running, then
asserts the matching `VolumeAttachment.status.attached` is `true` (proof
the pod only started because nodelet actually found and waited on it, not
a coincidence of timing). Needs `TEST_CSI_ATTACH_STORAGE_CLASS` — a real
attach-requiring driver with a working external-attacher, same class of
infra dependency `csi_pvc.sh` already has for provisioning.

**Confidence note**: `attach_required()`/`find_volume_attachment()`/
`attachment_publish_context()` are pure and unit-tested against
hand-built `CSIDriver`/`VolumeAttachment` objects — solid confidence
there. Not validated live: no real attach-requiring CSI driver or
external-attacher exists in this sandbox (edge/single-node target, no
cloud block storage), so the actual `driver_requires_attach()` cluster
lookup and the end-to-end wait-then-mount flow are unexercised outside
the e2e test's manual-invocation path, same caveat every CSI-adjacent
round since 12 has carried.

## Round 18: Memory Manager (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-17). Offered Memory
Manager, extending Topology Manager's `restricted` policy to the
multi-node case, CSI's Controller service, or device plugin polish; user
picked Memory Manager — the last of the three upstream managers, and it
reuses round 17's NUMA-topology groundwork directly rather than opening
new subsystem work, closing out the CPU/Memory/Topology manager thread
entirely.

- **New `memory_manager.rs`** (`cri`-gated): NUMA node pinning
  (`cpuset_mems`) for Guaranteed-QoS containers with a memory limit set —
  `wants_pinned_memory()` mirrors `cpu_manager.rs::wants_exclusive_cpus()`'s
  eligibility check, minus the "whole number" requirement (memory has no
  integer analog; any positive limit qualifies). `allocate()`/
  `allocate_preferring()` pick a single NUMA node with enough free
  capacity, trying a Topology Manager-preferred node first.
- **Three real simplifications, documented in the module's own doc
  comment rather than silently thin**: (1) **never spans multiple NUMA
  nodes** — real Memory Manager can split a container's memory across
  nodes if no single one has room; this always picks one node or falls
  back to unconstrained, the same graceful-degradation choice CPU Manager
  already makes; (2) **no shared-pool tracking for non-pinned
  containers** — CPU Manager explicitly sets every container's
  `cpuset_cpus` and retroactively updates already-running ones
  (`refresh_shared_pool_cpusets()`, round 16); Memory Manager only ever
  touches `cpuset_mems` on containers it actually pins, leaving everything
  else unconstrained (memory-safe, just less strictly isolated); (3) **no
  per-NUMA-node `--reserved-memory`-equivalent** — only total node
  capacity is tracked, not a separate system/kube reservation carved out
  of a specific node (system/kube-reserved memory is still subtracted
  from `Node.status.allocatable` overall, just not pinned away from any
  particular NUMA node).
- **`topology.rs` gained `read_numa_memory()`/`memory_hint()`** — reads
  each NUMA node's `MemTotal` from `/sys/devices/system/node/node*/meminfo`
  (validated with a real unit test against this sandbox's own host, same
  confidence level round 17's CPU topology read already had), and Memory
  Manager is now a third hint provider alongside CPU Manager and device
  plugins in `create_and_start_container()`'s alignment computation. The
  module's "No Memory Manager exists" caveat from round 17 is gone.
- **Wired into `runtime/cri.rs::create_and_start_container()`** right
  alongside the existing CPU Manager block — computes `wants_pinned_memory`
  early (used both for the Topology Manager hint and the actual
  allocation), sets `resources.cpuset_mems` on success, releases on
  `CreateContainer` failure (mirrors the CPU Manager rollback) and via the
  same `release_container_devices()`/`release_sandbox_devices()` helpers
  every other per-container resource already uses.
- 26 new unit tests: `memory_manager.rs`'s `wants_pinned_memory` and a
  full allocate/release/disjointness/preference suite mirroring
  `cpu_manager_tests`'s structure, plus `topology.rs`'s
  `read_numa_memory`/`memory_hint`.

559 tests passing with `--features cri` (up from 533), 179 mock-only
(unchanged — entirely `cri`-gated).
`deploy/lib/test/cases/memory_manager.sh` added — creates a Guaranteed
pod with a memory limit and checks its `cpuset.mems` cgroup file (found
by container ID, same technique `cpu_manager.sh` already uses) is
non-empty. Needs `TEST_MEMORY_MANAGER_STATIC=true`.

**Confidence note, same posture as round 17**: `read_numa_memory()` is
validated against this sandbox's real `/sys/devices/system/node`
directly. What's not validated live: genuine multi-container NUMA
capacity contention (this sandbox's single NUMA node means every
allocation trivially succeeds) and the "no single node has enough room,
falls back to unconstrained" path actually triggering for real. All
failure modes are logged warnings with the container left unpinned,
never a crash.

## Round 17: Topology Manager (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-16). Offered Topology
Manager, Memory Manager, CSI's Controller service, or device plugin
polish; user picked Topology Manager — it ties together CPU Manager
(rounds 15-16) and device plugins (round 14) rather than opening a new
subsystem, and Memory Manager would need the same NUMA-topology
groundwork this round already builds, so doing Topology Manager first
avoids duplicating that work.

- **New `topology.rs`** (`cri`-gated): reads real NUMA topology from
  `/sys/devices/system/node/node*/cpulist` (`read_numa_topology()`,
  parsed once at startup — NUMA layout doesn't change at runtime on any
  hardware nodelet targets), and implements a **single-node-only**
  hint/alignment algorithm — deliberately simpler than upstream's full
  per-provider-bitmask/cross-combination search, documented up front in
  the module doc comment. `cpu_hint()`/`device_hint()` compute which
  individual NUMA nodes can alone satisfy a CPU or device request;
  `align()` intersects every hint provider's candidate set and returns the
  lowest-numbered common node, or `None` if none exists. Reaches the same
  answer as upstream whenever a single node can satisfy everything (the
  common case, and the only case `single-numa-node` policy upstream ever
  accepts either) — the gap is upstream's `restricted` policy also
  accepting valid *multi*-node alignments, which this doesn't search for;
  `restricted` is treated identically to `single-numa-node` here.
- **`DeviceInfo` gained `numa_node: Option<u32>`** — parsed from the
  device plugin's `TopologyInfo.nodes[0]` during `ListAndWatch` (round 14
  only tracked `id`/`healthy`, dropping topology entirely). `None` (no
  `TopologyInfo` reported) is treated as "compatible with every NUMA
  node," not "compatible with none," matching upstream's own device
  manager hint generation.
- **`CpuManager::allocate_preferring()`/`DevicePlugins::allocate_preferring()`**
  — both gained a NUMA-preference parameter: try the aligned node's own
  CPUs/devices first, falling back to the rest of the pool if the
  preferred node alone can't supply the full count. `allocate()` on both
  is now a thin wrapper calling these with `None` (Topology Manager
  disabled — identical behavior to before this round).
- **Wired into `runtime/cri.rs::create_and_start_container()`**: before
  the existing CPU/device allocation logic runs, computes every hint
  provider's candidate set for *this* container (its exclusive-CPU want,
  if any; each device-plugin resource it requests), intersects them via
  `align()`, and applies the configured policy — `None` skips the whole
  computation (a true no-op, exactly pre-round-17 behavior); `BestEffort`
  logs and proceeds unaligned on failure; `Restricted`/`SingleNumaNode`
  return an error that propagates up through `ensure_container()`/
  `ensure_pod()`, leaving the pod Pending rather than starting it
  misaligned — nodelet's version of upstream's Topology Manager admission
  rejection.
- 25 new unit tests for `topology.rs` (`parse_cpulist`, `read_numa_topology`
  — including a real read against `/sys/devices/system/node` on this
  host, `cpu_hint`/`device_hint`/`align`), plus 4 new `CpuManager` tests
  and 4 new `pick_devices_preferring` tests for the NUMA-preference paths.

533 tests passing with `--features cri` (up from 500), 179 mock-only
(unchanged — entirely `cri`-gated).
`deploy/lib/test/cases/topology_manager.sh` added: an automated check
that `single-numa-node` policy never spuriously rejects a pod on a
single-NUMA-node host (this sandbox's own host has exactly one NUMA node
with cpus 0-3, confirmed via the real-topology unit test above), plus a
manual-note for genuine cross-provider alignment verification (needs
real multi-socket hardware or a NUMA-aware device plugin, neither
available here).

**Confidence note**: the NUMA-topology *reading* (`read_numa_topology()`)
is validated against this sandbox's real `/sys/devices/system/node`
directly in a unit test — genuine confidence there, unlike the usual
"no live X in this sandbox" caveat other rounds have carried. What's
*not* validated live is genuine multi-NUMA cross-provider alignment
(CPU + device on the same node) and the `Restricted`/`SingleNumaNode`
rejection path actually firing for a real unsatisfiable request — this
sandbox has only one NUMA node, so every request trivially aligns and
rejection never triggers here. Same failure-safe posture as always:
`None` policy (the default) never touches any of this new code.

## Round 16: CPU Manager retroactive shared-pool updates (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-15). Offered closing
round 15's biggest flagged gap, Topology Manager, Memory Manager, or
smaller polish items (CSI Controller service, device plugin
`GetPreferredAllocation`/`PreStartContainer`); user picked closing the
round-15 gap — it's what makes CPU Manager's isolation guarantee actually
hold in the common case (pods arriving in mixed order) rather than only
for containers created after an exclusive claim.

- **`runtime/cri.rs::refresh_shared_pool_cpusets()`** — after every
  exclusive CPU claim or release, sweeps every currently-tracked,
  non-exclusively-pinned container and calls CRI's
  `UpdateContainerResources` to bring its `cpuset_cpus` in line with the
  now-current shared pool. Diffs against last-known state first (skips a
  container whose cpuset already matches), and skips anything
  `cpu_manager.is_exclusive()` reports as pinned — those keep their own
  dedicated cores untouched.
- **New `container_resources` side table** (`"sandbox_id/container_name"
  -> (container_id, last-applied LinuxContainerResources)`) — needed
  because CRI's `ListContainers` doesn't expose a container's
  currently-applied resources in any structured, cross-runtime way, so
  `UpdateContainerResources` calls would otherwise risk clobbering a
  container's CPU shares/quota/memory limit while only meaning to change
  its cpuset. Recorded once a container's `StartContainer` succeeds,
  removed when it's torn down.
- **`CpuManager::is_exclusive()`** — new query the refresh sweep uses to
  tell "this container is intentionally pinned, leave it alone" apart from
  "this container should track the shared pool."
- **`release_container_devices()`/`release_sandbox_devices()` (round 14)
  are now `async`** — they already released CPU Manager claims (round 15);
  they now also trigger `refresh_shared_pool_cpusets()` afterward so a
  released claim's cores actually become available to whatever's already
  running, not just theoretically free for the next container created.
- 3 new unit tests for `CpuManager::is_exclusive()`. The
  `UpdateContainerResources` call itself isn't independently unit-tested
  (same treatment every other real CRI RPC call in this file gets — only
  the pure decision logic is unit-testable without a live socket); the new
  e2e test below is what actually proves this works end to end.

500 tests passing with `--features cri` (up from 497), 179 mock-only
(unchanged — entirely `cri`-gated).
`deploy/lib/test/cases/cpu_manager.sh` gained a second test:
`test_cpu_manager_retroactively_shrinks_an_already_running_shared_pool_container`
creates a BestEffort pod first, records its `cpuset.cpus`, then creates a
Guaranteed 1-CPU pod and asserts the BestEffort pod's cpuset actually
*changed* afterward — the one thing round 15's test couldn't prove (its
two pods were both created after the policy was already settled, so it
only showed disjoint exclusive assignment, not retroactivity).

**Still not implemented, unchanged from round 15**: topology/socket-aware
CPU selection (still simple ascending-ID) and Topology Manager itself.

## Round 15: CPU Manager (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-14). Offered CPU/Memory/
Topology managers, CSI's Controller service, and device plugins' remaining
gaps; user picked the managers category as "the last major unaddressed
category," and within it, CPU Manager was chosen as the most tractable and
valuable specific piece — CPU pinning matters for edge/latency-sensitive
workloads, Memory Manager is the least commonly used of the three upstream,
and Topology Manager only has something to coordinate once CPU Manager
(and ideally Memory Manager) exist to coordinate between.

- **New `cpu_manager.rs`** (`cri`-gated): implements real kubelet's
  `--cpu-manager-policy=static` — a Guaranteed-QoS container requesting a
  whole number of CPUs (`cpu` request == limit == an integer, the same
  rule upstream uses) gets exclusive cores via CRI's `cpuset_cpus`, picked
  ascending from the current shared pool (total cores minus reserved
  minus already-exclusively-claimed). Opt-in: `NODELET_CPU_MANAGER_POLICY`
  defaults to `none` (upstream's own default) — `cpu_manager` is `None` on
  `CriRuntime` in that case, and every container's `cpuset_cpus` is left
  unset exactly as before this round, a true no-op.
- **Reserved CPUs derived from existing config** — `reserved_cpu_count()`
  reuses `system_reserved_cpu_millicores + kube_reserved_cpu_millicores`
  (round 11), rounded up to a whole core, rather than adding a third
  reservation config just for this. No `--reserved-cpus`-equivalent flag.
- **Every container now gets an explicit `cpuset_cpus`, not just exclusive
  ones** — when the policy is enabled, a container that *doesn't* qualify
  for exclusive cores gets the current shared pool set explicitly (instead
  of an empty/unconstrained cpuset), so newly-created shared-pool
  containers correctly exclude whatever's already exclusively claimed.
- **Deliberately scoped as a first slice, documented in the module's own
  doc comment**: real kubelet's static policy is *bidirectional* — when a
  new exclusive claim is made, every already-running shared-pool
  container gets retroactively shrunk via `UpdateContainerResources`, and
  grown back on release. This round only sets `cpuset_cpus` at
  container-*creation* time; a shared-pool container that was already
  running before a later exclusive claim keeps its original (wider)
  cpuset. Still delivers the core guarantee (an exclusive container's
  cores are never *newly* handed to something else after the fact), just
  not full retroactive isolation from whatever was already running.
  Retroactive `UpdateContainerResources` sweeping, topology/socket-aware
  core selection, and Topology/Memory Manager are explicitly left for a
  future round.
- **New `device_allocations`-shaped side table folded into the existing
  release helpers** — `release_container_devices()`/`release_sandbox_devices()`
  (round 14) now also release any CPU Manager exclusive claim for the
  same key, so one release call at each restart/retry/GC/removal site
  handles both device-plugin and CPU-pinning cleanup instead of needing a
  second parallel set of call sites.
- 26 new unit tests: `wants_exclusive_cpus` (the QoS/whole-CPU eligibility
  rule), `format_cpuset`/`reserved_cpu_count` (cgroup-string rendering and
  reservation rounding), and a full `CpuManager` allocation/release/
  disjointness/reserved-exclusion suite.

497 tests passing with `--features cri` (up from 477), 179 mock-only
(unchanged — this round is entirely `cri`-gated).
`deploy/lib/test/cases/cpu_manager.sh` added — unlike device plugins, this
one *is* fully automatable without special hardware: it creates two
Guaranteed 1-CPU pods and asserts their `cpuset.cpus` cgroup files (found
by container ID under the cgroup tree, tolerant of driver naming) are
non-empty and disjoint. Needs `TEST_CPU_MANAGER_STATIC=true` telling the
suite the running nodelet actually has the policy enabled (same opt-in-
config pattern `static_pods.sh`/`log_rotation.sh` already use).

## Round 14: device plugins (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-13). Offered device
plugins, the CPU/Memory/Topology managers, and CSI's Controller service
(attach/detach); user picked device plugins, reasoning that it reuses
round 13's plugin-registration infrastructure directly (the registration
client already had to *reject* `DevicePlugin` registrations before this
round — now it accepts and routes them) rather than opening entirely new
surface, and has real value for edge devices with attached accelerators.

- **New `device_plugins.rs`** (`cri`-gated): the kubelet-side client for
  the Device Plugin API (`k8s.io/kubelet/pkg/apis/deviceplugin/v1beta1`),
  vendoring a cleaned `proto/deviceplugin.proto` (gogoproto import/options
  and per-field `[(gogoproto.customname)]` annotations stripped — third
  proto this project has needed that treatment for, after `cri.proto` and
  `pluginregistration.proto`). Three responsibilities: **inventory**
  (holds each registered plugin's `ListAndWatch` stream open for the life
  of its registration, tracking device health as it changes — a
  reconnect loop, same 5s-retry shape as other background loops in this
  codebase, handles a plugin process restarting); **capacity
  advertisement** (`capacity_map()` — healthy device counts); **allocation**
  (`allocate()` — picks specific healthy, not-already-allocated device
  IDs and calls the plugin's `Allocate` RPC for them).
- **`plugin_registry.rs` now routes by `PluginInfo.type`** instead of only
  accepting `"CSIPlugin"` — `"DevicePlugin"` registrations go to the new
  `DevicePlugins` registry via the identical `GetInfo`/
  `NotifyRegistrationStatus` handshake CSI drivers already used. Anything
  else still gets a real rejection response, not silence.
- **`Node.status.capacity`/`.allocatable` gained real plumbing** for
  extended resources — `node.rs::build_status()` now takes an
  `extra_capacity: &BTreeMap<String, u64>` parameter, threaded from a new
  `PodRuntime::device_plugin_capacity()` trait method (default: empty, so
  the mock runtime and any future runtime implementation are unaffected).
  This meant widening `node::register()`/`push_status()`'s signatures and
  `main.rs::heartbeat_loop()` to carry the runtime through — a real
  (small) refactor, not just additive.
- **`Allocate()` wired into `runtime/cri.rs::create_and_start_container()`**
  — a new pure `extended_resource_requests()` extracts every non-cpu/
  memory key from a container's `resources.limits`; any of those a
  registered device plugin actually backs gets allocated, and the
  response's envs/mounts/device-nodes/annotations are merged into the
  `ContainerConfig` right alongside everything else already built there
  (CRI's own `ContainerConfig.devices` field turned out to match the
  device plugin API's `DeviceSpec` message almost field-for-field —
  no translation gap to bridge).
- **New side table `device_allocations`** (`"sandbox_id/container_name" ->
  [(resource_name, device_ids)]`, same key shape `restart_counts` already
  uses) — so a container's devices get released back to the pool on
  restart-on-exit, init-container retry, sandbox GC, or pod removal,
  instead of being permanently stranded as "in use." Devices are also
  released immediately if `CreateContainer`/`StartContainer` fails after
  allocation succeeded — a failed container must not strand hardware.
- 30 new unit tests: `device_plugins.rs`'s `pick_devices` (the pure
  selection logic), capacity/registration-state transitions (register/
  deregister/re-register, stale-endpoint rejection); `node.rs`'s
  `build_status()` extra-capacity merge; `runtime/cri.rs`'s
  `extended_resource_requests()`.

477 tests passing with `--features cri` (up from 447), 179 mock-only (up
from 174 — `node.rs::build_status()`'s new tests are mock-buildable since
`node.rs` itself isn't `cri`-gated).
`deploy/lib/test/cases/device_plugins.sh` added: confirms the shared
registration directory exists (proves the watcher started), plus a
manual-note for the full flow — this suite has no GPU/FPGA hardware, and
a real device plugin binary isn't something to bundle in a test harness.
Noted as a natural future improvement: a small hand-rolled fake gRPC
device plugin (`GetInfo`/`ListAndWatch`/`Allocate` are all easy to fake
without real hardware) would make this fully automatable, unlike CSI's
`csi_pvc.sh`, which genuinely needs a real storage backend.

**Same confidence caveat as rounds 12/13**: no real device plugin was
reachable in the environment that built this — verified against the
vendored proto and real kubelet's documented Device Plugin API behavior,
not a live handshake. All failure modes (allocation failure, stream
disconnect, malformed registration) are logged warnings with the
container starting without that device (or, if a plugin's `Allocate`
genuinely can't be satisfied, that one extended-resource request being
silently dropped) — never a crash or a stuck reconcile loop.

## Round 13: dynamic CSI driver discovery + per-volume secrets (2026-07-31, same day)

Explicitly asked again (same pattern as rounds 11-12). Offered "deepen
PVC/CSI" (round 12's two flagged gaps) against the CPU/Memory/Topology
managers and device plugins; user picked deepening what already exists
over opening new surface.

- **New `plugin_registry.rs`** (`cri`-feature-gated): the client-side half
  of the CSI/DevicePlugin plugin-registration protocol, vendoring a
  cleaned-up `proto/pluginregistration.proto` (from
  `k8s.io/kubelet/pkg/apis/pluginregistration/v1/api.proto` — stripped the
  upstream file's `gogoproto` import/options, which are Go-codegen-only and
  have no prost equivalent, same treatment `cri.proto` already got for a
  protoc-version-specific field option). **The protocol is inverted from
  what the name suggests**: the *plugin* (a CSI driver's
  `node-driver-registrar` sidecar) runs the gRPC server, on a socket it
  creates in a shared watched directory; nodelet is the *client* — it polls
  the directory for new sockets (poll-based, matching `static_pods.rs`/log
  rotation's style rather than pulling in a filesystem-notification
  dependency), dials each one, calls `GetInfo()` to learn the driver's
  name/type/endpoint, and `NotifyRegistrationStatus()` to confirm or
  reject it.
- **`CsiDrivers`'s endpoint map is now mutable** (`Mutex<BTreeMap<...>>`,
  was a plain immutable map seeded once at construction) — new
  `register()`/`deregister()` methods the watcher calls as sockets appear/
  disappear. `NODELET_CSI_DRIVERS` still works exactly as before, as a
  seed for the same map a dynamic registration can now also populate or
  override.
- **Explicit rejection of non-CSI registrations** — device plugins use
  this *exact same* protocol, but nodelet doesn't implement the
  DevicePlugin gRPC API itself. Rather than silently ignore a device
  plugin's registration attempt (which would leave its registrar hanging/
  retrying forever without ever knowing why), `plugin_registry.rs` replies
  with a real `NotifyRegistrationStatus{plugin_registered: false, error:
  "..."}` — same courtesy real kubelet gives a plugin type it doesn't
  support.
- **`nodeStageSecretRef`/`nodePublishSecretRef`** — `runtime/cri.rs`'s
  `resolve_csi_source()` now resolves both (a new
  `resolve_csi_secret_ref()` helper) and threads them through to the CSI
  requests' `secrets` map, closing round 12's other explicitly-flagged
  simplification. `SecretReference.namespace` (optional, since
  `PersistentVolume` is cluster-scoped and has no natural pod namespace to
  inherit) falls back to the PVC's own namespace when unset.
- 6 new unit tests for `plugin_registry.rs`'s `scan_registry_dir()` (real
  `UnixListener` sockets in a scratch directory — genuinely exercises "is
  this actually a socket file," not a mock) and 6 for `CsiDrivers`'
  register/deregister/re-register state transitions.

447 tests passing with `--features cri` (up from 435), 174 mock-only
(unchanged — both additions are `cri`-gated).
`deploy/lib/test/cases/csi_plugin_registration.sh` added: an automated
check that the registry directory actually gets created (proves the
watcher started without erroring) plus a manual-note for the full
registration handshake, which needs a real CSI driver's registrar
pointed at nodelet instead of kubelet — not something this suite can
deploy on the cluster's behalf. `csi_pvc.sh` (round 12) now also
implicitly exercises this: run it *without* `NODELET_CSI_DRIVERS` set,
against a driver whose registrar is pointed at
`NODELET_PLUGIN_REGISTRY_PATH`, and it proves dynamic discovery end to
end if it still passes.

**Same confidence caveat as round 12, unchanged**: no CSI driver (or its
registrar sidecar) was reachable in the environment that built this — the
registration protocol implementation was verified against the vendored
proto and real kubelet's documented behavior, not a live handshake. Same
failure mode as always: a warning, not a crash, and static
`NODELET_CSI_DRIVERS` config keeps working regardless of whether dynamic
discovery ever succeeds.

## Round 12: PersistentVolumeClaim / CSI, first slice (2026-07-31, same day)

Explicitly asked again (same reasoning as round 11 — everything left is
big/invasive): user picked PVC/CSI over the CPU/Memory/Topology managers
and device plugins, calling it "the single biggest remaining feature gap
... real user-facing value," while accepting it as a first-slice/multi-round
effort rather than expecting full CSI support in one pass.

- **New `runtime/csi.rs`**: a minimal CSI Node-service client
  (`NodeStageVolume`/`NodeUnstageVolume`/`NodePublishVolume`/
  `NodeUnpublishVolume`/`NodeGetCapabilities`) using a freshly vendored
  `proto/csi.proto` (the stable v1 API from the upstream
  container-storage-interface/spec repo, tag `v1.9.0`) — compiled the same
  way `proto/cri.proto` already is (`tonic_prost_build`, `cri`-feature-only).
- **Deliberate scope-narrowing, documented up front in the module's own doc
  comment**: real kubelet discovers CSI drivers dynamically — a driver's
  DaemonSet registers its socket against kubelet's own plugin-registration
  gRPC service. Implementing that second server is real additional CSI
  plumbing on top of the Node service itself. This round takes the simpler
  route instead: `NODELET_CSI_DRIVERS=driver-name=unix:///path/to/socket`
  statically maps a driver name to its already-known socket path. This
  still talks to an unmodified, off-the-shelf CSI driver container (the
  registration dance is how kubelet *discovers* the socket, not something
  the Node RPCs themselves depend on) — it just needs that path configured
  up front instead of found automatically.
- **Wired into `runtime/cri.rs`**: `resolve_volumes()` gained a
  `persistent_volume_claim` branch — resolves the PVC (must be
  already-Bound; not-yet-bound is logged and retried next reconcile, same
  as any transient volume-resolution failure) to its `PersistentVolume`,
  extracts `.spec.csi`, calls `CsiDrivers::mount()`, and — on success —
  inserts the publish target path into the same `volume name -> host
  directory` map every other volume kind already populates. No special
  casing needed in `build_mounts()`: NodePublishVolume makes that directory
  a real, populated mountpoint, which then gets bind-mounted into the
  container exactly like a ConfigMap/Secret/emptyDir directory already is.
  `remove_pod()` gained a matching `unmount_csi_volumes()` that
  unpublishes (and, if this was the last pod referencing it, unstages)
  every CSI volume the pod had — best-effort, one failing driver call
  doesn't block the rest of teardown.
- **Per-node reference counting** (`CsiDrivers`'s `refs` map, keyed by
  `(driver, volume_handle)` -> the *set* of pod UIDs using it, not a plain
  counter) — `ensure_pod()`/`resolve_volumes()` runs on every reconcile of
  an already-running pod, not just once at creation, so a plain increment-
  on-mount counter would inflate without bound; a set makes repeated calls
  for the same pod a no-op, and `NodeUnstageVolume` only fires once the set
  is actually empty.
- 12 new unit tests, all pure logic: `has_stage_unstage_capability`
  (decides whether to call NodeStageVolume at all),
  `staging_path`/`mount_capability` (path/request construction). The
  gRPC plumbing itself (`connect_uds`, the actual Stage/Publish/Unpublish/
  Unstage calls) mirrors `runtime/cri.rs`'s existing CRI client exactly and
  is validated the same way that always has been: against a live socket,
  not a unit test — see the confidence note below.

435 tests passing with `--features cri` (up from 423), 174 mock-only
(unchanged — this round's new code is entirely `cri`-gated).
`deploy/lib/test/cases/csi_pvc.sh` added — creates a PVC against
`TEST_CSI_STORAGE_CLASS`, a pod mounting it, and verifies a file the
container wrote lands in the host-materialized volume path. Skips cleanly
without `TEST_CSI_STORAGE_CLASS` set to a StorageClass backed by both a
working external-provisioner *and* a driver also listed in the running
nodelet's `NODELET_CSI_DRIVERS` — real infrastructure this suite can't
stand up itself, same category as the graceful-shutdown and cgroup-write
manual/live-only checks from rounds 9 and 11.

**Known limitation, honestly flagged, same treatment as rounds 9 and 11**:
no CSI driver socket was reachable in the environment that built this, so
none of the actual gRPC calls (`NodeGetCapabilities`, `NodeStageVolume`,
`NodePublishVolume`, `NodeUnpublishVolume`, `NodeUnstageVolume`) have been
exercised against a real driver — only the proto compilation itself and
the pure request-construction logic were verified directly. The CSI wire
protocol is well-specified and this client mirrors the exact shape
`runtime/cri.rs`'s already-working CRI client uses (same `connect_uds`
pattern, same tonic-generated client style), so the main risk isn't
protocol-level, it's driver-specific behavior this slice doesn't handle:
drivers that require `nodeStageSecretRef`/`nodePublishSecretRef`
credentials, drivers whose `NodeGetCapabilities` needs a Controller-service
round-trip first (rare, but not impossible), and any driver-specific
`volume_context`/`publish_context` expectations beyond what's in
`PersistentVolume.spec.csi.volumeAttributes`. All of these fail as a
logged warning + the volume silently absent from the pod's mounts (the
pod still starts, just without that volume) — never a crash or a stuck
reconcile loop.

## Round 11: cgroup/QoS hierarchy + node allocatable enforcement (2026-07-31, same day)

Unlike rounds 6–10 (picked autonomously, "you pick the path"/"continue"),
this one was explicitly asked about first — the remaining list (PVC/CSI,
this round's item, CPU/Memory/Topology managers, device plugins) is bigger
and more invasive than recent rounds, so the user was asked to choose
rather than assuming. They picked this: "a real correctness gap (pods can
currently exceed what real kubelet would allow)" over PVC/CSI (bigger,
multi-round scope) and the CPU/Memory/Topology managers + device plugins
(lower value for nodelet's edge-device target).

- **QoS-scoped `cgroup_parent`** (`cgroup.rs::cgroup_parent_for`) — every
  pod sandbox now gets a real cgroup parent path scoped by QoS class
  (`/kubepods/pod<uid>` Guaranteed, `/kubepods/burstable|besteffort/pod<uid>`
  otherwise), wired into `runtime/cri.rs::ensure_pod` right alongside the
  existing `runtime_handler` resolution. Before this, `LinuxPodSandboxConfig.linux`
  was only ever populated for host-network pods — every other pod's
  sandbox had no `cgroup_parent` at all, so pods landed wherever the
  container runtime's own default happened to put them, with zero
  relationship to QoS class.
- **Key discovery that simplified this a lot**: CRI's own proto comment on
  `cgroup_parent` says "the cgroupfs style syntax will be used, but the
  container runtime can convert it to systemd semantics if needed" — so
  nodelet doesn't need to detect or configure a cgroup driver at all (no
  `NODELET_CGROUP_DRIVER`, unlike real kubelet's `--cgroup-driver` flag).
  It always builds the cgroupfs-style path and trusts the runtime to
  convert it if it's using systemd unit naming internally.
- **Node allocatable enforcement** (`cgroup.rs::enforce_node_allocatable`,
  called once at startup from `main.rs`) — creates and caps the top-level
  `kubepods` cgroup (`cpu.max`/`memory.max`, cgroup v2 only) at the node's
  allocatable resources, so pods collectively can never exceed it
  regardless of what any individual pod's own limits say. This is the
  actual "enforcement" real kubelet's `--enforce-node-allocatable=pods`
  (its own default) gives.
- **`Node.status.allocatable` is now a real computation, not `== capacity`**
  (`node.rs::allocatable_map`) — `capacity - (system-reserved +
  kube-reserved)`, new config: `NODELET_SYSTEM_RESERVED_CPU_MILLICORES`/
  `_MEMORY_BYTES`, `NODELET_KUBE_RESERVED_CPU_MILLICORES`/`_MEMORY_BYTES`
  (all default `0`, matching upstream — reservation is opt-in there too).
  This is a correctness fix independent of the cgroup enforcement above:
  even without cgroup v2 or root, the *reported* allocatable now reflects
  reservations, same as real kubelet's status always has.
- **Bonus, same code path**: `RuntimeClass.overhead` (`spec.overhead`)
  now also gets wired through, via a new `resource_list_to_linux_resources()`
  converting the flat `ResourceList` into `LinuxContainerResources` for
  `LinuxPodSandboxConfig.overhead` — the field existed right next to
  `cgroup_parent` in the same proto message, and the conversion logic was
  a near-identical variant of the existing `linux_resources()`, so this
  closed the previously-🟡 "RuntimeClass Overhead not implemented" note
  from round 7 for free rather than leaving it for its own round.
- 23 new unit tests: `cgroup_parent_for`, `cpu_max_line`/`memory_max_line`,
  `enforce_node_allocatable` (pointed at a scratch directory — proves the
  file layout/content, not real kernel cgroup semantics, see the caveat
  below), `allocatable_map`, `resource_list_to_linux_resources`.

423 tests passing with `--features cri` (up from 397), 174 mock-only (up
from 168 — `allocatable_map` lives in `node.rs`, not `cri`-gated; the rest
of this round's new code is in `cgroup.rs`, which is).
`deploy/lib/test/cases/cgroup_hierarchy.sh` added for live-cluster
validation — checks the `kubepods` cgroup exists with readable
`cpu.max`/`memory.max`, and that a BestEffort pod's cgroup lands somewhere
findable by UID under `kubepods` (tolerant of either cgroupfs or systemd
driver naming, since it can't assume which one a given cluster uses).

**Known limitation, honestly flagged, same treatment as round 9's D-Bus
glue**: `enforce_node_allocatable`'s actual cgroup v2 writes were never
exercised against a real `/sys/fs/cgroup` — this sandbox's cgroup v2 mount
is read-only to a non-root user, so only the pure logic (path building,
`cpu.max`/`memory.max` content formatting, and the file-creation flow
against a scratch directory standing in for the real path) could be
verified directly. The three things most likely to need a look on first
real use: (1) whether `cgroup.subtree_control` on `cgroup_fs_root`'s
top level already has `cpu`/`memory` delegated (a fresh systemd host
usually does; a container without the host's cgroup mount bind-mounted in
may not), (2) whether nodelet's own process has permission to write there
at all (needs root, or an equivalent capability/cgroup namespace grant),
(3) whether a systemd-driver containerd's own management of `kubepods.slice`
conflicts with nodelet also writing directly to a cgroupfs path in the
same tree (untested interaction — real kubelet with `--cgroup-driver=systemd`
uses a `dbus`/`systemd-run` call to set the slice's properties instead of
raw file writes for exactly this reason, which this round's simpler
cgroupfs-direct-write approach doesn't replicate). All three fail safe: a
logged warning, not a crash — `Node.status.allocatable` is still reported
correctly regardless (that computation doesn't touch the filesystem at
all), and pod scheduling/creation is entirely unaffected either way.

## Round 10: `/metrics/resource` + `/metrics/cadvisor` (2026-07-31, same day)

Continued closing gaps ("continue" — no further scoping given). Picked
these next: they were the last two items on `unimplemented.sh`'s active-
placeholder list, both reuse `/stats/summary`'s existing `PodUsage`/
`UsageStats` data (round 7), and neither touches the container-creation
path at all — a low-risk, self-contained follow-up after round 9's larger
D-Bus addition.

- **New `server::prom_metrics`** (`cri`-feature-gated, same as every other
  `server::*` module) — renders Prometheus text-exposition-format output
  from the same `PodUsage` data `/stats/summary` already collects via CRI's
  `ListPodSandboxStats`. No separate collection path for either endpoint.
- **`/metrics/resource`** implements
  [KEP-2371](https://github.com/kubernetes/enhancements/tree/master/keps/sig-node/2371-cri-pod-container-stats)'s
  small, well-specified metric set completely: `node_cpu_usage_seconds_total`,
  `node_memory_working_set_bytes`, `pod_cpu_usage_seconds_total`,
  `pod_memory_working_set_bytes`, `container_cpu_usage_seconds_total`,
  `container_memory_working_set_bytes`.
- **New node-wide CPU accounting** (`metrics.rs::read_node_cpu_seconds`) —
  parses the aggregate `cpu ` line of `/proc/stat` (same technique
  `node_exporter` uses) to get cumulative node CPU core-seconds since boot.
  This closes the "`/stats/summary` doesn't report node CPU" gap noted in
  round 7 too, for free — `server::stats::node_stats()` still doesn't use it
  (out of scope for this round; `/stats/summary`'s JSON shape wasn't
  touched), but the underlying data now exists for whichever endpoint wants
  it next.
- **`/metrics/cadvisor` is a deliberately scoped-down subset**, not the
  full cAdvisor catalog — real cAdvisor exposes dozens of metrics (network/
  disk I/O, per-cpu-core breakdowns, `container_last_seen`, spec/limit
  metrics, and more) that would be a lot of surface for an edge agent
  that's otherwise deliberately lean, and CRI's own stats don't carry most
  of that data anyway (no network/disk I/O in `ListPodSandboxStats`).
  Implements the four metrics most dashboards/scrapers built against
  cAdvisor actually read: `container_cpu_usage_seconds_total`,
  `container_memory_usage_bytes`, `container_memory_working_set_bytes`,
  `container_memory_rss`. Also drops cAdvisor's usual `id`/`name`/`image`
  labels (container cgroup path, runtime name, image ref) — nothing in
  `PodUsage` tracks those today, and faking them would be worse than
  omitting them; only `namespace`/`pod`/`container` labels are emitted.
- Deleted `deploy/lib/test/cases/unimplemented.sh` — its one remaining
  placeholder test was exactly this gap; replaced by real functional tests
  in the new `prom_metrics.sh` (same treatment streaming.sh/stats.sh got
  when their gaps closed in earlier rounds).

397 tests passing with `--features cri` (up from 374), 168 mock-only (up
from 161 — `read_node_cpu_seconds`'s pure parser lives in `metrics.rs`,
which isn't `cri`-gated).

## Round 9: graceful node shutdown (2026-07-31)

Continued closing gaps ("continue" — no further scoping given). Picked
graceful node shutdown next: it's the one item on the "biggest remaining"
list that's specifically valuable for nodelet's actual target hardware
(edge devices get power-cycled far more often than a datacenter node ever
does), and unlike PVC/CSI or the CPU/Memory/Topology managers it's a
self-contained addition that doesn't touch the container-creation path at
all.

- **New `shutdown.rs`** (`cri`-feature-gated, like `server.rs` and
  `static_pods.rs`'s real uses) — holds a systemd-logind shutdown-delay
  inhibitor lock (`Inhibit("shutdown", "nodelet", ..., "delay")` over
  D-Bus, via the `zbus` crate) for as long as nodelet is running with the
  feature enabled, and subscribes to logind's `PrepareForShutdown` signal.
  On `PrepareForShutdown(true)`, terminates every pod on the node through
  the *same* graceful path a normal delete already gets
  (`PodRuntime::remove_pod` — `preStop` + a bounded `StopContainer`
  timeout), bounded by a fixed time budget, then drops the held fd (closing
  it releases the lock) so shutdown actually proceeds. On `(false)`
  (shutdown cancelled), re-acquires the lock for next time.
- **Priority-ordered, budget-capped** — non-critical pods terminate first;
  `system-node-critical`/`system-cluster-critical` pods (reuses
  `eviction::is_critical`, the same definition node-pressure eviction
  already uses) get their own reserved sub-budget
  (`NODELET_SHUTDOWN_GRACE_PERIOD_CRITICAL_SECS`) and go last, so ordinary
  workloads get first crack at a clean exit while system add-ons keep
  serving as long as possible. Each pod's own `terminationGracePeriodSeconds`
  is capped to whatever's actually left in its group's budget — a pod
  asking for a 5-minute grace period doesn't get it if the node only has 30
  seconds of runtime left.
- **New config**: `NODELET_SHUTDOWN_GRACE_PERIOD_SECS` (default `0`,
  disabled — matches upstream, where this is opt-in) and
  `NODELET_SHUTDOWN_GRACE_PERIOD_CRITICAL_SECS` (default `0`, clamped to
  never exceed the total). `run()` doesn't even connect to D-Bus when
  disabled, so this is a true no-op on hosts without systemd or a system
  bus, same as every other opt-in background loop in this codebase.
- Pure scheduling logic (`split_by_criticality`, `budget_split`,
  `capped_grace_period`) is fully unit tested — 14 new tests. The D-Bus
  glue itself (`Connection::system()`, the `Inhibit` call, the signal
  stream) is **not** — see the caveat below.

374 tests passing with `--features cri` (up from 360), 161 mock-only (this
feature is entirely `cri`-gated, so it adds nothing to the mock-only count).
`deploy/lib/test/cases/graceful_shutdown.sh` added as a manual-note skip
test, not an automated one — see why below.

**Known limitation, honestly flagged, same treatment as round 6's exec/
attach proxy**: the D-Bus interaction was written and compiled against the
`zbus` 5.x API (verified by reading its vendored source directly, since no
network docs were consulted) but has never been run against a real
systemd-logind — there's no system/session D-Bus bus reachable in the
sandbox that built this. The three places most likely to need adjustment
on first real use: (1) whether `Connection::system()` succeeds inside
whatever init/container context nodelet actually runs in (needs
`/run/dbus/system_bus_socket` reachable — likely fine on a real systemd
host, questionable inside a minimal container without the host's D-Bus
socket bind-mounted in), (2) whether the `Inhibit` call is permitted by the
host's polkit policy for whatever user nodelet runs as (typically requires
root, or an explicit policy grant), (3) whether `PrepareForShutdown`'s
signal body actually deserializes as a bare `bool` the way `msg.body()
.deserialize()` expects (this matches the D-Bus signal signature `b`
documented for logind, but wasn't observed on the wire). None of these can
regress anything when the feature is disabled (the default) — worst case
if any of the three is wrong, `run()` logs a warning and returns, same as
today, and shutdown behaves exactly as it did before this feature existed
(SIGKILL-on-power-loss, not preStop-first). Also not implemented: real
kubelet's per-`PriorityClass`-level budget bands (`shutdownGracePeriodByPod
Priority`, a list of arbitrary priority/grace-period pairs) — this uses the
simpler two-tier critical/non-critical split kubelet's own
`--shutdown-grace-period`/`--shutdown-grace-period-critical-pods` flags
predate that with, which is a closer match to nodelet's minimalism anyway.

## Round 8: ephemeral containers (`kubectl debug`) (2026-07-30, same day)

Continued closing gaps ("continue" — no further scoping given). Picked
ephemeral containers next: it reuses the exec/attach proxy infrastructure
from round 6 for actually *using* a debug session, so `kubectl debug -it`
was already half-working — this closes the other half, getting the
container itself created and started.

- **`spec.ephemeralContainers` → CRI containers** — `ensure_pod()` now walks
  `spec.ephemeralContainers` after the app-container loop and starts any not
  already present, via a new `ensure_ephemeral_container()` in
  `runtime/cri.rs`. Unlike app containers, these are **one-shot**: once a
  container with that name exists (running or exited), it's never recreated
  or restarted, regardless of the pod's `restartPolicy` — matches real
  kubelet, which has no notion of "restart a debug session."
- **`EphemeralContainer` → CRI `ContainerConfig`** — reuses the exact same
  `create_and_start_container()` app/init containers go through, via a new
  `ephemeral_to_container()` that maps `EphemeralContainer`'s fields onto the
  regular `Container` shape (they're near-identical; `ports` is dropped,
  matching real kubelet, and `targetContainerName`, process-namespace-sharing
  metadata, is a no-op here since nodelet's sandbox containers already share
  the sandbox's PID namespace).
- **New `CTR_EPHEMERAL_LABEL`** — same pattern as the existing init-container
  label: lets status-building and future GC tell ephemeral containers apart
  from app containers without a second side table. `build_status()` now
  excludes both init- and ephemeral-labeled containers from the app-container
  phase/readiness computation (a debug container exiting must never flip the
  pod to Succeeded/Failed, or gate `ContainersReady`).
- **`PodStatus.ephemeralContainerStatuses`** — new `RuntimeStatus.ephemeral_containers`
  field (mirrors `init_containers`), populated in `pods.rs::build_pod_status`.
  Reported as `Terminated` (not `Waiting`/`PodInitializing`) when not
  running, since an ephemeral container that's stopped is *done*, not
  "still starting up" — the opposite framing init containers need.

360 tests passing with `--features cri` (up from 353), 161 mock-only.
`deploy/lib/test/cases/ephemeral_containers.sh` added — runs
`kubectl debug <pod> --image=... --container=debugger -- sleep 3600` against
a live pod and asserts `ephemeralContainerStatuses` reports it running and
the pod's own phase is untouched; skips cleanly if the test cluster's
kubectl/apiserver doesn't support the `ephemeralcontainers` subresource.

**Known simplification, documented not hidden**: exit codes aren't tracked
for ephemeral containers (`ContainerStateTerminated.exit_code` is always
reported as `0`) — real kubelet fetches this via `ContainerStatus` same as
init containers do, but that's an extra CRI round-trip per ephemeral
container on every status build, only worth paying if something actually
reads it; nothing does yet since `kubectl debug` output goes through
`exec`/`attach`, not the exit code.

## Round 2: correctness gaps closed (user-chosen sequencing)

User picked "correctness gaps first" out of the full list below — these were
"silently wrong" (a pod spec that looks correct behaved differently than on
real Kubernetes), as opposed to "missing feature, fails loudly." All five
are now done, each with unit tests for the pure translation logic
(`runtime/cri_tests/linux_resources.rs`, `linux_security_context.rs`,
`dns_config.rs`, `registry_auth.rs`, `init_container_decision.rs`):

- ✅ **Init containers** — `spec.initContainers` now run to completion, in
  order, before app containers start (`CriRuntime::ensure_init_containers`).
- ✅ **Resource requests/limits** — translated to CRI `LinuxContainerResources`
  (cpu shares from requests, cpu quota/period + memory limit from limits) —
  containers are no longer unbounded regardless of what the Pod spec asks for.
- ✅ **securityContext** — `runAsUser`/`runAsGroup`, `privileged`,
  `readOnlyRootFilesystem`, capabilities add/drop, `allowPrivilegeEscalation`
  → `no_new_privs`, `supplementalGroups`, and `seccompProfile` (RuntimeDefault/
  Unconfined/Localhost) now reach CRI's `LinuxContainerSecurityContext`.
  Still not translated: AppArmor profile, SELinux options, and runAsNonRoot
  *verification* against the image's actual configured user (needs image
  inspection, not just pass-through — left as a follow-up).
- ✅ **DNS config** — `dnsPolicy` (ClusterFirst/Default/None) +
  `dnsConfig` now set CRI's `PodSandboxConfig.dns_config`, via new
  `NODELET_CLUSTER_DNS`/`NODELET_CLUSTER_DOMAIN` config (kubelet's
  `--cluster-dns`/`--cluster-domain` equivalents).
- ✅ **Private registry auth** — `imagePullSecrets` (`kubernetes.io/dockerconfigjson`)
  are now resolved into CRI `AuthConfig` for `PullImageRequest`. Legacy
  `kubernetes.io/dockercfg` (no `"auths"` wrapper) and ServiceAccount-linked
  pull secrets are not handled yet.

215 tests passing with `--features cri` (up from 164), 107 with the default
(mock-only) build. Both builds compile clean.

## Round 3: pod-lifecycle correctness + eviction (2026-07-30, same day)

User said "keep closing the gaps, get to 100%" with no further scoping —
continued in the same "correctness first" vein rather than jumping straight
to the largest single remaining item (the streaming exec/logs/attach/
port-forward server, which needs a whole new TLS-authenticated HTTP(S)
listener and is a project of its own). Closed, each with unit tests:

- **Termination grace period + preStop hook** — `PodRuntime::remove_pod()`
  now takes the full `Pod` (was just `namespace`/`name`) specifically so it
  can read `terminationGracePeriodSeconds` and run each container's
  `preStop` hook before stopping it.
- **postStart lifecycle hook** — runs right after `StartContainer` succeeds.
- **Exit-code-aware `restartPolicy: Never` phase** — `Failed` vs `Succeeded`
  now actually depends on the exit code, not just "did everything exit."
- **Real container restart counts** — was hardcoded `0` since the very
  first pass; now a real per-container counter, also used as CRI's
  `attempt` number so restarted containers don't overwrite their own log file.
- **Projected + downwardAPI volumes** — the two most commonly hit
  "volume type not supported" warnings before this (a projected volume is
  what backs the auto-mounted `kube-api-access-*` service account volume,
  though its `serviceAccountToken` source specifically still isn't — see below).
- **hostAliases + fsGroup** — the two security/identity-adjacent volume
  behaviors that were pure no-ops before.
- **Node-pressure eviction** — the one item `ARCHITECTURE.md` had already
  called out by name as not implemented; now something actually happens
  when a node reports pressure, not just a condition update.

260 tests passing with `--features cri` (up from 215 at the start of this
round), 125 with the default (mock-only) build.

## Round 7: /stats/summary, RuntimeClass, usage-based eviction (2026-07-30, same day)

Continued closing gaps ("continue" — no further scoping given). Picked
these three because they compound: `/stats/summary` needed real per-pod
usage data anyway, and once that existed, feeding it back into eviction's
ranking (previously request-based only, explicitly flagged as a
simplification in round 3) was a small, self-contained follow-on rather
than a new investigation.

- **`/stats/summary`** — discovered CRI already solves the hard part:
  `ListPodSandboxStats` returns real per-pod *and* per-container CPU/memory
  usage in one call, with the runtime (containerd) handling cgroup-path/
  driver differences internally. No cgroup file reading needed at all.
- **Eviction ranking now usage-based** — `eviction.rs`'s tie-break within a
  QoS class uses the same CRI stats when available, falling back to
  requested memory per-pod otherwise (mock runtime, or a too-new pod CRI
  hasn't measured yet).
- **RuntimeClass** — `spec.runtimeClassName` now resolves to CRI's
  `runtime_handler` (was hardcoded empty/default before), so gVisor/Kata/
  etc. selection actually works.

353 tests passing with `--features cri` (up from 345), 157 mock-only.
`deploy/lib/test/cases/stats.sh` and `runtime_class.sh` added for live
validation — the RuntimeClass test only proves the *lookup and wiring*
using whatever handler this containerd already knows about (commonly
`runc`), since alternative-runtime binaries aren't something this suite
can assume are installed.

## Round 6: the kubelet HTTP(S) server — exec/logs/attach/port-forward (2026-07-30, same day)

User said "finish closing the gaps" — this was the one deliberately deferred
in round 4 as needing its own dedicated pass ("a project of its own: TLS,
auth, the whole listener"). Built it:

- `crates/nodelet/src/server/` (new, `cri`-feature-gated): `tls.rs`
  (self-signed cert via `rcgen`, cached as DER), `auth.rs` (bearer token
  via `TokenReview`), `routes.rs` (path/query parsing + dispatch), `logs.rs`
  (`kubectl logs`, including `-f`), `exec.rs` (`kubectl exec`/`attach`/
  `port-forward`, proxied to the CRI runtime's own streaming server rather
  than reimplementing SPDY/WebSocket).
- New `PodRuntime` trait methods (`container_log_path`, `exec_url`,
  `attach_url`, `port_forward_url`) implemented in `cri.rs` against CRI's
  `ContainerStatus`/`Exec`/`Attach`/`PortForward` RPCs.
- `Node.status.daemonEndpoints.kubeletEndpoint.port` — never set before;
  without it the apiserver has nowhere to proxy exec/logs requests to
  regardless of whether a server exists.
- New dependencies (all `cri`-gated): `rcgen`, `hyper`+`hyper-util` server
  features, `http-body-util`, `tokio-rustls`, `percent-encoding`,
  `tokio-stream`.
- Everything logic-based has real unit tests: CRI log-line parsing/
  reassembly, path/query routing, bearer token extraction, and (genuinely
  integration, not mocked) TLS cert generation/caching/permissions against
  the real filesystem. 345 tests passing with `--features cri` (up from
  302), 155 mock-only.
- **Honest confidence note**: the connection-splicing proxy in `exec.rs`
  (dial the CRI-returned URL, replay the client's upgrade request, mirror
  the response, `copy_bidirectional` the two upgraded connections) was
  written as carefully as reasoning allows but never observed completing a
  real SPDY/WebSocket handshake — this sandbox has no live cluster to test
  against. `deploy/lib/test/cases/streaming.sh` exists specifically to
  prove or disprove this the first time it runs for real; treat `kubectl
  exec` as the most likely thing in this round to need a live-cluster fix.
  `kubectl logs` (no protocol upgrade involved, just an HTTP response body)
  carries much higher confidence.
- Still explicitly out: `/stats/summary` (no usage-stats collector),
  client-cert auth (bearer token only), real `SubjectAccessReview`
  authorization (currently `AlwaysAllow` once a token authenticates,
  matching kubelet's own historical default).

## Round 5: live-cluster e2e test suite + initContainerStatuses fix (2026-07-30, same day)

User asked for two things: keep writing Rust tests for whatever's testable
that way, and — for what genuinely isn't (this is a live-container-runtime
project; a lot of correctness can only really be proven against a real
apiserver + real containerd) — a bash integration-test suite, structured
like `deploy/bootstrap-source.sh`'s `lib/*.sh` module pattern, that the user
runs manually against a real k3s deployment.

- **Found and fixed while building the suite**: `PodStatus.initContainerStatuses`
  and the `Initialized` condition were never populated at all —
  `kubectl describe`'s `Init:N/M` display had nothing to read, and
  `Initialized` always reported `True` even while genuinely waiting on init
  containers. New `RuntimeStatus.init_containers`/`.initialized` fields,
  threaded through `mock.rs`/`cri.rs`/`pods.rs`, with new unit tests.
- **`deploy/test-e2e.sh` + `deploy/lib/test/`**: a harness (register/run/
  assert, PASS/FAIL/SKIP reporting), kubectl wait/get helpers, and one case
  file per feature area, covering nearly everything from rounds 1–4 against
  a real cluster — pod lifecycle, init container ordering (structural, not
  just status-string), crash-restart + restart counts, exit-code-aware
  `Never` phase, all three probe types (including a real `httpGet` against
  a real pod IP), postStart/preStop hooks, termination grace period,
  ConfigMap/Secret/downwardAPI/projected volumes, real `serviceAccountToken`
  minting, hostAliases, fsGroup, `runAsUser`/`readOnlyRootFilesystem`,
  custom DNS config, **resource limits actually enforced in the container's
  own cgroup v2 files** (not just translated correctly in isolation), node
  status/pressure conditions, image GC, static pods + mirror pods, log
  rotation, and — deliberately — active assertions that `kubectl exec`/
  `kubectl logs` still *don't* work, so this suite fails loudly instead of
  going silently stale the moment someone lands the streaming server.
- The key trick making most of this possible without `kubectl exec`/`logs`:
  single-node architecture means the test script runs on the same host as
  nodelet, so a container's self-check output written into a shared
  `emptyDir` — or nodelet's own materialized ConfigMap/Secret/downwardAPI/
  projected volume — is directly readable off the host filesystem at the
  exact path bind-mounted into the container.
- Deliberately **not** automated (documented as manual procedures instead):
  node-pressure eviction and orphaned-sandbox GC, since exercising either
  needs exhausting a real resource or stopping nodelet out from under a pod
  — not something to do automatically to a host/service someone's relying on.

## Round 4: PID pressure, log rotation, static/mirror pods, serviceAccountToken (2026-07-30, same day)

User said "you pick the path... let's get this finished" — picked verifiable,
self-contained gaps over the streaming exec/logs/attach/port-forward server,
which is large enough to be a project of its own (TLS, auth, a whole new
listener) and can't be validated here without a live cluster to test
against. Explicitly did **not** attempt that this round; see below.

- **Real PID pressure** — was the one pressure signal still hardcoded
  `False` after rounds 1–3 fixed memory/disk; now real, same pattern.
- **Container log rotation** — `--container-log-max-size`/`-max-files`
  equivalent; previously logs grew forever.
- **Static pods + mirror pods** — the big win here is architectural, not
  just code volume: static pods reuse the exact same `PodRuntime` normal
  apiserver-sourced pods do, so every correctness fix from rounds 1–3
  (resource limits, securityContext, probes, volumes, ...) applies to static
  pods for free. Disabled by default (`NODELET_STATIC_POD_PATH` unset),
  matching upstream.
- **serviceAccountToken projected volume** — checked kube-rs 4.0 first (no
  typed helper for the `TokenRequest` subresource; used `kube::Client::request`
  with a raw HTTP call instead). Real apiserver-signed tokens, not a stub —
  this is what every actual `kube-api-access-*` volume needs to let a pod
  authenticate back to the apiserver, previously the one skipped source in
  an otherwise-working projected volume.

297 tests passing with `--features cri` (up from 260 at the start of this
round), 150 with the default (mock-only) build.

## Full kubelet responsibility list vs. current nodelet state

Legend: ✅ done · 🟡 partial · ❌ missing

### Pod & container lifecycle
- ✅ Pod sandbox + container create/start/stop/remove via CRI
- 🟡 **Restart-on-exit honoring `restartPolicy`, now with crash-loop backoff** (round 73; found in round 72's re-audit) — `restart_backoff_ready()`/`record_restart_backoff()` (new `restart_backoff` side table on `CriRuntime`) gate the actual restart, not just its status reporting: the first restart after any exit is always immediate, but a container that keeps exiting is throttled with an exponentially growing delay (10s base, doubling, capped at 5 minutes — matching real kubelet's own `flowcontrol.Backoff` constants exactly), resetting back to the base delay after ~10 minutes without needing another restart attempt. This closes the real behavior gap, not just the missing status string — nodelet's event-driven architecture (every status write is itself a Pod modification that re-triggers this controller's own watch stream, feeding right back into another reconcile) meant a crash-looping container was previously recreated as fast as the runtime could physically create+start it, with zero throttle. **Still not implemented**: the `CrashLoopBackOff` `state.waiting.reason` string itself — `ContainerStatus` has no `lastState` tracking at all yet (a separate, larger pre-existing gap this round didn't take on), so during the backoff window the container's status still shows its real `Terminated` state (accurate exit code/reason) rather than kubectl's familiar `Waiting: CrashLoopBackOff` + `lastState: Terminated` pairing.
- ✅ **Init containers** — run to completion, in order, before app containers (`ensure_init_containers`)
- ✅ **Native sidecar containers** (round 36; `initContainers[].restartPolicy: "Always"`, GA since 1.29, found in round 35's re-audit) — `sidecar_init_decision()` routes a sidecar-marked init container through its own decision matrix: doesn't block later init/app containers on its own exit (only on having started at all), restarts on exit like a normal container for the pod's whole lifetime, and its real probe-based readiness folds into the pod's overall `Ready`/`ContainersReady` (`pods.rs::build_pod_status()`). Genuinely automated e2e tests. **Documented simplification**: sidecars aren't stopped strictly *after* app containers on teardown (one pass instead), unlike upstream's exact ordering. See round 36 notes.
- ✅ **Ephemeral containers** (`kubectl debug`) — `spec.ephemeralContainers` started once (never restarted) via `ensure_ephemeral_container()`, reported in `PodStatus.ephemeralContainerStatuses`, excluded from pod phase/readiness. Exit codes not tracked (always reported `0`) — see Round 8 notes.
- ✅ **postStart / preStop lifecycle hooks** (`exec`/`httpGet`/`sleep`; not `tcpSocket`) — `run_lifecycle_hook()`. A failing `postStart` is logged, not (yet) turned into a container kill+restart like real kubelet does.
- ✅ **`lifecycle.stopSignal`** (GA 1.33; found in round 65's fresh gap re-audit; closed round 66) — translates directly to CRI's own `Signal` enum on `ContainerConfig.stop_signal` (native support, never wired up before this), a naming-convention-only translation (`stop_signal_cri()`/`stop_signal_k8s()`) since k8s and CRI define the identical 65-signal set. `containerStatuses[].stopSignal` is reported back from the runtime's own `ContainerStatus.stop_signal`, but only for exited containers (reuses an RPC already being made for termination details) — a still-running container's `stopSignal` isn't populated, a documented scope limitation to avoid a new per-container RPC on every healthy-pod reconcile.
- ✅ **Probe-level `terminationGracePeriodSeconds`** (round 44; added ~1.25, found in round 35's re-audit) — a `livenessProbe` can specify its own grace period for the container kill it triggers, distinct from the pod's own `terminationGracePeriodSeconds`; new pure `probes::probe_grace_period_seconds()` resolves the override (else the pod's own) and threads it into `PodRuntime::restart_container()`'s new `grace_period_seconds` parameter, replacing a previously-hardcoded `10`. Genuinely automated e2e test (a `SIGTERM`-trapping container that can only die via the grace-period kill, constructed so a wrong value causes an observable timeout). **Scoped to liveness probes only** — the startup-probe loop has no failure-threshold-triggered restart at all yet (pre-existing simplification: it retries forever until it passes), so there's no live code path for a startup-probe override to apply to yet either. See round 44 notes.
- ✅ **Termination grace period** — `terminationGracePeriodSeconds` now drives `preStop` + a per-container `StopContainer` timeout before `StopPodSandbox` (`graceful_stop_containers()`), instead of an untimed sandbox stop.
- ✅ **Container restart count** — real per-container counter (`restart_count_from`/`bump_restart_count_in`), threaded through `ContainerConfig.metadata.attempt` too (so restarted containers get distinct log files, not overwritten ones).
- ✅ **Exit-code-aware phase computation** — `restartPolicy: Never` now reports `Failed` (not `Succeeded`) when a container exited nonzero (`compute_phase()`'s new `any_failed` parameter).
- ✅ **`terminationMessagePath`/`terminationMessagePolicy`** (round 24; found in round 22's re-audit) — `create_and_start_container()` bind-mounts an empty host file at the container's `terminationMessagePath` (default `/dev/termination-log`) for App/Init containers, same approach real kubelet uses; `build_status()` reads it back (capped at 4096 bytes, keeping the last bytes if larger) into `ContainerStatus.state.terminated.message` for every exited container. Closing this also surfaced and fixed a bigger pre-existing gap: regular/init containers never reported a `terminated` state at all before this round (always `Waiting: ContainerCreating` forever once exited) — see round 24 notes. Still not implemented: `FallbackToLogsOnError` (documented, deliberate — nodelet always behaves as `File` policy, a strict subset of correct behavior, never wrong/misleading).
- ✅ **Pod `readinessGates`** (round 23; found in round 22's re-audit) — `spec.readinessGates` lets an external controller contribute additional `PodCondition`s that must all be `True` (alongside the built-in `ContainersReady`) before kubelet reports the pod's own `Ready` condition as `True`. `build_pod_status()`'s `Ready` computation now checks every gate's named condition against `prev`'s conditions; a missing condition counts as not-satisfied, matching upstream. Fixing this also required (and fixed) a real pre-existing bug: nodelet's JSON-Merge-Patch status writes were silently deleting any condition an external controller had set, since the whole `conditions` array got replaced wholesale — now foreign conditions are copied forward on every write. See round 23 notes.
- ✅ **Startup probe failure triggers a restart** (round 47; found in round 45's re-audit) — the startup-probe loop now checks the new public `ProbeTracker.failures` count against `failureThreshold` on every non-passing iteration, calling `restart_container()` (reusing round 44's `probe_grace_period_seconds()` unchanged — it was already generic over which probe called it) and resetting the tracker, then continuing to loop (the supervisor task lives for the container's whole lifetime, so it must keep re-attempting startup probing against the recreated instance rather than giving up). Genuinely automated e2e test (a marker file that's never created, so the probe can only ever fail — a nonzero restart count is direct proof). See round 47 notes.
- ✅ **`PodStatus.qosClass`** (round 55; found in round 54's re-audit) — new `QosClass::as_str()` matches real kubelet's exact API constants; `build_pod_status()`/`write_status()` gained a `qos` parameter, computed via the existing `eviction::qos_class()` and threaded in from all 4 `Pod`-derived status call sites, the same pattern round 23 already established for `readiness_gates`. Fixing this also required updating a pre-existing test whose premise ("qosClass is server-owned, nodelet doesn't touch it") no longer held — switched to `nominatedNodeName`, a field that's genuinely still server-owned. Genuinely automated e2e test (a real `Guaranteed` pod). See round 55 notes.
- ✅ **`PodStatus.hostIPs`** (round 56; plural, dual-stack; found in round 54's re-audit) — `build_pod_status()` now sets `hostIPs` alongside the existing singular `hostIP` unconditionally (a one-element list, matching real kubelet's own single-stack behavior), mirroring how `podIPs`/`podIP` already coexist correctly in this same struct. Genuinely automated e2e test. See round 56 notes.
- ✅ **`ContainerStatus.containerID` scheme prefix** (round 57; found in round 54's re-audit) — `CriRuntime` gained a `runtime_name` field (from a one-time `Version` RPC call, never called before this round) and new pure `format_container_id()` applies the real `<runtimeName>://<id>` format right where `ContainerRuntimeStatus.container_id` is populated, fixing both `ContainerStatus.containerID` and `state.terminated.containerID` (both read the same field) in one change. Best-effort: falls back to `"unknown"` rather than failing `connect()` if the `Version` call fails. Genuinely automated e2e test (asserts the scheme-separator shape, not a specific runtime name). Not the same finding as round 52's `imageID` — that field is *not* scheme-prefixed by real kubelet, so round 52's implementation needed no revisiting. See round 57 notes.

### Resource management
- ✅ **Container resource requests/limits** — translated to CRI `LinuxContainerResources` (cpu shares/quota/period, memory limit; `linux_resources()`)
- ✅ **QoS cgroup hierarchy** (`--cgroups-per-qos`) — every pod sandbox now gets a `cgroup_parent` scoped by QoS class (`cgroup.rs::cgroup_parent_for`): `/kubepods/pod<uid>` for Guaranteed, `/kubepods/burstable/pod<uid>` / `/kubepods/besteffort/pod<uid>` otherwise, wired into `runtime/cri.rs::ensure_pod`.
- ✅ **cgroup driver — no detection needed.** CRI's own `LinuxPodSandboxConfig.cgroup_parent` proto contract specifies the cgroupfs-style syntax is always sent, with "the container runtime can convert it to systemd semantics if needed" — nodelet always builds the cgroupfs-style path and lets the runtime do any systemd-unit-naming conversion, matching that contract exactly. No `--cgroup-driver`-equivalent config needed.
- ✅ **Node allocatable enforcement** (`--enforce-node-allocatable=pods`, its own upstream default) — `cgroup.rs::enforce_node_allocatable`, called once at startup, creates and caps the top-level `kubepods` cgroup (cpu.max/memory.max) at `Node.status.allocatable` so pods collectively can never exceed it. `Node.status.allocatable` itself is now `capacity - (system-reserved + kube-reserved)` (`node.rs::allocatable_map`) rather than always equal to capacity — a real correctness fix, not just the enforcement mechanism (`NODELET_SYSTEM_RESERVED_CPU_MILLICORES`/`_MEMORY_BYTES`, `NODELET_KUBE_RESERVED_CPU_MILLICORES`/`_MEMORY_BYTES`, all default `0`). Best-effort: needs root + cgroup v2 (cgroup v1 unsupported, matching modern kubelet defaults), logs and continues on failure rather than blocking startup — **unvalidated against a real cgroup v2 hierarchy**, no writable `/sys/fs/cgroup` in the sandbox that built this; see `deploy/lib/test/cases/cgroup_hierarchy.sh` for the live-cluster check.
- ✅ **CPU Manager** (`cpu_manager.rs`, `static` policy) — Guaranteed-QoS containers requesting a whole number of CPUs get pinned to exclusive cores (`cpuset_cpus`); every other container gets the current shared pool (total minus reserved minus exclusively-claimed). Opt-in (`NODELET_CPU_MANAGER_POLICY`, default `none` matching upstream). **Bidirectional as of round 16**: `runtime/cri.rs::refresh_shared_pool_cpusets()` retroactively shrinks/grows every already-running shared-pool container's `cpuset_cpus` via CRI's `UpdateContainerResources` whenever an exclusive claim is made or released, matching real kubelet's own policy behavior — not just newly-created containers. As of round 17, NUMA-aware when Topology Manager is also enabled (`allocate_preferring()`); simple ascending-CPU-ID selection otherwise.
- ✅ **Memory Manager** (`memory_manager.rs`, `static` policy) — Guaranteed-QoS containers with a memory limit get pinned to a single NUMA node (`cpuset_mems`). Opt-in (`NODELET_MEMORY_MANAGER_POLICY`, default `none`). **First-slice scope**: never spans multiple NUMA nodes (falls back to unconstrained if no single node has enough free capacity, rather than upstream's multi-node spanning); no shared-pool tracking or `UpdateContainerResources` retroactive sweep for non-pinned containers (unlike CPU Manager) — they're simply left unconstrained; no per-NUMA-node `--reserved-memory`-equivalent reservation. See round 18 notes.
- ✅ **Topology Manager** (`topology.rs`) — coordinates CPU Manager, Memory Manager (as of round 18), and device plugins so a container's exclusive cores, pinned memory, and allocated devices land on the same NUMA node. Opt-in (`NODELET_TOPOLOGY_MANAGER_POLICY`, default `none`); `best-effort` prefers alignment without rejecting, `single-numa-node` rejects the container outright if no single NUMA node satisfies every hint provider. **`restricted`** (round 20) gets a real, bounded multi-node relaxation instead — `spread()` places each hint provider on its own best node independently when no single node works for everyone, rejecting only if some provider's request can't be placed anywhere at all. **Single-node-only alignment, not upstream's full bitmask/permutation combination search** — `align()`/`spread()` reach the same answer as upstream whenever a single node can satisfy everything or every provider individually has *some* home, but don't search for upstream's genuinely joint cross-provider multi-node splits. See round 17/18/20 notes.
- ✅ **Device plugins** (`device_plugins.rs`, GPU/FPGA/etc. hardware resources) — discovered via the same dynamic plugin-registration protocol CSI drivers use (`plugin_registry.rs`, round 13); tracks live device inventory/health per resource name via `ListAndWatch`, advertises healthy counts on `Node.status.capacity`/`.allocatable`, and calls `Allocate()` during container creation to inject the envs/mounts/device-nodes/annotations a plugin returns. **`GetPreferredAllocation`/`PreStartContainer`** (round 21) — a plugin's `DevicePluginOptions` (fetched once via `GetDevicePluginOptions`) says whether either applies; `GetPreferredAllocation`'s response is validated (`is_valid_preferred_allocation()`) before being trusted, falling back to nodelet's own first-healthy-unallocated pick otherwise; `PreStartContainer` is called right after `Allocate()` succeeds when required, a failure there releasing devices and failing the allocation. Unvalidated against a real device plugin — see rounds 14 and 21 notes.
- ✅ **In-place pod vertical scaling** (`resize` subresource, GA 1.33; found in round 39's re-audit, mechanism round 42, status reporting round 43) — new pure `resize_decision()` compares an already-running container's *actual* last-applied resources (reusing `container_resources`, tracked since round 16 for CPU Manager) against the pod spec's *desired* ones, applying a change in-place via the existing `UpdateContainerResources` RPC when `resizePolicy` allows it (default `NotRequired`), or funneling into the existing restart machinery when it demands `RestartContainer`. `containerStatuses[].resources`/`.allocatedResources` (app containers only) and a `PodResizeInProgress` condition are now reported (round 43), computed purely from two new side tables tracking "actually applied" vs. "currently requested" resources. Genuinely automated e2e tests (`kubectl exec` reads the container's own live cgroup file and the reported status fields, before/after `kubectl patch --subresource resize`). **Still open**: `PodResizePending` isn't implemented — nodelet has no admission/node-fitting layer that could ever *defer* a resize, so there's no real state for it to represent (intentional non-goal, documented, not an oversight); no admission-time rejection of a resize that would change a pod's QoS class (same pre-existing no-admission-layer boundary); init/ephemeral containers don't participate in resize at all yet. See rounds 42-43 notes.
- ✅ **`oom_score_adj`** (round 28; found in round 27's re-audit) — `linux_resources()` now sets CRI's `LinuxContainerResources.oom_score_adj` per container via `eviction::oom_score_adj()`: Guaranteed `-998`, BestEffort `1000`, Burstable scaled by that container's own memory *request* against node capacity (`1000 - 1000*request/capacity`, clamped `[2, 999]`), matching real kubelet's formula exactly. Gives the kernel OOM killer QoS-aware signal, closing a real gap in this project's own eviction-manager story (rounds 7, 26) — a kernel OOM kill can happen faster than `eviction_loop()`'s check interval reacts. See round 28 notes.
- ✅ **gRPC probes** (round 29; found in round 27's re-audit) — `probes.rs::probe_check()` now handles `probe.grpc` too, via a vendored `grpc.health.v1` client (`proto/health.proto`) calling the standard `Health/Check` RPC. `cri`-gated (needs `tonic`'s transport); a mock-only build's `check_grpc()` always returns `false`. Unit-tested failure paths (timeout, refused, non-gRPC listener) with solid confidence; the success path (a real passing `Health/Check`) is unvalidated — no gRPC server available in this sandbox to prove it live. See round 29 notes.
- ✅ **Local ephemeral storage** (found in round 45's re-audit; closed rounds 48-49) — `Node.status.capacity`/`.allocatable["ephemeral-storage"]` reports the real total filesystem size backing `disk_path` (round 48, reusing `DiskPressure`'s existing `statvfs(2)` read). A pod exceeding its *own* `resources.limits["ephemeral-storage"]` is now evicted directly (round 49) — usage measured as CRI's per-container `writable_layer.used_bytes` plus a recursive walk of nodelet's own materialized volume directory, checked independently of and ahead of the general `MemoryPressure`/`DiskPressure`/`PIDPressure`-gated eviction path (the same relationship an individual container's own OOM kill has to overall node memory pressure). Genuinely automated e2e test — the only one of this project's eviction tests that doesn't need a manual procedure, since this trigger needs no artificial node-wide pressure. **Known scope limitation**: usage measurement doesn't include container log file size (`/var/log/pods/...`) yet, only volumes + writable layer. See rounds 48-49 notes.
- ✅ **HugePages** (found in round 58's re-audit; all 3 pieces closed rounds 59-61) — container `resources.limits["hugepages-<size>"]` is translated to CRI's `LinuxContainerResources.hugepage_limits` (round 59), `Node.status.capacity`/`.allocatable["hugepages-<size>"]` report every reserved hugepage pool read from `/sys/kernel/mm/hugepages/` (round 60), and `emptyDir.medium: "HugePages"`/`"HugePages-<size>"` volumes are now real `hugetlbfs` mounts (round 61).
- ✅ **`securityContext.supplementalGroupsPolicy`** (GA 1.33; found in round 58's re-audit; closed round 62) — `Merge`/`Strict` now translates directly to CRI's own `SupplementalGroupsPolicy` enum on both the sandbox- and container-level security context; genuinely automated e2e test proves `Strict` excludes image-defined `/etc/group` membership while `Merge` includes it, using a portable ConfigMap-subPath-supplied `/etc/passwd`/`/etc/group` rather than depending on `$TEST_IMAGE`'s own baked-in group file.
- 🟡 **Dynamic Resource Allocation** (`spec.resourceClaims`; found in round 58's re-audit; closed round 63, gating + batching closed round 64) — kubelet's actual responsibilities are implemented: resolving a pod's `resourceClaims` to their `ResourceClaim` objects' `status.allocation`, gating on `status.reservedFor` actually listing this pod (round 64 — real kubelet's own safety check; `reservedFor` itself is scheduler-written, not kubelet-written, correcting round 63's docs), calling the owning driver(s)' `NodePrepareResources`/`NodeUnprepareResources` once per driver per pod covering every claim it owns (round 64 batching; new `dra.rs`, reusing the plugin-registration infrastructure with a new `"DRAPlugin"` type), and wiring the returned CDI device IDs into each container's CRI `ContainerConfig.cdi_devices` per its `resources.claims[].{name, request}` entries. **Known scope limitations**: `proto/draplugin.proto` was reconstructed from public documentation rather than vendored from upstream, so its exact wire format is unverified against a live third-party driver; no genuinely automated e2e test exists (needs a real DRA driver binary this bash-only harness can't stand up — see `dra.sh`'s manual-spot-check notes, same honest-limitation pattern as `device_plugins.sh`).
- ✅ **Swap support (`memorySwap.swapBehavior`)** (GA 1.34; found in round 65's fresh gap re-audit; closed round 68) — `NODELET_MEMORY_SWAP_BEHAVIOR` (`NoSwap` default / `LimitedSwap`) drives CRI's native `LinuxContainerResources.memory_swap_limit_in_bytes` via new `container_swap_limit_bytes()`, implementing upstream's KEP-2400 formula exactly for `LimitedSwap` (proportional share for Burstable-shaped containers only, an emergent property of the per-container `request == limit` / no-request checks rather than a separate QoS lookup). **Known scope limitation**: no `--system-reserved`/`--kube-reserved`-equivalent knob for swap, so the node's full raw `SwapTotal` is used with nothing withheld.
- ❌ **PodResources API** (found in round 72's re-audit) — kubelet's own gRPC service on `/var/lib/kubelet/pod-resources/kubelet.sock` (`List`/`GetAllocatableResources`/`Watch`, `k8s.io/kubelet/pkg/apis/podresources`) exposing which CPUs/devices/NUMA nodes are assigned to which running container — not implemented at all. Real-world consumers: NVIDIA DCGM and other device-monitoring exporters query this socket directly rather than guessing allocation from `Node.status`. Nodelet already has every piece of the underlying state (`cpu_manager.rs`/`memory_manager.rs`/`device_plugins.rs` all track exactly this), so this would be a read-only projection of existing state onto a new gRPC surface, not new allocation logic.
- ❌ **`allocatedResourcesStatus` / ResourceHealthStatus** (`containerStatuses[].allocatedResourcesStatus`; found in round 72's re-audit; alpha/beta KEP-4680, `ResourceHealthStatus` feature gate) — per-device health (Healthy/Unhealthy/Unknown) isn't reported back into `PodStatus` at all; `device_plugins.rs` already tracks per-device health from `ListAndWatch` for capacity/allocatable purposes, but a device going unhealthy *after* allocation to a running container currently has no pod-visible signal at all. Lower priority than PodResources API — still an alpha/beta upstream feature, not GA.

### Security context
- ✅ **`securityContext`** — `runAsUser`/`runAsGroup`, capabilities add/drop, `privileged`, `readOnlyRootFilesystem`, `allowPrivilegeEscalation`→`no_new_privs`, `supplementalGroups`, `seccompProfile` (`linux_security_context()`). Not yet: `runAsNonRoot` verification against the image's actual user, AppArmor profile, SELinux options.
- ✅ **`securityContext.sysctls`** (round 41; found in round 39's re-audit) — new pure `pod_sysctls()` flattens `spec.securityContext.sysctls` into CRI's `LinuxPodSandboxConfig.sysctls` map, threaded through `sandbox_config()`/`ensure_pod()` alongside the existing hostname resolution. No admission-time allowlisting of "safe" (namespaced) vs. unsafe (host-wide) sysctls — that's the apiserver's job upstream and nodelet has no admission layer at all; an unsupported sysctl simply surfaces as a real `RunPodSandbox` error from the CRI runtime. Genuinely automated e2e test (a real container's own `/proc/sys` read). See round 41 notes.
- ✅ **`hostPID`/`hostIPC`/`shareProcessNamespace`** (round 40; found in round 39's re-audit) — new pure `pid_namespace_mode(host_pid, share_process_namespace) -> NamespaceMode` (`hostPID` wins → `Node`, else `shareProcessNamespace` → `Pod`, else `Container`) is now always applied, on both `sandbox_config()`'s `namespace_options.pid` and each container's own `linux_security_context()`'s `namespace_options.pid` (mirroring real kubelet setting it in both places). **Correctness fix, not just a missing feature**: CRI's own proto comment on `NamespaceOption.pid` says *"the CRI default is POD, but the v1.PodSpec default is CONTAINER"* — before this round nodelet never set the field at all, so every container was silently getting containerd's own POD-shared default, the **opposite** of real Kubernetes' actual default. `hostIPC` sets `namespace_options.ipc` to `Node` (else `Pod` — IPC has no `CONTAINER`-scope concept in the API at all, unlike PID). Genuinely automated e2e tests for `hostPID`/`shareProcessNamespace` (a container's own pid, structural proof); `hostIPC` is unit-tested only — no simple portable shell-level IPC probe in a minimal image, a documented scope limitation. See round 40 notes.
- ✅ **User namespaces** (`spec.hostUsers: false`, round 25; found in round 22's re-audit) — `userns.rs`'s `UsernsAllocator` gives each such pod an exclusive host UID/GID range (fixed length, default 65536, configurable via `NODELET_USERNS_BASE_UID`/`_LENGTH`/`_MAX_PODS`), set as a `POD`-mode `UserNamespace` on `LinuxSandboxSecurityContext.namespace_options.userns_options`. Simplified vs. upstream's own variable-length `usernsManager`: every pod gets the same fixed range size, and allocation state is in-memory only (self-heals as still-running pods reconcile, narrow double-allocation window across a nodelet restart — documented, not hidden). Unvalidated against a real CRI runtime's actual `userns_options` wire support — see round 25 notes.
- ✅ **`fsGroup` volume ownership application** — recursive chown + setgid on every volume directory nodelet itself materializes (`apply_fs_group()`). Only reaches those (ConfigMap/Secret/emptyDir/downwardAPI/projected) — there's no real PV/hostPath for it to reach beyond that yet.
- ✅ **RuntimeClass** — `spec.runtimeClassName` resolves the cluster-scoped `RuntimeClass` object and passes its `.handler` through as CRI's `runtime_handler` (`resolve_runtime_handler()`), so gVisor/Kata/etc. selection actually reaches the runtime. `Overhead.podFixed` now also accounted: converted to `LinuxContainerResources` (`resource_list_to_linux_resources()`) and set on `LinuxPodSandboxConfig.overhead`, closed alongside round 11's cgroup work since it's the same struct/code path. A missing/invalid RuntimeClass still isn't rejected at admission (falls back to the default handler with a warning instead, since nodelet can't enforce the validation a real cluster's admission plugin normally would).
- ✅ **`Node.status.runtimeHandlers`** (round 53; found in round 50's re-audit) — new `PodRuntime::runtime_handlers()` calls CRI's runtime-level `Status` RPC once (never called anywhere in this codebase before this round) and maps the discovered handlers through to `Node.status.runtimeHandlers`, via the same `images`/`mounted_csi_volumes`-shaped plumbing rounds 33/34 already established. Genuinely automated e2e test (asserts at least one handler is present; skips rather than fails if a given containerd version's `Status` RPC doesn't populate the list at all). See round 53 notes.

### Networking
- ✅ **DNS config** — `dnsPolicy`/`dnsConfig` → CRI `PodSandboxConfig.dns_config` (`dns_config_for()`), via new `NODELET_CLUSTER_DNS`/`NODELET_CLUSTER_DOMAIN`
- ✅ **`hostAliases`** — generates a pod-specific `/etc/hosts` (`write_etc_hosts()`) and bind-mounts it in, exactly how real kubelet does it (CRI has no dedicated field for this)
- ✅ Service/ClusterIP/NodePort routing (nftables — pre-existing, kube-proxy's job but already reimplemented here)
- ✅ **`spec.hostname`/`spec.subdomain`/`setHostnameAsFQDN`** (round 38; found in round 35's re-audit) — new pure `resolve_pod_hostname()` mirrors real kubelet's `GeneratePodHostNameAndDomain`/`ShouldSetHostnameAsFQDN`: `spec.hostname` overrides the short hostname (default the pod name); `setHostnameAsFQDN` (only meaningful with `spec.subdomain` also set) makes the sandbox's actual hostname the full `<hostname>.<subdomain>.<namespace>.svc.<cluster-domain>` FQDN instead of just the short name, rejecting (`Err`, same retry-and-report path as any other `ensure_pod()` failure) an FQDN over Linux's 64-byte `sethostname(2)` limit rather than silently truncating it. Genuinely automated e2e tests (a real container's own `hostname` output). See round 38 notes.

### Images
- ✅ **Private registry auth** — `imagePullSecrets` (`kubernetes.io/dockerconfigjson`) → CRI `AuthConfig` (`resolve_pull_auth()`), tried first; falling back to an **image credential provider** (round 71; found in round 69's re-audit) — `--image-credential-provider-config`/`-bin-dir` exec-plugin protocol (`credential_provider.rs`), including ServiceAccount token integration (`tokenAttributes`, beta/default-on in k8s 1.34) reusing the same `TokenRequest` machinery projected `serviceAccountToken` volumes use. Not yet: legacy `kubernetes.io/dockercfg`, ServiceAccount-linked pull secrets, multi-provider merge (only the first matching provider is tried).
- ✅ **Image garbage collection** (found in round 69's fresh gap re-audit; closed round 70) — real kubelet's disk-pressure-triggered high/low watermark policy: `NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT`/`_LOW_THRESHOLD_PERCENT`/`_MIN_AGE_SECS` (defaults 85/80/120s, matching `--image-gc-high-threshold`/`-low-threshold`/`--image-minimum-gc-age`) drive new pure `should_start_image_gc()`/`images_to_reclaim_space()` — an unreferenced image is left alone entirely until disk usage crosses the high threshold, then removed oldest-unreferenced-first down to the low threshold. Replaces the previous unconditional every-cycle sweep — a real behavior change, not just an addition.
- ✅ **Container log rotation** — running containers' log files are rotated past `NODELET_CONTAINER_LOG_MAX_SIZE_BYTES`, keeping `NODELET_CONTAINER_LOG_MAX_FILES` (`rotate_log_file()` + CRI `ReopenContainerLog`)
- ✅ **`Node.status.images`** (round 33; found in round 27's re-audit) — `node.rs::select_node_images()` sorts CRI's `ListImages` results (via the new `PodRuntime::node_images()` trait method) largest-first and caps at 50, matching real kubelet's own `--node-status-max-images` default. Genuinely automated e2e test. See round 33 notes.
- ✅ **`imagePullPolicy` enforcement** (round 51; found in round 50's re-audit) — new pure `effective_pull_policy()` (an explicit policy always wins; otherwise real kubelet's own default heuristic — `Always` for untagged/`:latest`, `IfNotPresent` for a specific tag *or* a digest reference) plus a new `ImageStatus` RPC call (vendored, never used before) to check local presence whenever the policy isn't `Always`. `Never` without local presence now `bail!`s a real error (surfaces via `ensure_pod()`'s existing retry-and-report path) instead of silently pulling anyway; `IfNotPresent` skips `PullImage` entirely when already cached. Genuinely automated e2e test for `Never`'s refusal behavior (a real, valid image/tag this suite never otherwise pulls — a fake tag wouldn't distinguish "correctly refused" from "ignored but failed anyway upstream"); `IfNotPresent`'s registry-round-trip-skipping is unit-tested only (needs nodelet's own logs to observe live). See round 51 notes.
- ✅ **`ContainerStatus.imageID`** (round 52; found in round 50's re-audit) — `ContainerRuntimeStatus` gained `image_id`, populated directly from CRI's own `Container.image_ref` (already fetched every reconcile by `list_pod_containers()`, just never read) — a "read-an-already-available-field" fix, no new RPC or plumbing. Lets a user confirm exactly which image digest is running, catching `:latest`-tag drift. Genuinely automated e2e test. See round 52 notes.

### Volumes
- ✅ ConfigMap / Secret / emptyDir (materialized to host paths)
- ✅ **ConfigMap/Secret live-update** (round 37; found in round 35's re-audit) — `PodController::run()` now watches ConfigMaps/Secrets cluster-wide (they have no node-scoping fieldSelector) alongside the existing node-scoped Pod watch; on a change, `referenced_configmap_names()`/`referenced_secret_names()` (pure) find every pod on this node whose volumes reference the changed object (direct or via `projected` sources) and re-`reconcile()`s them, reusing the existing idempotent `resolve_volumes()` materialization path to overwrite the bind-mounted host file content in place — no pod/container restart. Deliberately does NOT cover `envFrom`/`valueFrom.configMapKeyRef`/`secretKeyRef` — real kubelet captures those once at container start too. Genuinely automated e2e test proves the content updates AND the container's own restart count stays 0. See round 37 notes.
- ✅ **`valueFrom.resourceFieldRef` in container env vars** (round 44; found in round 35's re-audit) — a distinct code path from the still-open downwardAPI-volume `resourceFieldRef` gap below: new pure `resolve_resource_field_ref()`/`format_resource_field_value()` resolve `limits.cpu`/`limits.memory`/`requests.cpu`/`requests.memory` (falling back to the node's own capacity when the container has no limit set, matching real kubelet's documented Downward API behavior, then to the container's own limit for `requests.*` before that), reproducing kubelet's well-known "CPU reports whole cores, rounded up" default-divisor quirk and the common JVM-heap-sizing memory-divisor pattern with one shared ceiling-division formula. `ephemeral-storage` resolves to `"0"` (not tracked/enforced by nodelet at all — separate pre-existing gap) rather than bailing. Genuinely automated e2e test (`kubectl exec` reads the real env var values). See round 44 notes.
- ✅ **Projected volumes** — `configMap`/`secret`/`downwardAPI`, and now `serviceAccountToken` too (mints a real token via the `TokenRequest` API — `resolve_service_account_token()`; needs nodelet's client to have `create` on `serviceaccounts/token` in the namespace, a real RBAC requirement) merge into the volume dir, with `items`/`KeyToPath` key-selection-and-rename support. `clusterTrustBundle` sources are still skipped with a warning.
- ✅ **downwardAPI volumes** (`write_downward_api_volume()`, `fieldRef` only — `resourceFieldRef` needs the resolved container spec and isn't supported)
- 🟡 **PersistentVolumeClaim / CSI** (`runtime/csi.rs`, `plugin_registry.rs`) — resolves a bound PVC's `PersistentVolume.spec.csi` source and drives `NodeStageVolume` (if the driver supports it)/`NodePublishVolume`, with per-node reference counting so `NodeUnstageVolume` only fires once every pod using a volume is gone. **Dynamic CSI driver discovery** (round 13) — a driver's `node-driver-registrar` sidecar can register itself against `NODELET_PLUGIN_REGISTRY_PATH` the same protocol it'd use against real kubelet's plugin watcher, no static config needed; `NODELET_CSI_DRIVERS` still works too, as a seed/override. **`nodeStageSecretRef`/`nodePublishSecretRef`** (round 13) — resolved to real Secret data and passed through to the driver. **Attach coordination** (round 19) — checks `CSIDriver.spec.attachRequired`, and for drivers that need it, waits on the matching `VolumeAttachment.status.attached` before Stage/Publish, threading `status.attachmentMetadata` through as `publish_context`. Calling the Controller service itself (`ControllerPublishVolume`/`ControllerUnpublishVolume`) stays out of scope — that's external-attacher's job upstream too, not kubelet's, confirmed against docs in round 19. Still out of scope: device-plugin registrations against this module (the same registration protocol, explicitly rejected with a real `NotifyRegistrationStatus{plugin_registered: false}` rather than ignored — that's `device_plugins.rs`'s job instead). Unvalidated against a real CSI driver — see rounds 12, 13, and 19 notes.
- ✅ **hostPath** (found in a fresh gap re-audit; closed round 65) — `spec.volumes[].hostPath` resolves directly to the host's own existing path (not materialized under `VOLUME_ROOT` like every other volume kind), with real `type` validation (`DirectoryOrCreate`/`FileOrCreate`/`Directory`/`File`/`Socket`/`CharDevice`/`BlockDevice`) matching real kubelet's own create-vs-require-existing semantics exactly. A validation failure is logged and the volume skipped, same best-effort posture as every other unresolvable volume kind.
- ✅ **`emptyDir.sizeLimit` enforcement** (found in round 65's fresh gap re-audit; closed round 67) — a plain-disk `emptyDir` volume exceeding its own `sizeLimit` now evicts the pod, checked independently of (and ahead of) both the whole-pod ephemeral-storage limit (round 49) and general node-pressure eviction — new `PodUsage.empty_dir_usage_bytes` (per-volume, same `directory_usage_bytes()` walk round 49 already uses) plus new pure `empty_dir_size_limits()`/`first_empty_dir_over_limit()` in `eviction.rs`. Scoped to plain-disk `emptyDir` only — `Memory`/`HugePages`-medium `sizeLimit` is already a real kernel-enforced cap at mount time (rounds 30/61), nothing for measurement-based eviction to add there.
- ✅ **subPath `$(VAR)` expansion** (found in round 69's fresh gap re-audit; closed round 69) — `volumeMounts[].subPathExpr` now expands `$(VAR)` references against the container's own resolved env vars (new `expand_sub_path_expr()`), applying to both `HostPath`- and image-backed volumes. A reference to an unset/unclosed variable drops the mount entirely rather than substituting a garbage path, matching real kubelet's own posture.
- 🟡 **CSI ephemeral (inline) volumes** (round 46; found in round 45's re-audit) — `volumes[].csi` specified directly (not via a PVC or the generic `ephemeral` templated form, round 31), via new `resolve_csi_ephemeral_source()` and a synthetic `csi_ephemeral_volume_handle()` (`"<pod_uid>-<volume_name>"`, since there's no PV/PVC to derive one from). `CsiDrivers::mount()`/`unmount()` gained an `ephemeral: bool` param that skips `NodeStageVolume`/`NodeUnstageVolume` and any attach concept entirely, regardless of what the driver otherwise reports supporting — the CSI spec itself says neither applies to the inline form. Reuses all of the CSI Node-service plumbing built in rounds 12/13/19 as-is. Genuinely automated e2e test, gated behind a `TEST_CSI_INLINE_DRIVER` env var (same pattern as the PVC path's `TEST_CSI_STORAGE_CLASS`) since it needs a real driver. Unvalidated against a real CSI driver — same caveat every prior CSI round has carried. See round 46 notes.
- ✅ **`emptyDir.medium: Memory`** (round 30; found in round 27's re-audit) — `resolve_volumes()` now shells out to `mount -t tmpfs` (`mount_tmpfs_empty_dir()`) on the host directory for a `Memory`-medium `emptyDir`, honoring `sizeLimit` as `-o size=` when set. `remove_pod()` unmounts it again on teardown (a real RAM leak otherwise, unlike plain-disk `emptyDir`). Best-effort: falls back to the plain-disk directory (already created) on mount failure rather than failing the pod. Unvalidated against a real privileged mount in this sandbox — see round 30 notes.
- ✅ **Generic ephemeral volumes** (round 31; `volumeSource.ephemeral`, found in round 27's re-audit) — `resolve_volumes()` now recognizes `.ephemeral`, resolving the deterministic-named (`<pod name>-<volume name>`) PVC the ephemeral-volume controller (a `kube-controller-manager` component — not nodelet's job, same as dynamic provisioning) auto-creates, with an ownership safety check (by UID) before trusting it, then reuses all of CSI's existing mount machinery. Unvalidated against a real CSI driver/ephemeral-volume controller — see round 31 notes.
- ✅ **Image volume source** (round 32; `volumeSource.image`, KEP-4639, still beta, found in round 27's re-audit) — `resolve_volumes()`/`build_mounts()` use CRI's native `Mount.image`/`image_sub_path` fields directly (no host-path materialization needed, unlike every other volume kind) after a `PullImage` call resolves the reference. Always read-only, per the KEP. Genuinely automated e2e test (no external StorageClass/CSI driver needed — any pullable image works). See round 32 notes.
- 🟡 **`Node.status.volumesInUse`/`.volumesAttached`** (round 34; found in round 27's re-audit) — `CsiDrivers::mounted_volumes()` exposes the mount reference-counting round 12 already tracked; `node.rs::csi_unique_volume_name()` builds real kubelet's `kubernetes.io/csi/<driver>^<volume_handle>` naming. Scoped to CSI volumes only (this project's actual PVC story, rounds 12/13/19); CSI's own attach coordination (round 19) doesn't read these fields itself. **Deliberately lower-confidence by design**: whether a real attach/detach controller is satisfied by this is unvalidated, not just unvalidated by sandbox limitation. See round 34 notes.

### Node-pressure eviction
- ✅ MemoryPressure/DiskPressure *conditions* reflect real reads
- 🟡 **Eviction** — `eviction_loop()` now acts on real pressure: ranks eligible pods by QoS class (`eviction.rs`'s `qos_class()`/`pick_eviction_candidate()` — BestEffort before Burstable, Guaranteed and `system-*-critical` pods never evicted), evicts one per check. Ranking within a QoS class now uses **`spec.priority`** (round 26 — lower priority evicted first, matching real kubelet's own ordering; read directly off the Pod object since the apiserver's Priority admission controller already resolves `priorityClassName` into it, no lookup needed) **before** falling back to real memory usage from CRI's `ListPodSandboxStats` (the same source `/stats/summary` uses) when known, or requested memory otherwise (`eviction_weight()`). Still simplified vs. real kubelet: no soft-threshold grace period (hard-style immediate action only), and doesn't implement the "exceeds requests" step of upstream's comparator chain (round 26 only added the priority step).
- ✅ PID pressure — real `/proc/sys/kernel/pid_max` + a `/proc` scan (`read_pid_info()`/`pid_pressure()`), same fail-open pattern as memory/disk

### Static pods & mirror pods
- ✅ **Static pod manifest directory watching** (`NODELET_STATIC_POD_PATH`, disabled by default like real kubelet's optional `staticPodPath`) — `static_pods::run()` scans, hashes to detect changes, and drives the same `PodRuntime` normal pods use (so resource limits/securityContext/volumes/probes all apply identically)
- ✅ **Mirror pod creation/reconciliation** — a read-only Pod object per static pod (`kubernetes.io/config.mirror`/`kubernetes.io/config.source: file` annotations, matching real kubelet's markers), deleted when the manifest disappears. Simplified vs. real kubelet: no exact hash-based drift-detection annotation value (nodelet's own file-content hash serves the same "did it change" purpose internally, just isn't exposed as that specific annotation).

### kubelet HTTP(S) server (`crates/nodelet/src/server/`, `cri` feature only)
- ✅ **`kubectl logs`** (`server::logs`) — parses containerd's CRI log file format back into raw output, with `follow`/`tailLines`/`sinceTime`/`timestamps`/`previous` query params. `follow` mode polls the file for growth rather than using inotify (matches the poll-based style everywhere else in nodelet — probes.rs, gc.rs).
- ✅ **Streaming exec/attach/port-forward** (`server::exec`) — CRI's actual model here: `Exec`/`Attach`/`PortForward` RPCs return a one-shot URL to the *runtime's own* streaming server (containerd runs one internally, typically on `127.0.0.1:<random-port>` — unreachable to a remote kubectl client directly). nodelet doesn't implement the SPDY/WebSocket protocol itself; it dials that URL, replays the client's upgrade request, mirrors the response, and once both sides upgrade, splices the two raw connections together (`tokio::io::copy_bidirectional`) — the same "proxy" pattern real kubelet uses. **This is the piece with the least confidence without a live cluster**: the request/response replay and connection splicing were written as carefully as reasoning allows, but an actual SPDY/WebSocket handshake end-to-end was never observed — `deploy/lib/test/cases/streaming.sh` exists specifically to prove (or disprove) this for real.
- ✅ TLS serving certificate — self-signed, generated on first start via `rcgen` and cached as raw DER under `NODELET_SERVER_CERT_DIR` (persists across restarts so a client that already trusts it doesn't get invalidated). Not yet: CSR-based issuance against a real cluster CA.
- ✅ Bearer token authentication via `TokenReview` (the same mechanism real kubelet's `--authentication-token-webhook` uses). Authorization is deliberately `AlwaysAllow` once a token authenticates — matches real kubelet's own historical default (`--authorization-mode=AlwaysAllow`), not a from-scratch `SubjectAccessReview` implementation. No anonymous access (real kubelet has historically defaulted to allowing it; nodelet doesn't).
- ✅ `Node.status.daemonEndpoints.kubeletEndpoint.port` now advertised (was never set before — without it the apiserver has no route to proxy exec/logs/attach/port-forward requests to at all, regardless of whether a server is listening).
- ✅ **`/stats/summary`** (`server::stats`) — built from CRI's `ListPodSandboxStats` (one call gets per-pod *and* per-container CPU/memory usage, no cgroup-path guessing needed). Real caveat, not a nodelet limitation: `kubectl top` itself needs metrics-server (or another `metrics.k8s.io` implementation) deployed and configured to scrape this — implementing the endpoint is necessary but not sufficient for `kubectl top` on its own. Node-level CPU usage isn't populated in this endpoint's JSON shape (unlike `/metrics/resource` below, which does report it) — only memory comes from `/proc/meminfo` here.
- ✅ **`/metrics/resource`** (`server::prom_metrics`) — full [KEP-2371](https://github.com/kubernetes/enhancements/tree/master/keps/sig-node/2371-cri-pod-container-stats) metric set, including real node-wide CPU usage from a new `/proc/stat` parser (`metrics.rs::read_node_cpu_seconds`).
- 🟡 **`/metrics/cadvisor`** (`server::prom_metrics`) — a deliberately scoped-down subset of real cAdvisor's much larger legacy catalog: `container_cpu_usage_seconds_total`, `container_memory_usage_bytes`, `container_memory_working_set_bytes`, `container_memory_rss`, labeled `{namespace,pod,container}` only (no `id`/`name`/`image` — not tracked in `PodUsage`). Missing: network/disk I/O, per-cpu-core breakdowns, `container_last_seen`, spec/limit metrics.
- ❌ Client certificate authentication (bearer token only)
- ❌ **Checkpoint API** (`/checkpoint/{namespace}/{pod}/{container}`) (found in round 22's re-audit) — a forensic/debugging endpoint (CRIU-based container checkpointing, still alpha upstream) not implemented at all. Low value for nodelet's edge-device target and CRIU itself is a real external dependency (kernel + userspace tooling) beyond anything else this project needs — noted here rather than silently missing, not currently recommended for implementation.

### Node shutdown
- ✅ **Graceful node shutdown** (`shutdown.rs`) — a systemd-logind shutdown-delay inhibitor lock held via D-Bus, released once every pod's been driven through the normal `preStop`/`StopContainer` teardown path within a configurable time budget (`NODELET_SHUTDOWN_GRACE_PERIOD_SECS`, `0`/disabled by default matching upstream). Non-critical pods terminated first, `system-node-critical`/`system-cluster-critical` pods last, each pod's own `terminationGracePeriodSeconds` capped to whatever's left of the budget. **The D-Bus glue is unvalidated against a real systemd-logind** — no system bus in the environment that built it; see the round 9 notes below and `deploy/lib/test/cases/graceful_shutdown.sh`'s manual spot-check procedure.

### Bootstrapping / config
- ❌ TLS bootstrap (CSR-based initial client cert issuance) — nodelet currently expects to be handed a working kubeconfig directly; lower priority given the project's already-simplified config philosophy, but a real gap if "100%" includes it.
- ❌ `--config` file / drop-in config directory (nodelet uses env vars only) — same caveat as above.

## Scale reality check

The items marked ❌ above are, in aggregate, most of what a real kubelet is —
this is not a "few features," it's multiple person-months of work in upstream
Kubernetes (cgroup management, a TLS-authenticated streaming server, a CSI
client, an eviction manager, security-context translation...). Some are far
higher-value/correctness-critical than others:

- **Correctness-critical, silently wrong today**: resource limits not
  enforced, security context ignored, init containers skipped, DNS not
  configured, private images unpullable. These mean a pod spec that *looks*
  correct produces materially different (and less safe) behavior than on
  real Kubernetes.
- **Missing features, fails loudly/obviously**: `kubectl exec`/`logs`/`top`,
  static pods, PVC/CSI, RuntimeClass.
- **Advanced/opt-in on real clusters too**: CPU/Memory/Topology managers,
  device plugins, in-place resize — most real clusters don't enable these
  either.

## Progress on the original 3-gap pass (completed, commit `fdf003b`)
- [x] Probes (liveness/readiness/startup)
- [x] Pressure metrics (real MemoryPressure/DiskPressure)
- [x] GC (orphaned sandboxes + unreferenced images)

## Progress on full-parity pass (this rescoping)
- [x] Verify scope boundary against kubernetes.io docs
- [x] Comprehensive gap list (this doc)
- [x] Round 2 (user-chosen "correctness gaps first"): init containers,
      resource limits, securityContext, DNS config, private registry auth
- [x] Round 3: termination grace + preStop/postStart hooks, exit-code-aware
      phase, real restart counts, projected/downwardAPI volumes,
      hostAliases, fsGroup, node-pressure eviction
- [x] Round 4: real PID pressure, container log rotation, static/mirror
      pods, serviceAccountToken minting via TokenRequest
- [x] Round 5: live-cluster e2e bash test suite (deploy/test-e2e.sh) +
      initContainerStatuses/Initialized condition fix
- [x] Round 6: kubelet HTTP(S) server — kubectl logs/exec/attach/
      port-forward, TLS, TokenReview auth, daemonEndpoints advertisement.
      **Needs live-cluster validation** — see streaming.sh and this round's
      confidence note above, especially for kubectl exec.
- [x] Round 7: `/stats/summary`, usage-based eviction ranking, RuntimeClass
- [x] Round 8: ephemeral containers (`kubectl debug`)
- [x] Round 9: graceful node shutdown (systemd-logind inhibitor lock,
      unvalidated against a real logind — see the confidence note above)
- [x] Round 10: `/metrics/resource` (complete) + `/metrics/cadvisor`
      (scoped-down subset — see round 10 notes)
- [x] Round 11: cgroup/QoS hierarchy + node allocatable enforcement +
      RuntimeClass Overhead (user-picked over PVC/CSI and the CPU/Memory/
      Topology managers — see round 11 notes for the cgroup-write
      confidence caveat)
- [x] Round 12: PersistentVolumeClaim/CSI, first slice (static driver
      config, no Controller service — see round 12 notes; unvalidated
      against a real CSI driver, same confidence caveat pattern as rounds
      9 and 11)
- [x] Round 13: dynamic CSI driver discovery (plugin_registry.rs) +
      nodeStageSecretRef/nodePublishSecretRef — see round 13 notes, same
      unvalidated-against-a-real-driver caveat as round 12
- [x] Round 14: device plugins (device_plugins.rs) — discovery,
      Node.status.capacity/allocatable advertisement, Allocate() wired into
      container creation. See round 14 notes; same unvalidated-against-
      real-hardware caveat as rounds 12/13's driver-dependent pieces.
- [x] Round 15: CPU Manager static policy (cpu_manager.rs) — first slice,
      no retroactive UpdateContainerResources sweep, no topology/socket
      awareness. See round 15 notes.
- [x] Round 16: CPU Manager retroactive shared-pool updates
      (refresh_shared_pool_cpusets()) — closes round 15's biggest flagged
      gap; not topology/socket-aware yet. See round 16 notes.
- [x] Round 17: Topology Manager (topology.rs) — single-node-only
      alignment algorithm, coordinates CPU Manager + device plugins; no
      Memory Manager to also coordinate. See round 17 notes.
- [x] Round 18: Memory Manager (memory_manager.rs) — single-node-only
      pinning, no shared-pool retroactive tracking, no per-node reserved-
      memory. CPU/Memory/Topology manager thread now closed. See round 18
      notes.
- [x] Round 19: CSI attach coordination — `CSIDriver.spec.attachRequired`
      check + waiting on the matching `VolumeAttachment.status.attached`
      before Stage/Publish, threading `status.attachmentMetadata` through
      as `publish_context`. Calling the Controller service itself
      (`ControllerPublishVolume`/`ControllerUnpublishVolume`) stays out of
      scope — confirmed against docs that's external-attacher's job, not
      kubelet's. See round 19 notes.
- [x] Round 20: Topology Manager `restricted` multi-node spread —
      `topology::spread()` places each hint provider on its own best NUMA
      node independently when no single node satisfies everyone, instead
      of `restricted` behaving identically to `single-numa-node`'s strict
      single-node reject. Not upstream's exact joint-hint combination
      search — a real, honestly-scoped bounded relaxation. See round 20
      notes.
- [x] Round 21: device plugin `GetPreferredAllocation`/`PreStartContainer`
      — a plugin's `DevicePluginOptions` decides whether either applies;
      the preferred-allocation response is validated before use, falling
      back to nodelet's own selection otherwise. Closes the last item
      explicitly tracked on this list since round 14. See round 21 notes.
- [x] Round 22: fresh gap re-audit (no code change) — cross-referenced
      the kubelet CLI reference against the codebase and found 5
      previously-untracked items: `terminationMessagePath`/`Policy` not
      read back, pod `readinessGates` unimplemented, user namespaces
      (`hostUsers`) unimplemented, eviction's priority-tiebreak gap, and
      the (not recommended) checkpoint API. See round 22 notes.
- [x] Round 23: pod `readinessGates` — `Ready` now also requires every
      gate's named condition to be `True`; also fixed a real pre-existing
      bug along the way (JSON-Merge-Patch status writes were silently
      deleting any condition an external controller had set — now carried
      forward). Genuinely automatable e2e test, no real infra needed. See
      round 23 notes.
- [x] Round 24: `terminationMessagePath`/`terminationMessagePolicy`
      read-back — also fixed a bigger pre-existing gap along the way:
      regular/init containers never reported a real `terminated`
      `ContainerState` at all before this round (always
      `Waiting: ContainerCreating` forever once exited). Two genuinely
      automated e2e tests, no real infra needed. `FallbackToLogsOnError`
      still not implemented (documented, deliberate). See round 24 notes.
- [x] Round 25: user namespaces (`spec.hostUsers: false`) — new
      `userns.rs`'s `UsernsAllocator` gives each such pod an exclusive
      host UID/GID range via CRI's `userns_options`. Fixed-length
      allocator (not upstream's variable-length pool), in-memory state
      (documented, not hidden). Real automated e2e test checks
      `/proc/self/uid_map` inside the container directly. See round 25
      notes.
- [x] Round 26: eviction priority-tiebreaking — `pick_eviction_candidate()`
      now ranks by `spec.priority` (already resolved by the apiserver, no
      `PriorityClass` lookup needed) before falling back to usage,
      matching real kubelet's own `rankMemoryPressure` ordering. Closes
      the last item from round 22's audit. See round 26 notes.
- [x] Round 27: fresh gap re-audit (no code change) — found 7
      previously-untracked items: `oom_score_adj` not set, gRPC probes
      unimplemented, `emptyDir.medium: Memory` ignored, generic ephemeral
      volumes unimplemented, `Node.status.images` not populated,
      `volumesInUse`/`volumesAttached` not populated, image volume
      source unimplemented. See round 27 notes.
- [x] Round 28: `oom_score_adj` — `linux_resources()` now sets CRI's
      per-container `oom_score_adj` from real kubelet's own formula
      (Guaranteed `-998`, BestEffort `1000`, Burstable scaled by request
      vs. node capacity). Two genuinely automated e2e tests (no cgroup-v2
      dependency, unlike most of `resources.sh`). See round 28 notes.
- [x] Round 29: gRPC probes — `probe.grpc` now dials the standard
      `grpc.health.v1.Health/Check` protocol via a vendored client
      (`proto/health.proto`), `cri`-gated. Failure paths unit-tested with
      solid confidence; the success path is unvalidated (no gRPC server
      available in this sandbox). See round 29 notes.
- [x] Round 30: `emptyDir.medium: Memory` — `resolve_volumes()` mounts
      real tmpfs (`mount -t tmpfs`) for a `Memory`-medium `emptyDir`,
      honoring `sizeLimit`; `remove_pod()` unmounts it again on teardown.
      Real automated e2e test checks the host mountpoint's actual
      filesystem type. See round 30 notes.
- [x] Round 31: generic ephemeral volumes — `resolve_volumes()` resolves
      the ephemeral-volume controller's deterministic-named PVC (with an
      ownership safety check), reusing all of CSI's existing mount
      machinery. Unvalidated against a real CSI driver/controller. See
      round 31 notes.
- [x] Round 32: image volume source — CRI's native `Mount.image` field
      used directly (no host-path materialization needed). Genuinely
      automated e2e test, no external infra needed (any pullable image
      works). See round 32 notes.
- [x] Round 33: `Node.status.images` — `select_node_images()` reports
      CRI's cached images, largest-first, capped at 50. Genuinely
      automated e2e test, no external infra needed. See round 33 notes.
- [x] Round 34: `Node.status.volumesInUse`/`.volumesAttached` — scoped to
      CSI volumes only, reusing round 12's existing mount reference-
      counting. Deliberately lower-confidence by design (unvalidated
      against a real attach/detach controller, not just sandbox-limited).
      Closes the last round-27 candidate. See round 34 notes.
- [x] Round 35: fresh gap re-audit (no code change) — found 5
      previously-untracked items: native sidecar containers
      (`initContainers[].restartPolicy: Always`) unimplemented,
      ConfigMap/Secret live-update unimplemented, `spec.hostname`/
      `subdomain`/`setHostnameAsFQDN` not honored, env `resourceFieldRef`
      explicitly unsupported, probe-level `terminationGracePeriodSeconds`
      not applied. See round 35 notes.
- [x] Round 36: native sidecar containers — `sidecar_init_decision()`
      routes `initContainers[].restartPolicy: Always` through its own
      decision matrix (doesn't block on exit, restarts indefinitely, real
      probe-based readiness folds into pod `Ready`). Genuinely automated
      e2e tests. Teardown ordering (sidecars stopped last) is a
      documented simplification. See round 36 notes.
- [x] Round 37: ConfigMap/Secret live-update — two new cluster-wide
      watch streams (ConfigMap/Secret have no node-scoping fieldSelector)
      added to `PodController::run()`'s `select!` loop;
      `referenced_configmap_names()`/`referenced_secret_names()` (pure)
      find affected pods on this node, re-`reconcile()` reuses the
      existing idempotent materialization path. Deliberately excludes
      env var references (matches real kubelet). Genuinely automated
      e2e test, no external infra needed. See round 37 notes.
- [x] Round 38: `spec.hostname`/`subdomain`/`setHostnameAsFQDN` — new
      pure `resolve_pod_hostname()` mirrors real kubelet's hostname/FQDN
      resolution, threaded through `sandbox_config()`/`run_sandbox()`
      into the real `RunPodSandbox` call; rejects (not truncates) an
      FQDN over Linux's 64-byte hostname limit. Genuinely automated e2e
      tests (a real container's own `hostname` output). See round 38
      notes.
- [x] Round 39: fresh gap re-audit (no code change) — found 4
      previously-untracked items: **in-place pod vertical scaling**
      (`resize` subresource, GA 1.33 — highest value, `ensure_container()`
      never compares live resources against the current pod spec at
      all), `hostPID`/`hostIPC` unset, `shareProcessNamespace` unset
      (with a correctness note: nodelet's total silence on
      `NamespaceOption.pid` means containers get containerd's own
      CRI-level POD-shared default, the *opposite* of real Kubernetes'
      CONTAINER-scoped default), and `securityContext.sysctls`
      (CRI has a dedicated field, unread). See round 39 notes.
- [x] Round 40: `hostPID`/`hostIPC`/`shareProcessNamespace` — new pure
      `pid_namespace_mode()` (`hostPID` wins → `Node`, else
      `shareProcessNamespace` → `Pod`, else `Container`) now always
      applied on both the sandbox's and every container's own
      `namespace_options.pid`, fixing the correctness bug round 39 found
      (nodelet was silently relying on containerd's own POD-shared
      default for an unset `pid` field). `hostIPC` → `namespace_options.ipc`.
      Genuinely automated e2e tests for `hostPID`/`shareProcessNamespace`;
      `hostIPC` unit-tested only (documented — no simple portable
      shell-level IPC probe in a minimal image). See round 40 notes.
- [x] Round 41: `securityContext.sysctls` — new pure `pod_sysctls()`
      flattens `spec.securityContext.sysctls` into CRI's
      `LinuxPodSandboxConfig.sysctls` map. No admission-time allowlisting
      (apiserver's job upstream, nodelet has no admission layer at all);
      an unsupported sysctl surfaces as a real `RunPodSandbox` error.
      Genuinely automated e2e test (a real container's own `/proc/sys`
      read). See round 41 notes.
- [x] Round 42: in-place pod vertical scaling, slice 1 — new pure
      `resize_decision()` detects a live-vs-desired resource mismatch on
      an already-running container (reusing the existing
      `container_resources` side table, round 16) and either applies it
      via the existing `UpdateContainerResources` RPC or funnels into
      the existing restart machinery, per `resizePolicy`. Genuinely
      automated e2e test (`kubectl exec` + `--subresource resize`).
      **Deliberately still open** (next slice): `containerStatuses[]
      .resources`/`.allocatedResources` reporting, `PodResizePending`/
      `PodResizeInProgress` conditions. See round 42 notes.
- [x] Round 43: in-place pod vertical scaling, slice 2 (status
      reporting) — `containerStatuses[].resources`/`.allocatedResources`
      (app containers only) and a `PodResizeInProgress` condition, from
      two new side tables tracking "actually applied" vs. "currently
      requested" resources per container. `PodResizePending` deliberately
      not implemented (no admission/deferral layer exists to ever
      produce that state). Genuinely automated e2e test extension. See
      round 43 notes. **This closes the in-place resize arc** (rounds
      42-43) opened by round 39's audit.
- [x] Round 44: env `resourceFieldRef` + probe-level
      `terminationGracePeriodSeconds` — **this closes round 35's audit
      list entirely.** New pure `resolve_resource_field_ref()`/
      `format_resource_field_value()` resolve `limits.*`/`requests.*`
      CPU/memory env references (falling back to node capacity, then
      the container's own limit, matching real kubelet's documented
      behavior), replacing an unconditional `bail!`. New pure
      `probes::probe_grace_period_seconds()` resolves a liveness
      probe's own override (else the pod's), replacing a hardcoded
      `10` in `restart_container()`'s `StopContainer` timeout. Both
      genuinely automated e2e tests. See round 44 notes.
- [x] Round 45: fresh gap re-audit (no code change) — confirmed several
      plausible candidates are already implemented (`Node.status.nodeInfo`,
      all 3 node-pressure conditions, `preStop`'s `sleep` action), and
      found 2 previously-untracked items plus generalized a
      round-44-adjacent detail into its own gap: **CSI ephemeral (inline)
      volumes** (`volumes[].csi` directly — likely low implementation
      cost, the CSI Node-service plumbing already exists for the PVC
      path), **startup probe failure never triggers a restart** (retries
      forever instead of killing/restarting past `failureThreshold`, the
      same way a liveness failure does), and **local ephemeral storage
      isn't tracked anywhere** (capacity/allocatable/requests/limits/
      eviction — round 44's `resolve_resource_field_ref()` `"0"` stub
      was the first hint of this). See round 45 notes.
- [x] Round 46: CSI ephemeral (inline) volumes — `resolve_csi_ephemeral_source()`
      + synthetic `csi_ephemeral_volume_handle()` (no PV/PVC to derive one
      from); `CsiDrivers::mount()`/`unmount()` gained an `ephemeral: bool`
      that correctly skips staging/attach entirely for this volume kind
      (the CSI spec's own rule, not just a driver-capability check).
      Reuses all the existing CSI Node-service plumbing (rounds 12/13/19)
      as-is. Genuinely automated e2e test, gated behind a new
      `TEST_CSI_INLINE_DRIVER` env var. See round 46 notes.
- [x] Round 47: startup probe failure restart — the startup-probe loop
      now checks the new public `ProbeTracker.failures` against
      `failureThreshold`, restarts the container (reusing round 44's
      `probe_grace_period_seconds()` unchanged), and keeps retrying
      against the recreated instance rather than looping forever with
      no restart at all. Genuinely automated e2e test. See round 47
      notes.
- [x] Round 48: local ephemeral storage, slice 1 — new
      `ephemeral_storage_capacity_bytes()` reuses `DiskPressure`'s
      existing `statvfs(2)` read against `disk_path` to populate
      `Node.status.capacity`/`.allocatable["ephemeral-storage"]`
      (`allocatable_map()` needed no change — the key passes through
      untouched, matching `pods`). Genuinely automated e2e assertions.
      **Deliberately still open** (next slice): request/limit
      enforcement and an eviction-manager signal for `nodefs`/`imagefs`
      disk pressure. See round 48 notes.
- [x] Round 49: local ephemeral storage, slice 2 (eviction signal) —
      new `PodUsage.ephemeral_storage_usage_bytes` (CRI writable-layer
      usage + a recursive walk of nodelet's own volume directory),
      `ephemeral_storage_limit_bytes()`/`exceeds_ephemeral_storage_limit()`
      (`eviction.rs`), and a new eviction check in `eviction_loop()` that
      fires independent of general node pressure — a pod exceeding its
      own limit is evicted directly, the same relationship an
      individual OOM kill has to overall node memory pressure.
      Genuinely automated e2e test (no artificial pressure needed).
      **This closes the local-ephemeral-storage arc (rounds 48-49) and,
      with it, round 45's audit list entirely.** See round 49 notes.
- [x] Round 50: fresh gap re-audit (no code change) — found 3
      previously-untracked items: **`imagePullPolicy` is completely
      unenforced** (`PullImage` always called unconditionally,
      regardless of `Always`/`IfNotPresent`/`Never` — highest value,
      cuts against this project's own edge/offline design goal),
      **`ContainerStatus.imageID` always empty** (CRI's own
      `image_ref` is already available, just unread), and
      **`Node.status.runtimeHandlers` unreported** (CRI's runtime-level
      `Status` RPC is never called at all). See round 50 notes.
- [x] Round 51: `imagePullPolicy` enforcement — new pure
      `effective_pull_policy()`/`image_tag()` plus a new `ImageStatus`
      RPC call to check local presence; `Never` now `bail!`s instead of
      silently pulling, `IfNotPresent` skips the pull entirely when
      cached. Genuinely automated e2e test for `Never`'s refusal
      (a real image/tag, not a fake one, to actually distinguish
      correct from buggy behavior). See round 51 notes.
- [x] Round 52: `ContainerStatus.imageID` — `ContainerRuntimeStatus`
      gained `image_id`, populated from CRI's own `Container.image_ref`
      (already fetched every reconcile, just never read). No new RPC,
      no new plumbing. Genuinely automated e2e test. See round 52
      notes.
- [x] Round 53: `Node.status.runtimeHandlers` — **this closes round
      50's audit list entirely.** New `PodRuntime::runtime_handlers()`
      calls CRI's runtime-level `Status` RPC (never called before this
      round) and maps discovered handlers through, reusing the
      `images`/`mounted_csi_volumes`-shaped plumbing rounds 33/34
      already established. Genuinely automated e2e test. See round 53
      notes.
- [x] Round 54: fresh gap re-audit (no code change) — found 3
      previously-untracked items: **`PodStatus.qosClass` never set**
      (nodelet already computes this internally via
      `eviction::qos_class()`, just never surfaces it — likely the
      cheapest fix), **`PodStatus.hostIPs` (plural) never set** (only
      singular `hostIP` today, mirrors the already-correct
      `podIP`/`podIPs` singular/plural split), and
      **`ContainerStatus.containerID` missing its `<runtime>://`
      scheme prefix** (CRI's `Version` RPC, distinct from round 53's
      `Status` RPC, has never been called either). See round 54 notes.
- [x] Round 55: `PodStatus.qosClass` — new `QosClass::as_str()` plus a
      new `qos` parameter threaded through `build_pod_status()`/
      `write_status()` from all 4 `Pod`-derived status call sites,
      reusing the existing `eviction::qos_class()` computation. Also
      fixed a pre-existing test whose "qosClass is server-owned"
      premise no longer held. Genuinely automated e2e test. See round
      55 notes.
- [x] Round 56: `PodStatus.hostIPs` — `build_pod_status()` now sets
      the plural `hostIPs` alongside the existing singular `hostIP`
      unconditionally, mirroring the already-correct `podIP`/`podIPs`
      split. Genuinely automated e2e test. See round 56 notes.
- [x] Round 57: `ContainerStatus.containerID` scheme prefix — **this
      closes round 54's audit list entirely.** New `runtime_name`
      field (from a one-time `Version` RPC call, never called before
      this round) plus new pure `format_container_id()`, applied where
      `ContainerRuntimeStatus.container_id` is populated so both
      `ContainerStatus.containerID` and `state.terminated.containerID`
      get fixed in one change. Genuinely automated e2e test. See round
      57 notes.
- [x] Round 58: fresh gap re-audit (no code change) — found 3
      previously-untracked items: **HugePages entirely unimplemented**
      (3 pieces: container limit translation — CRI has direct support,
      cheapest; `Node.status.capacity`/`.allocatable` reporting; the
      `emptyDir.medium: HugePages` volume form), **`securityContext.supplementalGroupsPolicy`**
      (GA 1.33, merge-vs-strict group semantics, never read), and
      **Dynamic Resource Allocation** (`spec.resourceClaims`,
      genuinely complex, flagged for completeness — not necessarily
      recommended given the value-to-complexity ratio for a
      single-node edge kubelet). See round 58 notes.
- [x] Round 59: HugePages container limits — closes the cheapest of
      round 58's 3 HugePages pieces. `linux_resources()` now populates
      CRI's `LinuxContainerResources.hugepage_limits` via new pure
      `hugepage_limits()`/`hugepage_cri_page_size()` helpers (the latter
      a naming-convention translation only: k8s's `Mi`/`Gi`/`Ki` suffix →
      CRI's `MB`/`GB`/`KB` page_size string, byte value copied through
      unchanged). Genuinely automated e2e test reading back
      `hugetlb.2MB.limit_in_bytes`, skips cleanly if the node has no
      hugepages reserved. See round 59 notes.
- [x] Round 60: HugePages Node.status.capacity/.allocatable reporting —
      closes the second of round 58's 3 HugePages pieces. New
      `hugepages_capacity_map()`/`hugepage_size_kb_to_k8s_suffix()` scan
      `/sys/kernel/mm/hugepages/` and report every reserved pool size as
      `hugepages-<size>` capacity; `allocatable_map()` needed no changes
      since it already passes unrecognized keys through untouched.
      Genuinely automated e2e test, skips cleanly if no hugepages are
      reserved on the test runner. See round 60 notes.
- [x] Round 61: HugePages emptyDir.medium volume support — closes the
      last of round 58's 3 HugePages pieces (all 3 now fully closed). New
      `is_hugepages_medium_empty_dir()`/`hugepages_medium_pagesize_option()`/
      `hugetlbfs_mount_args()`/`mount_hugetlbfs_empty_dir()` mirror round
      30's tmpfs support; `unmount_memory_backed_empty_dirs()` renamed
      `unmount_special_medium_empty_dirs()` and now unmounts both media on
      pod teardown. Genuinely automated e2e test proving the host
      mountpoint's real filesystem type via `stat -f`, skips cleanly if no
      hugepages are reserved. See round 61 notes.
- [x] Round 62: securityContext.supplementalGroupsPolicy — CRI has direct
      native support (`SupplementalGroupsPolicy` enum on both sandbox- and
      container-level security context); new pure
      `supplemental_groups_policy_cri()` translates `Merge`/`Strict`,
      failing open to `Merge` on anything else. `sandbox_config()` gained
      a new `pod_sc` parameter threaded through `run_sandbox()`.
      Genuinely automated e2e test using a portable ConfigMap-subPath
      `/etc/passwd`/`/etc/group` (not dependent on `$TEST_IMAGE`'s own
      baked-in group file) proves `Strict` excludes image-defined group
      membership while `Merge` includes it. See round 62 notes.

**Scope note (2026-07-31, after round 62):** the project's goal was
re-stated by the user: nodelet should keep thriving as a single-node
deployment, but the project itself is no longer scoped to single-node
only — it's meant to be a full kubelet replacement usable in ordinary
multi-node clusters too. This puts previously-deferred multi-node-
relevant gaps back in scope, most notably **Dynamic Resource Allocation**
(`spec.resourceClaims`) — no longer just "flagged for completeness,
not necessarily recommended," but genuinely in scope now. Documentation
(this file, `README.md`, `docs/ARCHITECTURE.md`) is being updated
accordingly; see those files' own updated framing.

- [x] Round 63: Dynamic Resource Allocation — kubelet's actual DRA
      responsibilities implemented: resolve `spec.resourceClaims` ->
      `ResourceClaim.status.allocation`, call the owning driver(s)'
      `NodePrepareResources`/`NodeUnprepareResources` (new `dra.rs` +
      `proto/draplugin.proto` + a 3rd plugin-registration type), wire
      returned CDI device IDs into `ContainerConfig.cdi_devices`. 18 new
      unit tests. No genuinely automated e2e test — needs a real driver
      binary; see `dra.sh`'s manual-spot-check notes. See round 63 notes
      for known scope limitations (`reservedFor` not written back, no
      RPC batching, proto reconstructed from docs not vendored).
- [x] Code health: split `crates/nodelet/src/runtime/cri.rs` (~4930
      lines across 63 rounds) into 12 focused files under a new
      `runtime/cri/` submodule directory, plus a ~530-line `cri.rs` core
      (module doc/imports, `pub mod v1`/`events`, all top-level
      `const`s, `struct CriRuntime`, `connect_uds()` +
      `CriRuntime::connect()`, and every `#[cfg(test)] mod tests_x;`
      declaration — those stay pointing at the existing
      `cri_tests/*.rs` files unchanged, made visible via a blanket
      `pub(crate) use submodule::*;` re-export per new file):
      `sandbox.rs` (273L, `PodId`/sandbox lifecycle), `container_state.rs`
      (235L, restart/init decision state machine), `volumes_pure.rs`
      (443L) / `volumes_resolve.rs` (513L, split pure helpers from the
      async resolution methods), `env.rs` (500L, env var/DNS/hostname
      resolution), `resources.rs` (285L, resource-quantity/security-context
      translation), `claims.rs` (191L, DRA), `container_create.rs` (624L,
      `ensure_container`/`ensure_init_containers`/
      `create_and_start_container`) / `container_support.rs` (483L, image
      pull/auth/device-release support), `status.rs` (364L,
      `build_status`/stats), `events_gc.rs` (210L), `pod_runtime_impl.rs`
      (455L, the single `impl PodRuntime for CriRuntime` block, which
      can't be split — trait impls must stay in one block, but the file
      itself is still reasonably sized). No behavior change: 809 tests
      passing with `--features cri` (unchanged), 264 mock-only
      (unchanged), `cri_smoke` example builds clean, zero new compiler
      warnings. Not a numbered gap-closure round.
- [x] Round 64: DRA follow-ups — closed both known scope limitations
      round 63 flagged. `pod_is_reserved_for_claim()` (new) gates
      `NodePrepareResources` on `status.reservedFor` actually listing
      this pod — correcting round 63's docs, which wrongly said kubelet
      writes that field (it's scheduler-written; kubelet only gates on
      it). `dra.rs`'s `prepare()`/`unprepare()` now batch every claim a
      driver owns for a pod into one RPC instead of one call per claim,
      via new pure `map_prepare_results()`/`map_unprepare_results()`. 13
      new unit tests. See round 64 notes.
- [x] Fresh gap re-audit (2026-07-31): checked kubernetes.io docs + CRI/
      K8s API struct fields directly, similar to round 58's. Found:
      **hostPath** volumes (tracked ❌ since early rounds, still open —
      picked for round 65), **`lifecycle.stopSignal`** (GA 1.33, CRI
      already has native `Signal stop_signal` support, newly found —
      not yet picked), **swap support** (`memorySwap.swapBehavior`, GA
      1.34, CRI already has `memory_swap_limit_in_bytes`, newly found —
      not yet picked), and confirmed `emptyDir.sizeLimit` enforcement
      is still open (already tracked). User picked hostPath.
- [x] Round 65: hostPath volumes — `spec.volumes[].hostPath` resolves to
      the host's own existing path directly, with real `type` validation
      (`DirectoryOrCreate`/`FileOrCreate`/`Directory`/`File`/`Socket`/
      `CharDevice`/`BlockDevice`) via new pure `validate_host_path()`.
      10 new unit tests + 3 genuinely automated e2e tests (real
      bind-mount proof via a host-written marker file, `DirectoryOrCreate`
      actually creating, `Directory` NOT creating). See round 65 notes.
- [x] Round 66: lifecycle.stopSignal — translates to CRI's native
      `Signal` enum via new pure `stop_signal_cri()`/`stop_signal_k8s()`
      (naming-convention-only, k8s and CRI share the same 65-signal set).
      `containerStatuses[].stopSignal` reported back for exited
      containers only (reuses an already-made RPC, no new per-container
      cost on a healthy pod). 7 new unit tests + a genuinely automated
      e2e test proving a non-default signal (SIGUSR1) actually gets
      delivered via a trap-and-marker container. See round 66 notes.
- [x] Round 67: emptyDir.sizeLimit enforcement — a plain-disk `emptyDir`
      volume exceeding its own `sizeLimit` now evicts the pod, checked
      independently of the whole-pod ephemeral-storage limit and general
      node pressure. New `PodUsage.empty_dir_usage_bytes` (per-volume) +
      pure `empty_dir_size_limits()`/`first_empty_dir_over_limit()` in
      `eviction.rs`. Scoped to plain-disk emptyDir only —
      `Memory`/`HugePages`-medium `sizeLimit` is already kernel-enforced
      at mount time. 12 new unit tests + a genuinely automated e2e test.
      See round 67 notes.
- [x] Round 68: swap support (memorySwap.swapBehavior) — new
      `NODELET_MEMORY_SWAP_BEHAVIOR` knob (`NoSwap` default /
      `LimitedSwap`) drives CRI's native `memory_swap_limit_in_bytes` via
      new pure `container_swap_limit_bytes()`, implementing KEP-2400's
      formula exactly. `resize_decision()` also now compares the new
      field. 11 new unit tests + a genuinely automated e2e test proving
      the default `NoSwap` behavior's real cgroup effect
      (`memory.swap.max == 0`); `LimitedSwap` itself gets a
      manual-spot-check note (needs a node restart with a different env
      var). See round 68 notes. This closes every candidate round 65's
      fresh gap re-audit found.
- [x] Round 69: subPath $(VAR) expansion — preceded by a fresh gap
      re-audit (round 65's own candidates all closed; found `subPathExpr`
      itself, image GC watermark policy, and ServiceAccount token
      credential providers — user picked `subPathExpr`). New pure
      `expand_sub_path_expr()` substitutes `$(VAR)` against a container's
      own resolved env, applied to both `HostPath`- and image-backed
      volumes via `build_mounts()`'s new `envs` parameter. An
      unresolvable/unclosed reference drops the mount rather than
      substituting a garbage path. 7 new unit tests + a genuinely
      automated e2e test proving real Downward-API-driven expansion. See
      round 69 notes.
- [x] Round 70: image GC watermark policy — real kubelet's
      disk-pressure-triggered high/low watermark GC (3 new config knobs
      matching upstream's own flags), replacing the previous
      unconditional every-cycle unreferenced-image sweep — a real
      behavior change. New pure `disk_usage_percent()`/
      `should_start_image_gc()`/`images_to_reclaim_space()`, a new
      `image_unreferenced_since` side table on `CriRuntime`. 15 new unit
      tests; the existing automated e2e test for unconditional removal
      was rewritten to prove the opposite (a freshly-unreferenced image
      survives a GC cycle below the watermark), with actual
      watermark-triggered removal joining the manual-procedure group.
      See round 70 notes.
- [x] Round 71: image credential providers — kubelet's
      `--image-credential-provider-config`/`-bin-dir` exec-plugin
      protocol, including ServiceAccount token integration
      (`tokenAttributes`, beta/default-on in k8s 1.34). New
      `credential_provider.rs` module: config parsing, `matchImages` glob
      matching (real kubelet's own segment/port/path rules), 3-tier
      response caching, and subprocess exec of the first matching
      provider. `resolve_pull_auth()` tries `imagePullSecrets` first,
      falling back to a credential provider only if no secret resolves.
      21 new unit tests; a new e2e test file automates the
      feature-off-by-default regression check, with real exec-plugin
      fetches joining the manual-procedure group (needs a real cloud
      vendor binary + live registry). See round 71 notes.
- [x] Round 72: fresh gap re-audit — no code change. Found 3 new
      candidates: crash-loop backoff (biggest finding — restart-on-exit
      has no delay/attempt throttle at all, a genuine tight-loop risk
      given nodelet's event-driven reconcile, not just a missing status
      string), the PodResources API gRPC service (read-only projection of
      already-tracked CPU/Memory/device-manager state, real external
      tooling depends on it), and `allocatedResourcesStatus`/
      ResourceHealthStatus (alpha/beta, lower priority). See round 72
      notes.
- [x] Round 73: crash-loop backoff — `restart_backoff_ready()`/
      `record_restart_backoff()` (new `restart_backoff` side table on
      `CriRuntime`) throttle the actual restart, not just its status: a
      container that keeps exiting now waits an exponentially growing
      delay (10s base, doubling, capped at 5 minutes, resetting after
      10 minutes stable — matching real kubelet's own constants) instead
      of being recreated as fast as this event-driven controller's own
      status-write-triggers-another-watch-event feedback loop allows.
      Scoped precisely to genuine exit-triggered restarts only (a
      resize-triggered restart is never throttled). 10 new unit tests +
      a genuinely automated e2e test proving a low restart count over a
      20s window for an immediately-exiting container. **Still open**:
      the `CrashLoopBackOff` status string itself, since
      `ContainerStatus.lastState` isn't tracked at all yet. See round 73
      notes.
- [ ] Candidates for the next round, ranked: PodResources API (real
      external-tooling value, low implementation risk since
      cpu_manager.rs/memory_manager.rs/device_plugins.rs already track
      the underlying state), `allocatedResourcesStatus` (alpha/beta
      upstream, lower urgency), or `ContainerStatus.lastState` tracking
      (would unlock the `CrashLoopBackOff` status string this round
      deliberately left out). Ask before starting the next round.
