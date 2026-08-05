# e2e test run findings (2026-08-04)

Working doc while running `deploy/test-e2e.sh` test-by-test against a real
CRI-mode nodelet + k3s control plane on real hardware, to find and later fix
whatever's actually broken. Companion to `docs/GAP_CLOSURE.md` (which tracks
scope/feature gaps) — this tracks concrete bugs found by actually running
the suite.

Binary under test: rebuilt from `e1b3940` ("Round 102") on 2026-08-04.

## Confirmed bugs

### 1. Pod objects never actually get removed from the apiserver on delete

**Severity: high — cascades into most of the suite over a long run.**

`PodController::teardown()` (`crates/nodelet/src/pods.rs`), called from
`reconcile()` whenever a pod has `deletionTimestamp` set, only calls
`self.runtime.remove_pod(pod)` (tears down the CRI containers/local
runtime state) and logs "torn down" — it never issues a final
`Api::<Pod>::delete()` call back to the apiserver.

Real kubelet's behavior: a graceful `kubectl delete pod` only *sets*
`deletionTimestamp` (soft delete, object stays in etcd); the object is
actually purged only when kubelet, after finishing termination, calls
`Delete` again (effectively grace-period-0 at that point). Nothing in
nodelet does this second call for the normal deletion path — only
nodelet's own self-initiated terminations (`evict_pod()` in `main.rs`,
static pod mirror cleanup in `static_pods.rs`, shutdown handling in
`shutdown.rs`) issue a real `pod_api.delete()`.

**Effect observed**: every pod deleted via normal `kubectl delete
pod`/`kubectl delete namespace` gets its containers torn down
correctly, but the Pod object itself sits forever in `Terminating`.
Namespaces containing such pods never finish deleting (namespace GC
waits for all contained objects to be gone). Over the course of one
e2e run, three consecutive test namespaces were still stuck
`Terminating` 50+ minutes later, and pods kept phase `Running`/`1/1`
in `kubectl get` despite deletionTimestamp being long past. This is
what made repeated e2e runs look increasingly "broken" — a growing
pile of undead namespaces/pods, not something individual tests do
wrong.

**Fix direction**: `teardown()` needs to also call
`Api::<Pod>::delete(name, &DeleteParams { grace_period_seconds: Some(0), ..Default::default() })`
after `remove_pod()` succeeds (mirroring the pattern already used in
`evict_pod()`/`static_pods.rs`/`shutdown.rs`), tolerating a 404 (already
gone) as success. Needs a unit/e2e test asserting a deleted pod's
object actually disappears from the apiserver within a bounded time,
not just that its containers stop.

**Workaround used to unblock further e2e runs**: `kubectl delete pod
--grace-period=0 --force` / `kubectl delete ns --grace-period=0
--force` on the leftover objects.

### 2. `--with-cri` build can silently leave a stale non-CRI binary installed on memory-constrained devices

**Severity: high on small devices — the whole point of this project — but
easy to misdiagnose as "I must have mistyped something."**

This device is ~2.8GB RAM / 8 cores. `[profile.release]` in the workspace
`Cargo.toml` uses `lto = true, codegen-units = 1` for the smallest/fastest
edge binary — right for CI/a beefy build box, but the final whole-program
LTO/codegen step needs memory proportional to the *entire* dependency
graph merged into one unit, and with `--features cri` that graph includes
tonic/prost/rustls/zbus/x509-parser on top of kube. On this device that
step alone drove free memory from ~1.4GiB down to ~100MiB and started
pulling on swap before anything visibly failed — confirmed by hand,
twice, running a bare `cargo build --release --features cri` and watching
`free -h` while it happened (had to `pkill -9 rustc cargo` both times to
stop it before it took the whole VM down).

`deploy/lib/nodelet-build.sh`'s `build_nodelet()` *does* already have a
fallback for this (retry once with `CARGO_PROFILE_RELEASE_LTO=thin
CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16` if the first `cargo build` fails)
— but the first attempt, the dangerous one, always runs first and unlike
a normal OOM-killed process (which returns a clean nonzero exit `cargo
build` can catch and retry from), a build that takes down the whole VM
gets no chance to retry: the box comes back up with whatever was already
sitting in `target/release/nodelet` from a previous build (e.g. an older
mock-only binary from initial setup) untouched by the crashed attempt,
and nothing about that state screams "this is stale" — `bin/nodelet`'s
mtime updates because *something* ran `install` at some point in this
device's history, just not the run that just crashed.

This is almost certainly what happened here: the binary this session
found installed (`bin/nodelet`, root-owned, dated the same session as a
reported `--with-cri` run) contains **zero** occurrences of
`crates/nodelet/src/runtime/cri.rs` in its strings — the CRI feature's
code plainly isn't in it — and the systemd unit's `NODELET_RUNTIME` was
still `mock`. Both are consistent with the real `cargo build --release
--features cri` crashing the VM before `install_nodelet_service` (which
writes `NODELET_RUNTIME=cri` into the unit) ever ran, on a reboot that
left the previous mock-only build's artifacts in place.

**Fixed**: `nodelet-build.sh` now checks `/proc/meminfo`'s `MemTotal`
(`release_lto_settings_for_this_device()`) and goes straight to the light
LTO settings for the *first* attempt on anything under ~4GB RAM, instead
of always trying the expensive profile first and hoping the process-level
retry gets a chance to run. Also fixed alongside it, same investigation:
`install_nodelet_binary()` now `rm -f`s the destination before `install`
and checks the result — the plain `install -m 0755 src dst` this replaced
silently no-op'd on a permission error against an existing (e.g.
root-owned, from an earlier privileged run) `bin/nodelet`, which is
exactly how this session ended up with the stale binary described above:
the surrounding script logged "nodelet built" unconditionally right after,
with nothing about the output saying the copy had actually failed. And
`install_nodelet_service_systemd`/`_openrc`/`_fallback` now explicitly
`restart` (or kill-and-respawn, for the fallback tier) rather than
`enable --now`/`start`, which are no-ops against an already-running
service — a re-run of `bootstrap-source.sh` was silently leaving the OLD
nodelet process running the whole time regardless of what the new build
produced.

**What actually worked**: `CARGO_PROFILE_RELEASE_LTO=thin
CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 CARGO_BUILD_JOBS=2 cargo build
--release --features cri` (capping parallel rustc jobs too, not just
LTO) — confirmed this stays well within memory on this device.

### 3. Not a code bug: this device runs genuinely close to real `DiskPressure`

Once a real CRI binary was running, every single pod (across every test)
failed to schedule at all — `FailedScheduling ... 1 node(s) had
untolerated taint(s)`. The node really did have
`node.kubernetes.io/disk-pressure:NoSchedule` set, and it was correct:
`/var/lib/nodelet` (the configured `disk_path`) had ~5.7% available space,
under the default 10% hard threshold — this is real kubelet-standard
behavior (no plain pod tolerates that taint by default), not a nodelet
bug. Root cause was mundane disk bloat, not this project's own state:
leftover build toolchain/`target/` from investigating finding #2 above
(3.5GB), stale apt caches (~530MB), an old Claude Code CLI version
directory (273MB), and normal apt list indexes. Freed all of it and got
back above 10% available, which cleared the taint within one eviction
tick (`eviction_check_interval`) with no code change needed.

**Practical note for future sessions on this device**: keep an eye on
`df -h /` / `/var/lib/nodelet`'s statvfs available-percent — a `--with-cri`
build alone can eat enough headroom (rust toolchain + `target/`, ~3-4GB)
to tip this specific device into real `DiskPressure`, which then makes
*every* e2e test that schedules a pod fail for a reason that has nothing
to do with the code under test. `rm -rf target/ .bootstrap/toolchain`
after a successful build (the binary's already been copied to
`bin/nodelet`) is cheap insurance.

## Confirmed bugs (continued)

### 4. Self-initiated eviction deletes the Pod object immediately, unlike real kubelet — makes its own status unobservable

**Severity: medium — the eviction *decision* itself is correct and fast;
only the after-the-fact observability is wrong, and it makes
`test_pod_exceeding_its_own_ephemeral_storage_limit_is_evicted` (and,
by the same code path, every other eviction test) inherently racy
rather than reliably assertable.**

`evict_pod()` (`main.rs`) — the one function behind node-pressure
eviction, per-pod ephemeral-storage/emptyDir limit violations, and
`activeDeadlineSeconds` — patches `status.phase=Failed` /
`status.reason=<given reason>`, then *immediately* issues a real
`pod_api.delete()` with `grace_period_seconds: 0`, back-to-back with no
delay. Confirmed live (nodelet logs): the ephemeral-storage-limit test's
pod goes from Running to fully deleted from the apiserver in well under
a second of wall-clock time — `kubectl get pod ... -o
jsonpath={.status.reason}` essentially never has a chance to observe
`Evicted` before the object is just gone (404), regardless of how often
something polls for it. `not-k8s-e2e-test`'s own `wait_until` (2s poll
interval, 60s budget) never once caught it live.

Real kubelet does **not** do this: an evicted (or otherwise
kubelet-terminated) bare Pod is left in the apiserver with
`phase: Failed` / a `reason` for its owner (or a human) to observe and
clean up — this is the well-known real-world behavior where nodes
accumulate visible "Evicted" pods until something (a TTL controller, a
human running `kubectl delete pod --field-selector=status.phase=Failed`,
etc.) removes them. The project's own code already half-documents this
tension — `evict_pod()`'s doc comment calls out that reusing this same
terminate-and-delete path for `activeDeadlineSeconds` specifically is a
"**Deliberate simplification vs. upstream**... real kubelet marks the
pod `Failed`/`DeadlineExceeded` but leaves the object itself for the
owning controller to observe/react to" — but doesn't flag that the
*other* two callers of the same function (node-pressure eviction, the
ephemeral-storage/emptyDir case this test exercises) inherit the exact
same deviation, and that it isn't just a controller-observability nuance
but actively breaks black-box assertions like this e2e test's.

**Fix direction**: stop deleting the object from `evict_pod()` itself —
patch status only (`phase: Failed`, the given `reason`/`message`) and
let deletion be someone else's job (a controller, a human, or a future
TTL-based GC pass), matching upstream. `gc.rs` already exists and
already reasons about pod lifecycle for cleanup purposes — worth
checking whether it's the natural place to eventually reclaim
long-Failed unmanaged pods, rather than nodelet unilaterally deleting
them itself at eviction time.

Not fixed in this pass — noted here rather than blocking on it, since
unlike finding #1 it doesn't cascade into other tests (if anything, the
current immediate-delete behavior keeps eviction tests' own namespaces
cleaner, not messier) and the eviction *decision* logic itself
(`pick_eviction_candidate()` et al.) is separately covered by
`cargo test`'s `eviction_tests/`.


### 5. Fixed test bug (not app code): `sleep 3600 & wait` doesn't actually block on the child

**Severity: test-only, no product impact — but was previously
unobservable, and its own attempted fix hit two more real gotchas worth
recording.**

`test_termination_grace_period_is_honored_not_instant`
(`lib/test/cases/hooks.sh`) used
`command: ["sh", "-c", "trap 'echo trapped' TERM; sleep 3600 & wait"]`,
intending a container that traps SIGTERM, keeps running, and only
actually dies to a SIGKILL once `terminationGracePeriodSeconds` (8s in
this test) elapses. It doesn't do that: in ash/dash (and other POSIX
shells), a signal caught by `trap` while `wait` is blocked interrupts
`wait` immediately once the trap handler returns — regardless of
whether the backgrounded child (`sleep 3600`) has actually exited. So
the container's PID 1 (the `sh -c` script) reaches its own end and
exits voluntarily within milliseconds of receiving SIGTERM, having
"trapped" it but not actually stayed alive. Confirmed directly (both
via a plain local `sh -c` repro and via `crictl stop --timeout 8` on a
live container from this exact pod spec): full teardown in ~0.3–1.7s,
nowhere near 8s.

This was invisible before finding #1's fix: with pod deletes never
actually reaching the apiserver, this test's `wait_until ... pod_gone`
could never observe the pod disappearing either-fast-or-slow — it just
always timed out for the same reason every other pod-delete assertion
did. Fixing finding #1 exposed this as a *different* failure (pod
gone almost immediately, not stuck) rather than revealing it as a pass.

**Fix**: replaced the container command with
`trap 'echo trapped' TERM; while true; do sleep 1; done` — a foreground
loop has no `wait`-interrupt escape hatch; dash still runs the trap and
returns straight to the loop, so only a real SIGKILL (at the end of the
grace period) ends it. Verified: 15s round-trip (create → Running →
delete → gone), comfortably inside the test's own 5–35s sanity bounds.

**Two harness gotchas hit while fixing this, worth remembering for
next time someone edits a case file or drives it standalone**:

- **Backticks inside a heredoc "comment" are not inert.** The fix's
  first draft explained the bug in a `#`-prefixed comment line inside
  the pod manifest's `apply_manifest <<EOF ... EOF` heredoc, using
  backticks for inline-code emphasis (`` `sleep 3600 & wait` ``). An
  unquoted heredoc (`<<EOF`, not `<<'EOF'`) isn't parsed as bash
  source — there's no such thing as a heredoc "comment" to the shell,
  `#` is just a literal character — so bash's normal command
  substitution still fires on backticks anywhere in the body,
  regardless of a leading `#`. The result: `` `sleep 3600 & wait` ``
  actually ran on the *host*, backgrounding a real `sleep 3600` and
  blocking the whole heredoc's construction on `wait` — which looks
  exactly like the pod hanging, except no `kctl apply` (or anything
  else pod-related) ever actually ran; `kubectl get events` showed
  nothing, because nothing was ever sent to the apiserver. Lesson: no
  backticks (or unescaped `$`) in prose inside an unquoted heredoc,
  ever — quote them (`'like this'`) instead.
- **`timeout N sudo -n bash script.sh` doesn't reliably kill anything.**
  `sudo` doesn't forward SIGTERM to the child it execs by default, so
  `timeout`'s signal to the `sudo` process can leave `script.sh`
  running as an orphan indefinitely, invisible to whatever's waiting on
  the timed-out call. `sudo -n timeout -k <grace> N bash script.sh`
  (timeout *inside* the sudo call, directly wrapping the real target)
  doesn't have this problem. Several stray `run_one.sh` processes (and,
  once, an actual node-wide `DiskPressure` trip from the image churn
  they caused) came from this exact mistake during today's session.

### 6. Fixed systemic test-harness bug: `bash -c "...kctl..."` never worked (function not exported)

**Severity: test-only, but wide — 23 call sites across 8 case files,
found by scanning rather than one at a time.**

`kctl`, `pod_field`, `pod_condition_status`, `pod_container_restart_count`,
`pod_volume_host_path`, and friends (`lib/test/k8s.sh`,
`lib/test/manifests.sh`) are plain shell functions, convenient inside the
case files themselves (which run in the *same* bash process that sourced
them) — but several `wait_until`/`try_wait_until` call sites build their
poll check as `bash -c "... kctl get pod ... "`, which execs a genuinely
separate `bash` process. Shell functions aren't inherited by a child
process unless explicitly `export -f`'d, and nothing here was — so every
one of those 23 sites' function reference silently resolved to "command
not found" (swallowed by the surrounding `2>/dev/null`), the condition
being polled for could never become true, and the test just burned its
full timeout and failed regardless of what was actually happening in the
cluster. Confirmed directly: `test_native_sidecar_container_restarts_on_crash`
failed on `kctl get pod ... initContainerStatuses[0].restartCount`
timing out — while `kubectl get pod` run by hand against the exact same
live pod showed the restart count climbing normally (1, 2, 3, ... every
~5s) the whole time.

Found the full extent with a script scanning every `lib/test/cases/*.sh`
for a helper-function name appearing inside a `bash -c "..."` string,
rather than fixing the one site that happened to surface first — 23
matches across `eviction.sh`, `lifecycle.sh`, `probes.sh`,
`readiness_gates.sh`, `resources.sh`, `security.sh`, `streaming.sh`.

**Fix**: `export -f` every one of these helpers (`k8s.sh`,
`manifests.sh`, plus `log`/`warn`/`die` in `common.sh`, since several
helpers call `die`), and `export TEST_NAMESPACE` wherever it's set
(`test-e2e.sh`; this session's own standalone harness needed the same
fix) — the functions are also useless in a child process without the
variable they all key off of. One central fix rather than rewriting 23
call sites to avoid the helpers. Verified:
`test_native_sidecar_container_restarts_on_crash` now passes in 7s.

Re-ran the previously-diagnosed eviction failures after this fix to
check whether it was secretly the whole story there too — it wasn't:
`test_pod_exceeding_its_own_ephemeral_storage_limit_is_evicted` still
times out identically. Finding #4 (immediate-delete-on-evict racing the
assertion) is a real, separate bug, not an artifact of this one.

### 7. Fixed test bug: `busybox httpd` isn't compiled into alpine:3.20's busybox

**Severity: test-only.** `test_host_port_publishes_the_container_on_the_nodes_own_ip`
and `test_host_network_pod_needs_no_explicit_port_mapping`
(`lib/test/cases/networking.sh`) both used `busybox httpd -f -p <port>
-h /www` as their in-container HTTP responder. Confirmed live: alpine
3.20's busybox binary doesn't have the `httpd` applet compiled in
(`busybox httpd` → `httpd: applet not found`, exit 127), so the
container crash-looped immediately — which both tests' own error
handling reported as "pod never reached Running", pointing a debugger
at `port_mappings_for()`/`sandbox_config()`/CRI wiring that was never
actually exercised. Real cause had nothing to do with hostPort/
hostNetwork at all.

**Fix**: replaced the responder with a `busybox nc -lp <port>` loop
serving a pre-built HTTP response from a file (`nc` *is* in this
image's busybox build, confirmed live) — `while true; do nc -lp $port
< /tmp/resp; done`, since busybox's `nc -l` exits after one connection
and needs the loop to keep serving `try_wait_until`'s repeated polls.
Verified both tests pass live (4s and 25s respectively).

## Confirmed bugs (continued, part 2)

### 8. `restartCount` never incremented for probe-triggered restarts

**Severity: medium — a real, user-visible status field silently wrong,
though the restart itself (kill+recreate) works correctly.**

`restart_container()` (`runtime/cri/pod_runtime_impl.rs`) — the path a
failed liveness/startup probe takes to kill and recreate a still-running
container — stops and removes the container but never calls
`bump_restart_count()`. Confirmed live: a liveness-probe-triggered
restart cycle repeated correctly (container actually stopped, actually
recreated, new `startedAt` each time) but `status.containerStatuses[0]
.restartCount` stayed at `0` through multiple observed cycles.

Root cause, once traced through `container_create.rs`'s reconcile path:
that path *does* call `bump_restart_count()`, but only from the branch
that finds an **existing** (CRI-reported-exited) container to replace —
the normal crash-restart case, confirmed still working correctly
(`test_crashing_container_restarts_and_increments_restart_count`
passes). `restart_container()` runs *before* that reconcile ever sees
the container again — it's a separate, self-contained stop+remove called
directly from the probe-failure path (`main.rs`'s probe loop) — so by
the time the next reconcile runs, there's no existing container left to
trigger that branch's increment. The two restart paths (crash-detected
vs. probe-killed) share the removal/recreation *behavior* but hadn't
shared the counter bump.

**Fix**: `restart_container()` now calls `self.bump_restart_count(&sandbox_id,
container)` itself, right after removing the old container instance —
matching what the reconcile path already does for the crash case, just
inline here since there's no second chance to catch it later. Verified
live: `test_liveness_probe_failure_restarts_the_container` now passes
(45s).
