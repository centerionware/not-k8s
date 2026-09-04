# e2e test run findings (2026-08-04)

Working doc while running `deploy/test-e2e.sh` test-by-test against a real
CRI-mode nodelet + k3s control plane on real hardware, to find and later fix
whatever's actually broken. Companion to `docs/GAP_CLOSURE.md` (which tracks
scope/feature gaps) — this tracks concrete bugs found by actually running
the suite.

Binary under test: rebuilt from `e1b3940` ("Round 102") on 2026-08-04.

## Rust bootstrapper migration gates: runs 32809878935 and 32811051979

This table combines the failures observed while running the Rust e2e runner
against a cluster installed by `nodebootstrap` on 2026-08-25. Run
`32809878935` was cancelled before shards 1 and 5 finished, so those rows are
observations up to cancellation rather than complete shard totals. It had 32
observed test failures: shard 1 had 7, shard 2 had 7, shard 3 had 5, shard 4
had 7, and shard 5 had 6. The subsequent source-build run `32811051979`
failed before e2e on all five shards with the same bootstrap toolchain error;
it produced no additional test results.

The historical shell suite passed these feature areas, but that is not marked
as completion here. The last column receives `✅` only after the named Rust
test passes against a cluster configured by the bootstrapper. A blank means
that the failure still needs to be fixed or re-run successfully.

| Shard(s) | Rust test or test group | Observed result | Classification | Bootstrapper/test-environment control needed | Prior shell baseline | Completed under Rust bootstrapper |
| --- | --- | --- | --- | --- | --- | --- |
| 1-5 (run 32811051979) | Source-built combined runtime bootstrap | Every shard failed before the e2e runner: `can't find crate for core`; `x86_64-unknown-linux-musl` was not installed. Each shard also emitted rustup warnings about Rust already being installed and `$HOME` differing under `sudo` | Confirmed bootstrap toolchain setup defect; no Rust e2e result exists for this run | Install and verify the musl target in nodebootstrap's managed rustup toolchain before `cargo build`; keep `HOME`, `CARGO_HOME`, and `RUSTUP_HOME` consistent instead of relying on a root/sudo rustup context | N/A | |
| 1 | `test_static_pod_creates_a_mirror_pod` | Mirror Pod was NotFound | Runtime or test-fixture behavior; no missing prerequisite reported | Ensure the bootstrapper exposes the static-Pod path and restarts/reloads nodelet after the test override | Passed | |
| 1 | `test_datastore_creates_a_key_only_if_absent` | Compare-and-put returned `succeeded: false` for a missing key | Datastore behavior/translation | Verify revision-zero compare semantics through the Rust gRPC path | Passed | |
| 1 | `test_a_real_apiserver_starts_and_serves_against_nodestore` | Throwaway apiserver client could not connect | Throwaway test-environment/setup behavior | Reserve an isolated apiserver port and wait for the local apiserver/nodestore pair before querying it | Passed | |
| 1, 2, 3 | `test_device_plugin_advertises_capacity_and_allocates_into_a_container`; `test_device_plugin_health_transition_updates_allocated_resources_status`; `test_device_plugin_preferred_allocation_and_prestart` | Fake device capacity never appeared in Node status | Nodelet device-plugin integration; the tests start their own fake plugin, so this is not a missing CI driver | Confirm plugin socket registration, ListAndWatch delivery, and Node status publication before creating the Pod | Passed | |
| 1 | `test_csi_ephemeral_inline_volume_is_mounted` | Inline-volume Pod never reached Running | CSI inline-volume runtime/test path; reference CSI driver was installed | Keep the host-path CSI driver installed and expose `TEST_CSI_INLINE_DRIVER`; then diagnose nodelet inline CSI staging | Passed | |
| 1, 2 | `test_scheduler_consults_an_http_extender_and_honours_a_filter_rejection`; `test_scheduler_schedules_a_pod_an_http_extender_approves` | `sudo tee` could not write the scheduler override because its parent directory did not exist | Confirmed test setup defect | Create `/etc/systemd/system/nodescheduler.service.d` before writing the override, then daemon-reload and restart the scheduler | Passed | |
| 1 | `test_pod_exceeding_its_active_deadline_is_terminated` | Deadline termination was not observed before timeout | Runtime behavior or status-observation race | Keep the Pod visible long enough to observe `DeadlineExceeded`, or make the test match the bootstrapper's eviction lifecycle | Passed | |
| 2 | `test_scheduler_wakes_a_pending_pod_on_a_real_event` | Scheduler blocker never became bound | Scheduler/runtime behavior | Ensure nodescheduler is active, has a valid lease, and receives the event/watch update after the resource change | Passed | |
| 2 | `test_fsgroup_change_policy_on_root_mismatch_skips_the_second_chown` | First fsGroup-policy Pod never reached the expected state | Volume/runtime behavior | Ensure CSI host-path readiness and volume ownership prerequisites are complete before this test starts | Passed | |
| 2 | `test_client_certificate_authentication_works` | Connection refused on nodelet port 10250 | Bootstrap service configuration mismatch | Enable nodelet's CRI HTTPS server, set `NODELET_CLIENT_CA_FILE`, publish the serving endpoint, and restart nodelet after the test override | Passed | |
| 2 | `test_sysctls_are_applied_to_the_sandbox` | Pod sysctl value never appeared | Runtime behavior or host sysctl policy mismatch | Configure permitted sysctls on the runner and make nodelet report rejected sysctls instead of leaving the Pod waiting indefinitely | Passed | |
| 2 | `test_clusterip_is_reachable_from_inside_a_pod` | ClusterIP access timed out | CNI/nodeproxy/runtime integration | Verify the bootstrapper's flannel/CNI mode, nodeproxy service, nftables capabilities, and Pod-to-Service routing before the test | Passed | |
| 3 | `test_tls_bootstrap_issues_a_real_client_certificate` | Nodelet never submitted a TLS bootstrap CSR | TLS-bootstrap fixture or nodelet bootstrap behavior | Provide a bootstrap kubeconfig with CSR create/watch permissions and verify nodelet consumes it in a clean service environment | Passed | |
| 3 | `test_upgrade_straight_to_a_multi_member_cluster_is_refused` | Fixture failed preparing client API TLS material before checking the refusal | Confirmed test-fixture configuration defect | Generate and pass the datastore CA, client certificate, and client key to the throwaway multi-member fixture | Passed | |
| 3 | `test_unreferenced_image_is_not_removed_below_the_watermark` | Containerd did not retain the pulled image | Image-store/runtime behavior | Configure the CRI image namespace and wait for the pull to be visible through the same containerd endpoint used by nodelet | Passed | |
| 3 | `test_image_volume_source_mounts_a_read_only_image` | Image-volume Pod never reported its read-only mount | Image-volume runtime behavior or image availability | Pre-pull/retain the source image and verify the CRI image-volume mount path is enabled in the bootstrapper | Passed | |
| 4 | `test_limited_swap_gives_burstable_pods_proportional_swap` | API rejected the Pod because `spec.containers` was missing | Confirmed test fixture defect | Add the intended container to the Pod manifest before testing LimitedSwap | Passed | |
| 4 | `test_datastore_refuses_a_read_below_the_compaction_point` | Revision parsing failed with `invalid digit found in string` | Confirmed datastore e2e translation defect | Pass numeric revisions as JSON numbers or unquoted strings consistently through the tonic helper | Passed | |
| 4 | `test_image_gc_removes_unreferenced_images_above_the_watermark` | Containerd did not retain the pulled image | Image-store/runtime behavior | Make image retention and GC thresholds deterministic in the bootstrapper's containerd configuration | Passed | |
| 4 | `test_host_network_pod_uses_the_node_network_namespace` | Pod IP did not match Node InternalIP | Host-network/runtime or node-address environment mismatch | Make node address detection and host-network namespace use agree with the bootstrapper's selected interface | Passed | |
| 4 | `test_set_hostname_as_fqdn_reports_the_full_fqdn` | FQDN was not observed before timeout | Runtime behavior or hostname setup | Set a stable node hostname/domain in the bootstrap environment and verify the runtime writes the expected hostname | Passed | |
| 4 | `test_credential_provider_supplies_auth_for_an_otherwise_rejected_pull` | Containerd had no CRI registry section | Confirmed bootstrap environment configuration gap | Bootstrap a writable CRI registry config with credential-provider plugin settings before running the test | Passed | |
| 4 | `test_kubectl_attach_streams_the_containers_stdout` | Later stream line never arrived | Streaming runtime or Rust assertion behavior | Ensure nodelet server/attach proxy is enabled and flushes multiple stream frames through the bootstrapper's endpoint | Passed | |
| 5 | `test_datastore_enforces_compare_and_swap` | Revision parsing failed with `invalid digit found in string` | Confirmed datastore e2e translation defect | Preserve numeric revision types when converting tonic responses into transaction requests | Passed | |
| 5 | `test_image_pull_policy_never_fails_when_image_is_absent` | `ErrImageNeverPull` was never observed | Runtime/status behavior | Ensure the image is absent from the configured containerd namespace and wait for the terminal image-pull status | Passed | |
| 5 | `test_host_aliases_still_work_under_host_users_false` | Pod never exposed the expected host aliases | Runtime user-namespace/hosts-file behavior | Verify hostUsers namespace setup and `/etc/hosts` materialization under the bootstrapper's CRI configuration | Passed | |
| 5 | `test_host_path_directory_type_rejects_a_nonexistent_path` | Missing Directory hostPath rejection was never observed | Runtime/status behavior | Ensure the path is absent on the node and surface the mount validation failure in Pod status | Passed | |
| 5 | `test_run_as_user_is_applied` | Expected termination message never appeared | Runtime/security-context behavior | Verify CRI user/group mapping and termination-log collection in the bootstrapper's containerd setup | Passed | |
| 5 | `test_host_users_volume_ownership_translation_is_correct` | Ownership translation was never observed | Runtime user-namespace/volume behavior | Configure subordinate-ID/user-namespace support and verify the hostPath ownership mapping used by nodelet | Passed | |

### Positive controls from the same cancelled run

These are not failed rows and are the setup pieces already demonstrated to
work under the Rust bootstrapper:

| Control | Evidence | Completed |
| --- | --- | --- |
| Reference CSI driver installation | Generic ephemeral, dynamic CSI registration, `volumesInUse`, attachment, raw-block, and WaitForFirstConsumer tests passed | ✅ |
| Reference DRA driver installation | DRA claim allocation/reservation test passed | ✅ |
| Worker bootstrap without flannel or proxy | Shards 3 and 4 completed the worker validation successfully; shard 2 reached validation before cancellation | ✅ |
| Per-shard result publication | The retrying publisher preserved separate shard result files despite concurrent branch updates | ✅ |

The cancelled run still used a prebuilt `target/debug/notk8s`; it did not
exercise source compilation. The subsequent source-build run
`32811051979` attempted the intended path, but all five shards failed before
cluster bootstrap for the same missing-musl-target problem documented above.
It therefore contributes no additional test pass/fail results; the source
bootstrap gate remains incomplete until that toolchain setup is fixed.

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

### 7b. Same `busybox httpd` bug, one more site: `probes.sh`

`test_http_get_readiness_probe_against_a_real_server` had the identical
`busybox httpd -f -p 8080 -h /www` bug documented in finding #7 above.
Same fix (an `nc -lp` loop serving a canned 200 response — the probe
only checks the status code, not the exact body/path). Grepped the
whole suite afterward for any other `busybox httpd` survivors: none.
Verified live: passes in 4s.

### 9. kubelet-style server's self-signed cert had zero IP SANs — apiserver proxy dial always failed

**Severity: high — this alone blocked `kubectl exec`/`logs`/`attach`/
`port-forward` end to end, entirely independent of and prior to the
separate CA-trust gap noted below.**

`server::tls::load_or_generate()` built its self-signed cert's SAN list
as `[node_name, "localhost"]` only. The apiserver proxies
exec/logs/attach/port-forward requests to nodelet's kubelet-style server
by dialing the node's *address* (`Node.status.addresses`' InternalIP —
the same one `node.rs::detect_internal_ip()` advertises), not its
hostname. Confirmed live against a real k3s apiserver:
`kubectl exec <pod> -- echo hi` failed every time with `x509: cannot
validate certificate for 10.226.246.213 because it doesn't contain any
IP SANs` — zero IP SANs on the cert at all, so this could never have
worked regardless of anything else being configured correctly.

**Fix**: `load_or_generate()` now takes the node's IP as a parameter and
includes it (plus `127.0.0.1`) in the SAN list passed to
`generate_simple_self_signed()` — it parses each string and adds it as a
`SanType::IpAddress` or `SanType::DnsName` automatically, so a literal IP
string is all that's needed. `server::run()` supplies
`node::detect_internal_ip()`. New unit test
(`cert_includes_the_node_ip_as_a_san`) parses the generated cert with
`x509_parser` and asserts the IP SAN is actually present, not just that
generation succeeds. Existing cert/key files on disk are reused as-is
(by design — see the file's own header comment on why), so upgrading
an already-running node needs its stale
`$NODELET_SERVER_CERT_DIR/server.{crt,key}.der` deleted once to pick up
the fix; confirmed live, the very next `load_or_generate()` call
regenerates cleanly.

**Update — the remaining two trust legs are now also fixed (same
session, immediately after this one)**: past the IP-SAN fix,
`kubectl exec` still failed twice more, each a genuinely separate
issue, confirmed by the error message changing each time rather than
the same error reappearing:

1. `x509: certificate signed by unknown authority` — k3s's apiserver
   didn't trust nodelet's self-signed leaf at all. Fixed by pointing
   `--kube-apiserver-arg=kubelet-certificate-authority=` directly at
   nodelet's own cert (a self-signed leaf can vouch for itself when
   explicitly placed in a trust store this way — not a real CA, but
   valid x509). This has a real ordering problem: k3s starts before
   nodelet has ever generated that cert file. Solved with a *second*,
   later call into `setup-control-plane.sh` (`enable_kubelet_
   certificate_authority_trust()` in `lib/control-plane.sh`, run after
   `run_and_verify` — nodelet has started and generated its cert by
   then) rather than reordering the whole bootstrap flow or adding a
   new nodelet CLI mode; `setup-control-plane.sh`'s installer is
   already idempotent/safe to re-run. `server::tls::load_or_generate()`
   now also writes a PEM copy of the cert (`server-ca.pem`, alongside
   the DER rustls actually serves) since kube-apiserver only reads PEM
   — regenerated on every load, not just first generation, so a node
   upgrading from a version predating this self-heals the missing file
   on next restart.
2. `missing or malformed Authorization: Bearer <token> header` —
   nodelet's own server had no client CA configured
   (`NODELET_CLIENT_CA_FILE` unset), so it fell back to bearer-token-only
   auth, but the apiserver authenticates via mTLS client cert here, not
   a bearer token. Fixed by setting `NODELET_CLIENT_CA_FILE` to k3s's
   own client CA (`/var/lib/rancher/k3s/server/tls/client-ca.crt` —
   fixed by k3s's installer) in `nodelet_env_lines()`
   (`lib/nodelet-service.sh`), CRI-runtime-only. No ordering problem
   this direction: k3s writes that file during its own first startup,
   which always precedes nodelet's service install.

**Both verified live** through the real deploy scripts (not manual
edits) — a real config from a clean rebuild + reinstall of both
services, not hand-patched units: `kubectl exec`, `kubectl logs`, and
`kubectl logs -f` all pass against a live pod, and
`lib/test/cases/streaming.sh`'s three real (non-skip) tests
(`test_kubectl_logs_returns_real_output`,
`test_kubectl_logs_follow_streams_new_output`,
`test_kubectl_exec_runs_a_command_and_returns_its_output`) all pass.
`attach`/`port-forward` share `exec`'s exact proxy code path per that
test file's own header, so this is full functional coverage of all
four endpoints this gap was blocking, not just `exec` in isolation.

**Near-miss worth recording**: the first attempt at the apiserver-trust
leg used `--kube-apiserver-arg=kubelet-insecure-tls=true`, a *plausible-
sounding but nonexistent* kube-apiserver flag — confirmed wrong the hard
way, live: `Error: unknown flag: --kubelet-insecure-tls` crashed the
running k3s outright (recovered in seconds by reverting the flag and
restarting). No script ever shipped this — caught and reverted before
being wired into any committed file. The real flag
(`kubelet-certificate-authority`) was verified working by hand before
being wired into `setup-control-plane.sh`/`control-plane.sh` at all.

**Not a code bug — this session's own mistake, corrected here for the
record**: a reboot mid-session revealed `flanneld` permanently broken
("plugin type=flannel failed... loadFlannelSubnetEnv failed", every new
pod stuck `Pending` forever). Root cause: this session ran its own ad hoc
`rm -rf .bootstrap/toolchain` several times as a disk-pressure mitigation
(finding #3), which deleted `.bootstrap/toolchain/bin/flanneld` along
with the actual Rust build cache it was trying to clear. This is **not**
a gap in `bootstrap-source.sh` — `deploy/lib/cleanup.sh`'s own
`cleanup_build_footprint()` already gets this exactly right, and says so
explicitly in its header: "`$TOOLCHAIN_DIR/bin` is mixed: build-only
(cc/gcc/g++/protoc/go) sits next to runtime binaries
(runc/containerd/flanneld) — remove only the named build-only entries,
never glob the whole directory," and its actual implementation does
precisely that (`rm -f "$TOOLCHAIN_DIR/bin"/{cc,gcc,g++,protoc,go}`,
never a blanket delete of the directory). This session's own cleanup
commands bypassed that safety rail instead of calling the project's own
function. Recovered by re-fetching `flanneld-arm64` v0.25.6 from its
GitHub release to the same path. Lesson for future sessions on this
device: use `cleanup_build_footprint()` (or its exact named-file list)
for disk-pressure cleanup, never a blanket `rm -rf
.bootstrap/toolchain`.

### 10. Unresolved (low severity): `readOnly: false` volume mount status comes back absent, not `false`

`containerStatuses[].volumeMounts[].readOnly` for a non-read-only mount
comes back as an absent JSON key on a live pod, not the literal boolean
`false`. Traced nodelet's own code and found it correct as far as I
could follow: `volume_mount_status_tuples()`
(`runtime/cri/volumes_pure.rs`) computes `readonly = false` correctly
for an unspecified `VolumeMount.readOnly`, `volume_mount_statuses_field()`
(`pods.rs`) wraps it as `Some(false)`, and the vendored `k8s-openapi
0.28.0`'s generated `VolumeMountStatus::serialize` does emit
`Some(false)` as `"readOnly": false` (confirmed by reading its generated
source directly — the skip is `if let Some(value) = &self.read_only`,
None-only, not a "skip default" pattern). Didn't find where between
there and the client the field actually disappears — could be
kube-rs's `Patch::Merge` path, JSON Merge Patch semantics at the
apiserver, or something else not yet identified.

**Severity: low, functionally inert** — "absent" and "false" are
semantically identical for this field (matches upstream K8s convention
for many optional bool fields), and
`test_container_status_reports_recursive_read_only`'s own *other*
assertion two lines above (`recursiveReadOnly` should be empty, not an
explicit "unspecified" value, when not read-only) already treats
"absent = default" as correct for the sibling field. Test loosened to
accept either `"false"` or empty rather than requiring the literal
string, rather than continuing to chase this with no live-debugging
access to instrument nodelet's actual patch bytes on the wire. Worth a
closer look with `RUST_LOG=debug` + a raw HTTP capture of the PATCH
body nodelet actually sends, if this ever needs to be pinned down for
real (e.g. if a real client starts depending on the field being
present).

### 11. Verified (not previously tested anywhere): finalizers behave correctly with the Round 103 delete fix

Neither `nodelet`'s own code nor any `lib/test/cases/*.sh` file mentions
finalizers at all — a real gap in coverage for something Round 103's
`teardown()` change (adding a real `Api::<Pod>::delete()` call) could
plausibly have interacted badly with. Tested live rather than left
unverified: created a pod with a finalizer, deleted it, and confirmed
the full lifecycle is correct —

1. `deletionTimestamp` set, finalizer stays (apiserver's own behavior,
   nothing to do with nodelet).
2. nodelet tears the CRI sandbox/containers down immediately regardless
   of the finalizer (`crictl pods` showed nothing left) — correct: real
   kubelet doesn't wait on finalizers to stop containers either,
   finalizers only block *object* removal.
3. The Pod object correctly stays present (`Terminating`, finalizer
   still listed) — `teardown()`'s `delete()` call doesn't error or
   loop against a finalizer-blocked object; logged "torn down" twice
   (matching the usual multi-watch-trigger pattern) and then stopped,
   no flooding.
4. Removing the finalizer by hand purged the object immediately.

No code change needed — this is a clean pass, recorded here because it
was a real untested interaction with Round 103's fix, not because
anything was wrong.

### 12. Sandbox never told CRI it would host a privileged container — fixed, and found live installing a real CSI driver

**Severity: high — blocked every privileged container outright, not
CSI-specific.**

Installed `kubernetes-csi/csi-driver-host-path` (the upstream CSI
sanity/conformance test driver) against this node to actually exercise
nodelet's CSI code paths for real, rather than only unit tests and
clean-skip e2e tests. Every privileged container in the driver's pod
(`hostpath`, `node-driver-registrar`, `csi-attacher`, `csi-provisioner`)
failed `CreateContainer` with `no privileged container allowed in
sandbox` — confirmed via `crictl inspectp` that the sandbox's own
`security.privileged` was `false` even though every container inside it
requested `privileged: true`.

Root cause: `sandbox_config()` (`runtime/cri/sandbox.rs`) never set
`LinuxSandboxSecurityContext.privileged` at all. CRI's own proto comment
on that field says exactly why this matters: "Indicates whether the
sandbox will be asked to run a privileged container. If a privileged
container is to be executed within it, this field has to be set." A
sandbox created without it refuses to host *any* privileged container
afterward, regardless of that container's own securityContext — this
isn't a per-container check, it's a sandbox-wide capability that has to
be declared at `RunPodSandbox` time, before any container exists.

**Fix**: new pure helper `pod_requests_privileged(containers,
init_containers)` computes whether *any* container in the pod (app or
init) asks for `privileged: true`; `ensure_pod()` calls it once
up front (same pattern already used for `port_mappings`/`cgroup_parent`)
and threads it through `run_sandbox()` into `sandbox_config()`'s
`LinuxSandboxSecurityContext.privileged`. 9 new unit tests
(`sandbox_config.rs`: 2 for the sandbox wiring, 6 for the pure helper
including an init-container case, all direct hits on this exact
scenario). Verified live: after the fix, `crictl inspectp` on a freshly
created sandbox for this same pod shows `"privileged": true`, and the
`no privileged container allowed` error is gone entirely from
subsequent `CreateContainer` calls.

**Caveat confirmed while verifying**: an *existing* sandbox created
before this fix keeps its old (non-privileged) `RunPodSandbox` config —
`sandbox_reuse_decision()`'s `Reuse` path never calls `sandbox_config()`
again for a live sandbox, same as any other sandbox-level CRI setting.
A node upgrading to this fix with an already-running privileged
workload needs that workload's pod deleted (not just restarted) so its
sandbox gets recreated — force-deleting the pod (or `crictl rmp` its
sandbox directly) is what actually picks this up; a plain container
restart or nodelet restart alone does not.

### 13. Fixed: `probes::spawn()`'s own supervisor task leaked every per-container probe loop on restart, causing an unbounded restart storm

**Severity: high — a genuine restart storm, confirmed at 810 restart
attempts in 90 seconds for one pod (periodSeconds=2 for the fastest
probe involved would cap that around 45-90 in the same window even at
100% failure) — found integration-testing the CSI driver above, not
CSI-specific itself.**

Once finding #12's fix let the CSI driver's containers actually start,
`hostpath` and `node-driver-registrar` (livenessProbe periodSeconds 2s
and 10s respectively) began restarting far faster than either interval
could produce — `journalctl`'s own timestamps show dozens of "liveness
probe failed; restarting container" lines within the same second,
repeatedly, for many consecutive seconds.

Root cause: `probes::spawn()` used to wrap every per-container probe
loop it spawned inside one outer `tokio::spawn()` task and return only
that outer task's handle. `PodController::stop_probe_supervisor()`
`.abort()`s exactly the handle it's given — Tokio doesn't treat a task
spawned *by* another task as that task's child for cancellation
purposes, so aborting the outer wrapper never touched the inner
per-container loops it had spawned. Every container restart therefore
leaked the *previous* generation's probe loops instead of replacing
them: they kept running forever, independently kept polling on their
own schedule, and each one kept calling `restart_container()` whenever
its own probe failed. Restarts accumulate leaked loops (one set per
restart), so the failure rate compounds every cycle instead of staying
constant — exactly the 810-in-90s runaway observed live.

**Fixed**: `probes::spawn()` (`crates/nodelet/src/probes.rs`) now
returns `Vec<JoinHandle<()>>` — one handle per probed container,
directly, with no wrapping outer task at all. `PodController` now
stores `Vec<JoinHandle<()>>` per pod key and `stop_probe_supervisor()`
aborts every handle in the Vec, not just one. Regression test:
`probes::tests_supervisor::spawn_returns_one_independently_abortable_handle_per_probed_container`
proves `spawn()`'s handle count matches the probed-container count and
that aborting all of them genuinely stops every loop (no more
`restart_container()` calls afterward, even when probes keep failing).

Verified live: after the fix, the CSI driver's containers settle into
a disciplined restart cadence that tracks each container's own
`periodSeconds`/`failureThreshold` (no snowball), instead of restarting
dozens of times per second.

### 14. Found live re-verifying #12: a StatefulSet pod recreated with a new UID kept reusing its previous incarnation's stale sandbox forever

**Severity: high — silently defeats any fix (like #12) that only takes
effect for freshly-created sandboxes, for the very common case of a
StatefulSet pod (stable name across every incarnation) being recreated.**

While re-verifying #12's privileged-sandbox fix against the CSI driver
(scaled its StatefulSet to 0 then back to 1 to get a clean pod), the
*same* "no privileged container allowed in sandbox" error kept
happening — with the newly-built, correctly-fixed binary already
running. Root cause: `CriRuntime::find_sandbox()` looks a pod's sandbox
up by namespace+name label only, never pod UID. A StatefulSet pod's
name is stable across every incarnation, so when it's recreated (new
UID) before its *previous* sandbox is torn down/GC'd, the namespace+name
match finds that old, `Ready`, but stale sandbox and reuses it
unconditionally — built for an entirely different pod object, from
before the fix even existed. `gc_orphaned_sandboxes()` can't clean this
up either: its own orphan check is keyed by the same namespace+name, and
a live pod with that key still exists (just a different UID), so the
stale sandbox never looks orphaned.

**Fixed**: added `find_sandbox_with_uid()` (returns the sandbox's own
recorded pod UID alongside its id/state) and made `sandbox_reuse_decision()`
UID-aware — a namespace+name match with a mismatched UID is now always
treated as `RecreateStale` regardless of CRI state, tearing the old
sandbox down and building a genuinely fresh one. Regression tests:
`runtime::cri::tests_sandbox_reuse::ready_sandbox_with_mismatched_uid_is_recreated_not_reused`
and `mismatched_uid_overrides_ready_state_every_time`.

### 15. Found live re-verifying #12/#14: `CreateContainerRequest`'s own redundant `sandbox_config` field was hardcoded to `privileged: false`, silently overriding the sandbox's real privileged flag

**Severity: high — the actual remaining root cause of "no privileged
container allowed in sandbox" even against a genuinely fresh,
correctly-`privileged: true` sandbox (post #12 and #14).**

CRI's `CreateContainerRequest` carries its own `sandbox_config` field —
"the same config that was passed to `RunPodSandboxRequest`... passed
again here just for easy reference" per the proto's own doc comment.
containerd's CRI plugin actually *reads this copy back* to decide
whether a privileged container may be created — not whatever was
stored at `RunPodSandbox` time. Confirmed via `crictl inspectp`'s
`info.config` (the real stored `PodSandboxConfig`, as opposed to
`status.linux`, which doesn't carry a security context at all and
was an earlier red herring while investigating this): the sandbox
itself was genuinely `privileged: true`. Yet `container_create.rs`'s
`create_and_start_container()` was building this redundant copy with
`sandbox_config(id, None, &id.name, &HashMap::new(), None, false)` —
a hardcoded `false`, unconditionally, for every container, regardless
of what the pod actually required. A leftover from #12's fix: the
mechanical pass that added the new `privileged` parameter to every
`sandbox_config()` call site added a trailing `false` here too, since
this call site isn't the one that creates the sandbox and wasn't
re-examined for what it should actually be passing.

**Fixed**: threaded a `privileged: bool` parameter down through
`create_and_start_container()` → `ensure_container()` /
`ensure_ephemeral_container()` / `ensure_init_containers()`, computed
once in `ensure_pod()` (the same `pod_requests_privileged()` call #12
already added) and passed through every call site, so the redundant
`sandbox_config` in `CreateContainerRequest` always matches what
`RunPodSandbox` actually used.

Verified live end-to-end: after all three of #13/#14/#15, force-deleting
the CSI driver's StatefulSet pod produces a fresh pod that reaches
`5/5 Running` immediately, with a disciplined (non-snowballing) restart
cadence thereafter — CSI driver testing can now resume.

### 16. Fixed: `command`/`args`' `$(VAR)` references were never expanded against a container's own env vars

**Severity: high — the actual remaining cause of the "disciplined restart
cadence" #13/#14/#15 left behind: `hostpath` (the real CSI driver binary)
was still restarting every ~10-15s, and it turned out to be crashing for
real, not the storm bug at all.**

Found live re-verifying #13/#14/#15 with the CSI StatefulSet scaled back
up: `hostpath`'s own container spec sets `--endpoint=$(CSI_ENDPOINT)`,
referencing a sibling `env: [{name: CSI_ENDPOINT, value:
unix:///csi/csi.sock}]` entry — Kubernetes' standard "dependent
environment variables" feature, where `$(VAR_NAME)` in `command`/`args`
is substituted against the container's own resolved env before it's
exec'd. nodelet never did this substitution at all. The driver's own
logs confirmed it: `Listening for connections on address:
&net.UnixAddr{Name:"/$(CSI_ENDPOINT)", Net:"unix"}` — it was trying to
bind its gRPC CSI socket at the *literal* path `/$(CSI_ENDPOINT)`
instead of `unix:///csi/csi.sock`. The socket never came up, so the
driver failed its own liveness probe (`period=2s #failure=5`) every
~10s and got killed and restarted forever by nodelet's (entirely
correct, by this point) restart-on-exit logic — which then cascaded
into the `csi-attacher`/`csi-provisioner`/`node-driver-registrar`
sidecars, all of which also crash-looped since they could never dial a
socket that didn't exist.

**Fixed**: new `expand_command_arg()` in `volumes_pure.rs` — the same
`$(VAR)`/`$$`-escaping grammar `expand_sub_path_expr()` (round 69)
already implements for `subPathExpr`, but with real kubelet's actual
(more lenient) semantics for `command`/`args`: an unresolved `$(VAR)`
is left as literal text rather than failing the container, matching
`expandContainerCommandAndArgs()` upstream. Applied to both `command`
and `args` in `container_create.rs`'s `ContainerConfig` construction.
7 new regression tests in `cri_tests/command_args_expansion.rs`.

Verified live end-to-end: after rebuilding and redeploying, the CSI
driver's `hostpath` container's own logs show it binding the real
socket and answering `csi.v1.Identity/Probe` gRPC calls, and the pod
ran `5/5 Running` with **zero restarts across all 5 containers** over
a full 5 minutes of observation (previously: 12+ restarts on `hostpath`
alone within the first 2 minutes). CSI driver testing can now actually
proceed past pod startup.

### 17. Fixed: nodelet never reconciled `CSINode`, and never synced CSI topology labels onto the Node — a topology-aware provisioner could never provision anything

**Severity: high — every PVC backed by a topology-aware CSI provisioner
(`csi-provisioner --feature-gates=Topology=true`, the common default
for real CSI drivers, including this project's own bundled deploy
manifest) permanently failed to provision, with no way to recover
short of disabling topology awareness in the provisioner itself.**

Found running the CSI e2e suite for real (`TEST_CSI_STORAGE_CLASS` set
against the now cleanly-running hostpath driver from #16):
`test_pod_mounts_a_persistent_volume_claim` skipped — "PVC never became
Bound within 60s". `kubectl logs ... -c csi-provisioner` showed a
permanently repeating `"error generating accessibility requirements: no
available topology found"`. `kubectl get csinodes -o yaml` showed
`spec.drivers: null` — nodelet had never implemented any `CSINode`
reconciliation at all (`grep -rn CSINode crates/nodelet/src` was
entirely empty before this fix). Real kubelet's own "Node Info Manager"
does this automatically the moment a CSI driver registers via the
plugin-registration protocol: it calls `NodeGetInfo`, then reconciles
`{name, nodeID, topologyKeys}` onto `CSINode.spec.drivers[]`.

**Fixed, first pass**: new `csi_node.rs` — pure `upsert_driver()`/
`remove_driver()` list-update functions (7 unit tests), plus thin async
`upsert()`/`remove()` wrappers doing a read-modify-write against the
real `CSINode` API object (full-list JSON merge patch, since merge
patch doesn't deep-merge array elements). Wired into
`plugin_registry.rs`'s `register_one()` (upsert on registration) and its
deregistration path (remove when a driver's socket disappears). A new
`CsiDrivers::node_info()` in `runtime/csi.rs` makes the actual
`NodeGetInfo` RPC call.

**That alone wasn't enough**: redeploying and re-running the PVC test
showed progress but a *different* failure —
`"topologyKeys [topology.hostpath.csi/node] were not found on any
nodes"`. `csi-provisioner` reads `topologyKeys` off `CSINode` to know
which label *keys* matter, then reads the actual *values* off the Node
object's own labels to build `TopologyRequirement` — real kubelet's
other half of the Node Info Manager, which the first pass had
deliberately (and, it turned out, incorrectly) scoped out as unlikely
to matter. Fixed with a second function, `node.rs`'s
`apply_topology_labels()` — merge-patches `NodeGetInfo`'s
`accessible_topology` segments onto the Node's labels, called from the
same `plugin_registry.rs` registration path right after the `CSINode`
upsert.

Verified live end-to-end: `kubectl get node debian` now carries
`topology.hostpath.csi/node: debian`, `kubectl get csinodes` shows the
matching `topologyKeys`, and the previously-skipping
`test_pod_mounts_a_persistent_volume_claim` now passes, along with
`test_pod_with_an_attach_required_pvc_waits_for_volumeattachment`,
`test_csi_ephemeral_inline_volume_is_mounted`, and
`test_pod_mounts_a_generic_ephemeral_volume` — the full set of CSI e2e
tests that need a real, bindable PVC.

### 18. Fixed: three separate, real bugs found standing up a real DRA driver (kubernetes-sigs/dra-example-driver) — the reference driver real Kubernetes e2e/conformance tests use for CDI/GPU passthrough, same role csi-driver-host-path plays for CSI

**Severity: high — Dynamic Resource Allocation (CDI/GPU device passthrough)
was completely non-functional end-to-end, on three independent layers,
despite round 63/64's implementation looking complete on inspection.**

Deployed `kubernetes-sigs/dra-example-driver` (Helm chart, `numDevices=4`,
paths pointed at nodelet's `NODELET_PLUGIN_REGISTRY_PATH`) to actually
exercise DRA for the first time against a real driver, exactly the "use
whatever kubelet uses for their testing" approach that verified CSI in
rounds 117-120. Registration itself worked immediately (`plugin registry:
plugin registered ... plugin_type=DRAPlugin`), but nothing downstream did,
in three separate ways, each requiring root-causing and fixing in turn:

**Bug 1 — projected ServiceAccount tokens were never bound to their Pod.**
The reference driver's own Helm chart ships a `ValidatingAdmissionPolicy`
gating `ResourceSlice` writes on the requesting token carrying
`authentication.kubernetes.io/node-name` (real Kubernetes 1.36's
`ServiceAccountTokenPodNodeInfo`, GA/always-on). That claim is only
populated for tokens whose `TokenRequestSpec.boundObjectRef` points at a
Pod with a resolvable `spec.nodeName` — nodelet's own
`resolve_service_account_token()` hardcoded `bound_object_ref: None` for
every projected `serviceAccountToken` volume. **Fixed**: threads
`(pod_name, pod_uid)` through to set a real `BoundObjectReference{kind:
Pod, ...}`, matching real kubelet. A real security property too, not just
this fix: an unbound token stays valid after its pod is deleted, unlike
real kubelet's.

**Bug 2 — `ResourceClaim` was fetched against the wrong, now-removed API
version.** `resource.k8s.io/v1beta1` doesn't exist on any cluster running
Kubernetes ≥1.34 (DRA graduated to GA as `resource.k8s.io/v1`) —
confirmed via `kubectl get --raw /apis/resource.k8s.io`, only `v1` listed.
Every `ResourceClaim` GET 404'd silently (claims.rs logs and skips rather
than failing the pod), so a container requesting a device always started
without it, no error visible anywhere except a WARN log easy to miss.
**Fixed**: rather than bumping the workspace's pinned `k8s-openapi`
schema feature (`v1_33` → `v1_36`, attempted first — turned out to ripple
into unrelated breaking field renames across the whole codebase, a much
bigger and riskier change than this fix warrants), `ResourceClaim` is now
fetched via a raw request into a small hand-written `RawResourceClaim`
struct (only the fields DRA actually reads), the same raw-request pattern
`resolve_service_account_token()` already uses for a subresource
k8s-openapi 0.28 doesn't generate a helper for either.

**Bug 3 — the reconstructed gRPC proto was wrong.** `proto/draplugin.proto`
carried an explicit caveat since round 63 ("reconstructed from public
documentation... NOT validated against a live third-party DRA driver") —
now definitively confirmed broken, live: `NodePrepareResources` failed
outright with `unknown service dra.v1beta1.DRAPlugin`. The real service
package is `k8s.io.kubelet.pkg.apis.dra.v1` (again, DRA graduated past
beta before this was originally written), and the `Device` message's
field layout was also wrong (missing `pool_name`/`device_name`/`share_id`,
`cdi_device_ids` at the wrong field number). **Fixed**: `draplugin.proto`
rewritten by transcribing `k8s.io/kubelet/pkg/apis/dra/v1/api.proto`
(kubernetes/kubernetes staging) directly — the same source a real
driver's own generated stubs come from.

**A fourth, non-nodelet gap found along the way**: containerd's own
`enable_cdi` defaults to `false` — without it, `ContainerConfig.cdi_devices`
is silently accepted and does nothing at the OCI spec generation layer,
regardless of anything nodelet does correctly. Fixed in
`deploy/lib/container-runtime.sh` (flips it on for every fresh bootstrap)
and applied live. Also needed a real k3s apiserver feature gate
(`--kube-apiserver-arg=feature-gates=ServiceAccountTokenPodNodeInfo=true`,
already GA/on-by-default in 1.36 but harmless/idempotent to set
explicitly) added to `deploy/setup-control-plane.sh` for completeness —
this one turned out to be a red herring for the actual fix (Bug 1 above
was the real blocker), but is documented and left in since it costs
nothing and matches what a real 1.36 cluster already does anyway.

Verified live end-to-end, all the way through: a Pod with a
`resourceClaims`/`ResourceClaimTemplate` referencing the reference
driver's `gpu.example.com` DeviceClass reaches `Running`, and `kubectl
exec`'ing into it shows real CDI-injected environment variables the
driver's `NodePrepareResources` response specified —
`GPU_DEVICE_0=gpu-0`, `GPU_DEVICE_GPU_0_RESOURCE_CLAIM=<uuid>`,
`DRA_RESOURCE_DRIVER_NAME=gpu.example.com`, `DRA_ADMIN_ACCESS=false`,
`GPU_DEVICE_0_SHARING_STRATEGY=TimeSlicing` — proof the full chain
(registration → `ResourceSlice` publish → scheduler allocation →
`NodePrepareResources` → CDI injection via containerd) genuinely works,
not just individual pieces in isolation.

### 19. Fixed: a probe-triggered container restart never actually recreated the container — found live when CoreDNS crash-looped for the rest of a CI run

**Severity: critical — a real, previously-undiscovered production bug,
not a test artifact. Any liveness/startup probe failure could
permanently kill a container with no automatic recovery, for any
workload, not just test pods.**

Found running the full e2e suite in GitHub Actions CI for the first
time (round 123): CoreDNS started failing its liveness probe once
(cause unconfirmed — likely ordinary cold-start jitter on a fresh
runner), and from that point on, restarted "successfully" every single
probe cycle (every few seconds) for the rest of a 30+ minute run,
without ever actually coming back — visible in nodelet's own logs as
an unbroken stream of `WARN liveness probe failed; restarting
container pod=kube-system/coredns...` lines, one per probe period, with
no gap. DNS being down for the whole run then cascaded into ~15
unrelated test failures across totally different categories (PID
namespaces, OOM score, pod status fields, cgroup enforcement variants)
— anything needing DNS resolution, and (once fail-fast + per-test
diagnostics were added to actually see node state at each failure)
confirmed the node itself was healthy the entire time (Ready, no
pressure, no taints) — the breakage was entirely inside nodelet's own
container lifecycle handling, not the cluster/environment.

**Root cause**: `runtime::cri::pod_runtime_impl.rs`'s `restart_container()`
(what every probe failure calls) only ever stops and removes the old
container — it was never responsible for creating a new one. That was
always implicitly left to `ensure_pod()`, called from `pods.rs`'s
`reconcile()`. But `reconcile()` only runs in response to a **watch
event on the Pod object itself** (or a ConfigMap/Secret it references
changing) — a probe-triggered restart is a purely internal action
nodelet takes on its own, generating no such event. Unless something
*else* happened to touch that Pod's object again, nothing ever
re-triggered `ensure_pod()`, and the removed container stayed removed
forever — every subsequent probe check found no container, "restarted"
it (a no-op, since `find_container_id()` already returned `None`), and
logged the exact same warning again next cycle, permanently.

**Fixed**: `probes.rs` gained `restart_and_reensure()` — after
`restart_container()` succeeds, re-fetches the live Pod object via the
apiserver and calls `ensure_pod()` again immediately, matching real
kubelet's own `SyncPod` semantics (a probe-triggered restart is
kill-then-immediately-start, one atomic sync, not kill-and-hope).
Needed threading a `kube::Client` through `probes::spawn()`/
`probe_container()` (previously only had a `PodRuntime` handle, no way
to re-fetch the Pod object). Reuses the exact same `ensure_pod()` path
every other pod-creation flow already goes through — no new
container-creation logic, no duplicated volume/pull-secret/claim-device
resolution.

### 20. Fixed: `nodescheduler`'s first live run — a Pending pod that explained nothing, and a self-inflicted hot loop in the fix for it

The first time `crates/nodescheduler` was deployed to a real cluster
(`SCHEDULER=nodescheduler`, k3s's own scheduler disabled), 277 unit tests
were green and three of the nine new e2e cases failed immediately. Two
findings came out of it, and the second is the more interesting one because
this project's own fix caused it.

**Found**: `deploy/lib/test/cases/scheduler.sh` asserted that a pod
requesting more CPU than any node advertises reports
`status.conditions[PodScheduled] = False`. It reported nothing at all — no
condition, no event.

**Root cause**: the scheduling loop handled `CycleOutcome::Unschedulable` by
parking the pod in the queue and logging at `debug`, and never told the
apiserver anything. That is the worst diagnostic state Kubernetes has: the
pod sits Pending, `kubectl describe` shows an empty Events section, and
there is no way to distinguish "no node has enough CPU" from "the scheduler
is not running at all". Cluster-autoscaler also keys off that condition to
decide whether to add a node, so its absence is not cosmetic.

**Fixed**: `report.rs` writes both halves — the machine-readable condition
(`reason: Unschedulable`, plus `nominatedNodeName` when preemption has
promised one) and a `FailedScheduling` Warning event for `kubectl
describe`. Spawned off the scheduling loop: two apiserver round trips per
failed pod, on a loop that handles one pod at a time by design, would let a
burst of unschedulable pods throttle the schedulable ones queued behind
them. A gated pod still gets neither, structurally rather than by a check —
`PreEnqueue` rejections never reach a scheduling cycle.

**Then that fix closed a loop.** `watch.rs` treated every pod update as a
fresh arrival: it re-projected with `queued_at = now` and called
`SchedulingQueue::add`, which only deduplicated against the *active* queue,
so a pod parked in `unschedulable` could sit in both containers at once.
With `report.rs` in place that becomes a spin:

```text
cycle fails -> report patches the pod's status to say why
            -> apiserver emits a pod update
            -> add() pushes it back to active, skipping backoff
            -> cycle fails -> ...
```

at full speed, forever, hammering the apiserver on behalf of a pod that
simply does not fit. Neither half is a bug alone — reporting a failure is
correct, and re-queueing on a pod change is *nearly* correct — which is
exactly why unit tests on either side stayed green.

**Fixed**: `SchedulingQueue::update` replaces the stored object where the
pod already is and leaves its position alone. An edit is not a reason to
retry; only a cluster event some plugin subscribed to is, and that arrives
through `move_all_to_active_or_backoff`. It carries forward the two fields
that belong to the queue rather than to the API object — `queued_at`
(resetting it starves precisely the pods being retried most, which is every
failed pod, because being told why it failed *is* an edit) and `attempts`
(without which backoff stays pinned at its 1s floor).

**What the unit tests could not have caught, and why.** Every plugin tests
its own predicate in isolation and `cycle.rs` tests its arithmetic in
isolation; nothing ran the real PreFilter and the real Filter together and
checked what the cycle concluded. The first attempt to reproduce the live
failure as a unit test *passed*, because it hand-built `PodInfo` and so
skipped `PodInfo::from_pod` -> `pod_requests` -> `parse_quantity_*`, which
is what the live path hits first. `cycle_tests.rs` now starts from a real
`Pod` object and runs through to a cycle outcome; any future test of a
placement decision should start there rather than at `PodInfo`.

**Also**: `deploy/lib/e2e-debug-dump.sh` now dumps `nodescheduler`'s unit
status, journal and the `kube-scheduler` lease. The first run had to be
diagnosed almost blind — the dump tailed nodelet and said nothing about
what had actually made the placement decision.

### 21. Fixed: the apiserver rewrites what you submit — `cpu: "10000"` comes back as `"10k"`, and the scheduler read it as zero

The unexplained half of finding 20. A pod requesting more CPU than the whole
cluster has was **bound** to a 4-core node, while every unit test of that
exact path — including one driving a real `Pod` object through the
projection and the entire scheduling cycle — correctly rejected it.

**Found**: by making the e2e assertion print the two numbers the fit check
is actually made from, instead of reasoning about them for a third time:

```text
pod requests:     {"cpu":"10k"}
node allocatable: {"cpu":"4", ...}
```

**Root cause**: the pod was submitted as `cpu: "10000"`. Kubernetes
canonicalises quantities to their shortest form on admission, so what comes
back over the watch is `"10k"` — **the string a scheduler sees is not the
string anyone wrote**. `parse_quantity_milli` handled only the `m` suffix
and bare numbers, so `"10k"` fell through to a bare `f64::parse`, failed,
and hit `.unwrap_or(0)`.

The pod therefore read as requesting *zero* CPU. `Resources::names()`
returned nothing for it, the fit loop had nothing to iterate, and it fit
anywhere. Fail-open, on the one code path where failing open means
overcommitting a node.

**Fixed**: one parser covering the whole spec — binary (`Ki`..`Ei`), decimal
SI (`m`, `k`, `M`..`E`, lowercase `k` only), and decimal exponents,
including the `1E` (exa) versus `1E3` (thousand) ambiguity, which differ by
fifteen orders of magnitude. An unknown suffix returns zero *and warns*
rather than guessing base units, since with the full set handled it should
be unreachable.

**Why 303 unit tests missed it, and one endorsed it.** Every fixture used a
spelling the parser already handled, because the same person wrote both —
the tests probed the author's assumptions rather than the apiserver's
behaviour. Worse, `an_unparseable_quantity_reads_as_zero_rather_than_
panicking` asserted the fail-open default *as intended behaviour*, under a
name that made it sound like defensive hygiene. A test can hold a bug in
place.

The replacement is a property rather than a table: no spelling of a CPU
request may ever parse as zero, checked across every canonical form the
apiserver emits.

**Three smaller bugs from the same run**, all in retry or reporting paths
that looked obviously correct:

  * `PatchParams::apply(..).force()` with `Patch::Strategic` is rejected by
    the *client*, before the request leaves the process — `force` is
    server-side-apply only. Every `PodScheduled=False` write failed locally,
    costing one warning and nothing else, which from outside is
    indistinguishable from a scheduler with nothing to say. The diagnostic
    hole finding 20 closed, reopened by the call that closed it.
  * The watch loop spun at full CPU whenever the apiserver was down
    (`WatchStartFailed` returns instantly on every poll), which is a real
    window on every deploy, since `setup-control-plane.sh` runs twice. `kube`
    self-heals an *interrupted* stream but does not pace one that cannot
    start.
  * The backoff ceiling then chosen for that fix, 30s, contradicted the
    comment directly above it: doubling reached the ceiling while the
    apiserver was still down and slept through its return, so recovery took
    ~72s. Now 5s, with the test asserting the bound rather than restating
    the constant.

**The pattern worth keeping.** Three of these sat in the gap between what a
comment claimed and what the code did. The comments were not stale — they
were written at the same time, and were simply wrong about the library or
about the value beneath them. Re-reading code against its own documentation
found as many real bugs here as running it did.

### 22. Fixed: `nodecontroller` had no per-controller ServiceAccount
impersonation — every controller ran as one over-privileged identity

**Severity: real gap, now fixed in `nodecontroller` itself.**

Found live bootstrapping a cluster against a real upstream apiserver
(2026-08-22, in a separate branch prototyping a from-source, non-k3s
bootstrap path): `nodecontroller` was started with
`--use-service-account-credentials=true` (the same flag real
`kube-controller-manager` takes) and its own kubeconfig. Its
`root-ca-cert-publisher` controller immediately 403'd trying to create
`ConfigMap`s:

```text
failed to create kube-root-ca.crt ConfigMap namespace=kube-public
error=... "configmaps is forbidden: User \"system:kube-controller-manager\"
cannot create resource \"configmaps\"..."
```

**Root cause**: real upstream `kube-controller-manager`, when given
`--use-service-account-credentials=true`, does not run its ~30 controllers
under its own client identity at all. It uses that identity only to create
and hand out a narrowly-scoped `system:serviceaccount:kube-system:
<controller-name>` token per controller (e.g. `root-ca-cert-publisher`,
`node-controller`, `namespace-controller`), each bound to its own tightly
scoped `system:controller:<name>` bootstrap `ClusterRole` — this is exactly
why the flag exists, and exactly why upstream's own bootstrap policy keeps
`system:kube-controller-manager`'s own role deliberately narrow (mostly
leader-election machinery, not workload permissions).

`grep`ing `crates/nodecontroller/src/` for `use_service_account_credentials`,
`impersonat`, or `system:controller:` returned **nothing** — not present
anywhere, including `config.rs`'s own CLI/env surface. Every controller
shared one client, authenticated once as whatever identity the process's
own kubeconfig carried. On this repo's own k3s-based bootstrap
(`deploy/lib/run.sh`), that identity is `/etc/rancher/k3s/k3s.yaml` —
cluster-admin — so the gap was masked here: cluster-admin can do anything
any controller needs, so nothing ever 403'd. It only surfaced against a
real upstream apiserver bootstrapped with `nodecontroller` running as a
narrowly-scoped identity instead, which is the architecture
`--use-service-account-credentials=true` claims to implement.

**Suspected same-cause, not directly confirmed before the fix**: the
`node.kubernetes.io/not-ready` taint on a freshly-registered Node not
clearing in time for `nodescheduler` to bind a pod to it —
`node-lifecycle-controller` needs to patch `Node.spec.taints`, and if that
403'd under the same blanket identity, silently (no test asserted on this
specific call succeeding), the pod would stay `Pending` forever with
nothing pointing at why.

**Fixed properly, in `nodecontroller` itself**: `crates/nodecontroller/src/lib.rs`
gained `upstream_controller_sa()` (mapping each of this crate's ~20
controllers to the real upstream `system:serviceaccount:kube-system:<name>`
identity it corresponds to) and `impersonated_client()` (builds a
`kube::Client` per controller carrying `Impersonate-User`/
`Impersonate-Group` headers for that identity, via `kube::Config`'s
`headers` field). `run()` now builds one impersonated client per
controller instead of `client.clone()`-ing a single shared one. On a
cluster-admin-backed kubeconfig (this repo's k3s bootstrap today) this
needs no additional RBAC — cluster-admin can already impersonate any
identity; a bootstrap that authenticates `nodecontroller` as a
narrower identity instead (as a from-source, non-k3s bootstrap path will)
additionally needs an `impersonate` grant scoped to the SA/group names
`impersonated_client()` actually uses, so that RBAC then authorizes each
controller's real requests against that narrower identity's own existing
`system:controller:<name>` bootstrap role, the same as real upstream.

**Two wrong mappings found and fixed one round later**
(`daemonset-controller`/`resourceclaim-controller` instead of the real
`daemon-set-controller`/`resource-claim-controller` — confirmed by dumping
a live cluster's actual `system:controller:*` ClusterRoles rather than
trusting memory a second time).

**Bigger architectural point, found the same round**: per-controller
impersonation covers **writes**, not reads. Real upstream's own
`createClientBuilders()` (`cmd/kube-controller-manager/app/
controllermanager.go`) builds two client builders — a `rootClientBuilder`
(base identity, backs the shared informer factory nearly every controller
reads through) and a `clientBuilder` (per-controller impersonation, used
for each controller's own writes). Confirmed against a live dump:
`system:controller:node-controller`'s real rules have no
`coordination.k8s.io` `leases` permission at all, despite
`node-lifecycle-controller` needing to watch node leases — that read was
never meant to come from the per-controller identity.

`nodecontroller`'s own `SharedWatch` (`watch.rs`) already deduplicates
reads across controllers via one watch per resource type (a `OnceLock`
per type) — which made the first version of this fix doubly wrong for
shared reads specifically: only the *first* controller to reach a given
`OnceLock::get_or_init` actually determined which identity's permissions
that shared watch ran under, for every controller subscribed to it,
non-deterministically depending on task scheduling. Fixed by giving
`watch.rs` its own explicit base-identity client
(`set_base_client`/`base_client()`), set once in `lib.rs::run()` before
any controller starts — every shared/dedup'd read now always uses the
base identity, matching upstream's real split, and no longer depends on
which controller happens to start first.

**Verification**: proven end to end (real per-controller impersonation,
real RBAC-scoped identities, a real pod scheduled and Running) against a
from-source, non-k3s bootstrap prototype's own CI. That prototype crate
isn't merged yet; this fix landed on its own because `nodecontroller`
already ships on `main` and was running with the broken blanket-identity
model described above regardless of which bootstrap path fronts it. Not
independently re-run against this repo's k3s-based e2e suite as part of
this change — see the PR this finding shipped with for which gates did run.

### 23. Not a nodebootstrap bug: `nodebootstrap-e2e.yml`'s own CI never
exercised the real CRI/containerd/CNI path, only the mock runtime

**Severity: real gap in the e2e workflow's coverage, not in nodebootstrap
itself -- found live scoping the `e2e.yml`/`release.yml` cutover.**

`crates/nodebootstrap/src/containerd.rs` and `cni.rs` were marked "✅ real,
every tier" in `docs/NODEBOOTSTRAP_PLAN.md`'s status table from reading
the code -- but `nodebootstrap-e2e.yml` (the workflow that produced that
confidence) installed `nodelet` without `NODELET_RUNTIME=cri` (so it
silently ran the mock runtime) and never called `nodebootstrap
containerd`/`cni` at all. The smoke test's `--image=does-not-matter` only
ever "worked" because nothing real was pulling or running it. This is
exactly the gap this whole methodology exists to catch, caught against
its own tooling this time.

**Fixed**: `nodelet` now builds `--features cri` (a superset of mock --
`NODELET_RUNTIME` still selects which at runtime), the workflow now runs
`nodebootstrap containerd` and `nodebootstrap cni` for real, installs
`nodelet` with `NODELET_RUNTIME=cri`, and the smoke test runs
`registry.k8s.io/pause:3.9` (a real image, actually pulled and run by
containerd) with a `crictl pods`/`ps` dump proving it.

**Real bug found immediately by turning this on**: the new "verify
flanneld wrote a subnet lease" check failed on the first real run --
`flanneld`'s kube-subnet-mgr mode needs the Node *object* to exist before
it can patch backend data onto it (`Failed to get node for backend data:
nodes "runnervm76f27" not found`, then `timeout contacting kube-api`
after ~30s), and at the point `nodebootstrap cni` runs, `nodelet` (which
creates that Node object on registration) hasn't run yet -- it's ordered
several steps later, matching `lib.rs::run_all()`'s own documented
ordering (`cni` runs right after `nodescheduler`/`nodecontroller`, before
`nodelet`/`nodeproxy`).

**Turned out not to be an ordering bug**: `deploy/bootstrap-source.sh`
has exactly the same gap (`ensure_cni` at line 437, `run_and_verify`
which installs nodelet at line 441 -- CNI before nodelet there too), and
it works in production because `flanneld.service` carries
`Restart=always`/`RestartSec=5s` (`service_mgr.rs`) -- the first attempt
genuinely fails, but it keeps retrying every 5s and succeeds once nodelet
registers the node moments later. The real bug was in the *new
verification step itself*: it asserted `/run/flannel/subnet.env` existed
immediately after installing `cni`, before `nodelet` had run at all --
an assertion neither the shell version nor `nodebootstrap` ever made.
Fixed by moving that check to after node registration/Ready (where a
real subnet lease can actually be expected), not by reordering
`run_all()` or `cni.sh`, which were both already correct.

**The pattern worth keeping**: turning on a supposedly-passing e2e gate
found a real gap in the gate itself within one dispatch. The fix a
plausible-looking failure suggests first (reorder the bootstrap) was the
wrong one; reading the actual error against the actual retry semantics
(`Restart=always`) found the real one (fix the check's timing) instead.

### 24. Fixed: release run 50 started the reference drivers without proving
the replacement control-plane read path or nodelet's DRA registration

**Severity: high — four failures and a warning storm in the 0.7.0 release
gate, fixed in the bootstrap and e2e harness.**

Release pipeline run 50 (2026-08-23) failed in two independent ways. A CSI
PVC stayed Pending long enough for `test_node_reports_volumes_in_use_for_a_csi_volume`
to fail, and a DRA claim pod remained Pending because `nodescheduler` reported
that no node had an available device. The same run logged repeated 403 watch
errors for `system:kube-controller-manager` on PVCs and for
`system:kube-scheduler` on CSI/DRA resources. Both replacement binaries were
running, but their shared informer clients used base identities whose built-in
bootstrap roles did not cover the unconditional watch set implemented by
`nodecontroller/src/watch.rs` and `nodescheduler/src/watch.rs`.

**Root cause and fix**: `nodebootstrap` now applies narrowly-scoped
`get`/`list`/`watch` supplements for the exact PV/PVC, storage/CSI, and DRA
resources each base identity reads. The shell bootstrap path applies the same
roles before starting the services. Controller-specific writes remain on the
existing impersonated ServiceAccount grants. An e2e RBAC test calls
`kubectl auth can-i` for every grant so a future missing rule fails directly
instead of surfacing as a provisioning timeout.

The DRA setup had a second readiness gap: it checked for a ResourceSlice but
did not prove that the fresh DRA registrar had registered with nodelet. After
the driver pod was recreated, nodelet retained a dead registrar socket and
repeatedly logged connection-refused warnings. The setup now removes only
that driver's stale registration sockets and waits for a fresh nodelet
`plugin registered` event before exporting the DRA test variables. It also
creates a temporary PVC and requires it to become Bound before the CSI/DRA
tests begin, turning the shared provisioning path into an explicit gate.

Finally, the fake device-plugin test asserted `allocatedResourcesStatus` in
the same instant the pod became Running. Allocation and the subsequent Pod
status write are separate asynchronous events; the run-50 failure occurred
after `Allocate()` had succeeded and the container had the expected device
environment. The test now waits for the Healthy status entry before asserting
it, and device-health notifications use a priority runtime-event channel so
they cannot sit behind a busy CRI container-event queue.

### 25. Fixed: nodebootstrap's flannel readiness barrier initially waited for
the controller that allocates its prerequisite PodCIDR

**Severity: bootstrap-fatal — found live while rerunning the run-50 focused
e2e gate.**

After the run-50 fixes moved the final apiserver restart ahead of the
replacement control-plane services, the combined `nodebootstrap` path started
`nodelet` and then waited for `/run/flannel/subnet.env` before starting
`nodecontroller`. That cannot succeed: flannel's kube-subnet-manager needs a
Node `spec.podCIDR`, and nodecontroller's node-ipam controller is what assigns
that CIDR. Both focused shards failed after 30 seconds with no subnet file;
this was a deterministic dependency cycle, not a flannel networking failure.

**Fixed**: nodecontroller now starts immediately after the final apiserver
restart and RBAC barrier, before the flannel wait. Nodescheduler remains after
the wait so no workload is placed while CNI is still acquiring the subnet.
The e2e failure dump now passes the nodebootstrap kubeconfig explicitly,
falls back to sudo when the bootstrap ran as root, and includes flanneld's
unit status/logs; the previous dump's `/etc/rancher/k3s` errors obscured this
diagnosis.

### 26. Fixed: a late Pod watch event could destroy a same-name replacement

**Severity: high — found while bringing the reference DRA driver up against
the replacement apiserver.**

The DRA setup deliberately deletes and recreates its DaemonSet Pod so the
fresh registrar socket is exercised. The replacement Pod received a new UID,
but nodelet could still process an older watch object for the deleted UID.
Because runtime sandbox lookup was keyed by namespace/name, nodelet treated
that stale object as authoritative and alternated between tearing down the
new sandbox and recreating the old one. The driver consequently never stayed
alive long enough to publish a `resource.k8s.io/v1` `ResourceSlice`; the API
group itself was already discoverable and listable.

Nodelet now rejects Pod watch objects older than the accepted etcd
`resourceVersion`, retains the UID across delete events, and refuses a
UID-scoped teardown when the matching sandbox belongs to another Pod
incarnation. The DRA e2e coverage now directly waits for a published
ResourceSlice, and failure diagnostics include the DRA Pod's description and
current/previous container logs.

### 27. Fixed: independent informer order could bind a static WaitForFirstConsumer volume too early

**Severity: high — found in full e2e run 33835282561 and reproduced with a
deliberately incomplete manifest graph.**

The API can accept a Pod that names a PVC, a PVC that names a StorageClass,
and a PV that names the same class before any of those dependencies has
reached every controller's local cache. The persistent-volume binder received
the matching PV/PVC before its StorageClass informer had delivered the class,
treated the cache miss like an Immediate class, and bound a static PV before
the scheduler had selected a node. The scheduler had the same class-cache
ordering gap: a Pod rejected while its named class was absent was reported as
an internal error and could only be rescued by blind backoff, despite the
StorageClass ADD/UPDATE event already being registered as a useful wakeup.

The binder now defers an unclaimed static PV whenever a named class is not yet
cached, and the scheduler parks a missing-class Pod as a pending dependency so
the class event wakes it directly. The e2e regression creates the Pod and PVC
before creating the StorageClass, asserts that the graph remains unresolved
while the class is absent, then lets the real hostpath CSI provisioner create
the PV after scheduling and requires the existing objects to converge to a
Bound, Running Pod. This keeps the final runtime assertion on a CSI volume,
which nodelet supports; an in-tree hostPath PV would correctly remain
unsupported. The companion static-WFC case covers the opposite order, where
the class is created before the volume objects.

### 28. Fixed: the static PV cache-order guard confused an absent class with a late class

**Severity: real regression in the static binder path — found in full e2e run
33842307349.**

The fix for finding #27 correctly treated a named StorageClass missing from the
binder's local cache as unknown, because the class might have been created but
its informer event might not have arrived yet. That made the static-PV test
hang: its PV and PVC intentionally used a matching class name without creating
a StorageClass object at all. Static binding does not require a StorageClass
object, so the binder deferred the claim forever while waiting for an event
that could never exist.

**Fixed**: on that cache-miss path, the binder now asks the authoritative
controller read client for the StorageClass. An existing
`WaitForFirstConsumer` class still defers until scheduling pre-binds the PV; a
confirmed `NotFound` means there is no delayed-binding policy and the static
PV can bind; transient lookup errors requeue the claim. The existing static
PV e2e test remains the regression case for the absent-class order.

### 29. Fixed: a PDB could publish an empty status after its Pod graph arrived

**Severity: real convergence race — found in full e2e run 33842307349.**

The disruption controller consumed independent Pod and PodDisruptionBudget
informers. If the PDB was observed and reconciled before the Deployment's Pods,
then the Pod watch was still initializing or relisting, the controller could
publish `expectedPods=0` and lose the later wakeup. The Deployment became ready,
but the PDB status stayed empty until the test timed out. The failure was
intermittent because it depended on informer startup and apiserver event
ordering.

**Fixed**: PDB reconciliation refreshes the current Pods through the shared
controller read client, retries failed reads/status writes, and periodically
requeues known budgets as an informer safety-net. The e2e test now creates the
PDB before the Deployment so the empty-before-Pods ordering is exercised
directly, then requires the status to converge through all four numeric fields.

### 30. Fixed: concurrent e2e runs could clobber each other's result branch

**Severity: CI validation race — found while validating findings #28 and #29.**

Manual e2e runs were independent at the cluster level, but every run force-pushed
the same `e2e-results` branch during setup. When two targeted runs were
dispatched together, one run could reset that branch while the other was
preparing or publishing its shard, causing a failure before the Rust test
runner started. This was a harness failure, not a product failure, but it
made concurrent investigation unreliable.

**Fixed**: e2e and release workflows now use a run-scoped results branch,
`e2e-results-$GITHUB_RUN_ID`, so setup and shard publication are isolated per
workflow run.
