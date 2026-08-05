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

**Fix direction**: on a device this small, `nodelet-build.sh` should
default to the light LTO settings for the *first* attempt too (or detect
low total RAM — e.g. via `/proc/meminfo` — and choose the profile up
front) rather than always trying the expensive one first and hoping the
process-level retry logic gets a chance to run. A build that can crash
the host it's running on shouldn't be the default path on exactly the
class of device this project targets.

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

