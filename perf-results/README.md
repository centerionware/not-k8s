# nodeapiserver perf results

Captured 2026-09-03 on a memory-constrained aarch64 phone VM, chasing the
cumulative per-shard slowdown described in
[#526](https://github.com/centerionware/not-k8s/issues/526). Debug build
(with symbols, unoptimized) of `nodeapiserver` branch commit `e42a78e`,
deployed via `nodebootstrap` with `NOTK8S_COMBINED_PREBUILT`, captured with
`deploy/profile-process.sh` (real `perf record`, DWARF call graphs,
`--no-inline`). Rendering was kept off this box on purpose — `perf.data` is
included raw in both captures so a flamegraph (or any other view) can be
regenerated elsewhere without spending this box's compute; `flamegraph.svg`
in `idle-capture/` is included anyway since `flamegraph.pl` itself turned
out cheap.

## `churn-run-e2e-output.txt`

The three back-to-back churn iterations that reproduced the CI cascade
locally, re-running this fixed set of 11 tests from CI's failure list on a
fresh cluster and letting cluster state accumulate across iterations
without a re-bootstrap in between:

```
test_scheduler_does_not_preempt_when_policy_forbids_it, test_scheduler_honours_pod_anti_affinity,
test_restart_policy_never_exit_zero_is_succeeded, test_pod_exceeding_its_active_deadline_is_terminated,
test_image_pull_policy_if_not_present_skips_the_registry_round_trip, test_liveness_probe_failure_restarts_the_container,
test_http_get_readiness_probe_against_a_real_server, test_downward_api_volume_writes_pod_metadata,
test_empty_dir_medium_memory_is_backed_by_tmpfs, test_host_path_directory_or_create_creates_a_missing_directory,
test_recursive_read_only_enabled_blocks_writes_in_a_nested_mount_too
```

Failures compounded exactly like the CI shard did:

| iteration | failed | elapsed |
|---|---|---|
| 1 | 7 / 11 | 775s |
| 2 | 10 / 11 | 935s |
| 3 | 11 / 11 | 979s |

## `churn-run-capture-corrupted/`

The `perf record` that ran *during* those three iterations (2400s safety
cap, `--stop-file`-bounded). Kept for the record, but it isn't usable:
`--output` was accidentally pointed at this session's own harness-managed
tmpfs scratch directory, which filled up mid-recording (the raw
`perf.data` alone reached >1GB) and the OS killed the write. `perf.data`
itself has been deleted (its data-size trailer never got written, so `perf
report`/`perf script` refuse to process it — "failed to process sample");
`process.txt`, `threads-before.txt`, and the empty `perf-report.txt` /
`perf-report.err` are what survived. The real capture is `idle-capture/`
below, taken immediately after on real disk.

## `idle-capture/`

90-second `perf record` of the live `nodeapiserver` process taken **right
after** the three churn iterations above finished, with no e2e test
running and no client traffic at all — the cluster had gone completely
idle. `nodeapiserver` was still burning ~63% of a core the entire time
(`SUMMARY.txt`, `top-functions.txt`).

**Where the CPU actually goes** (`perf-callgraph.txt`, caller-oriented,
`--no-inline`): ~48.8% of all samples are inside
`nodeapiserver::codec::protobuf::decode_message`, and ~38.5 of those
points are inside `fields_by_number` — a helper that filtered nodeapiserver's
*entire* `PROTO_FIELDS` table (every field of every message type this
build knows about) down to one message's fields and rebuilt a fresh
`HashMap` from scratch, **on every single protobuf decode** (i.e. on every
read/write against nodestore). That's a real, fixable, generic-loop
inefficiency, not a genuine business-logic cost, and it explains an
"idle" nodeapiserver still spending most of a whole CPU core: the sheer
call volume of protobuf decodes multiplies a small fixed cost per call
into a dominant one.

Filed as a fix, not just an issue, since it was small and clearly correct
once traced: see `fix(nodeapiserver): memoize the wire-number field index
used by protobuf decode`
(https://github.com/centerionware/not-k8s/pull/527, second commit) —
memoizes the per-message field-number index once via `OnceLock`, the same
established pattern `codegen::proto_field_index()` already used for the
JSON-name-keyed encode direction.

That PR also carries the RBAC fix from #526 itself (routing
`authz::resolve::rules_for`'s `ClusterRoleBinding`/`RoleBinding` lookups
through nodeapiserver's watch cache instead of an uncached storage round
trip on every authorized request) — a real but, per this capture,
secondary contributor next to the protobuf decode cost. **Neither fix has
been re-profiled yet** — that's the natural next step before assuming this
closes #526 for good, along with the unrelated
[#528](https://github.com/centerionware/not-k8s/issues/528) (a
debug-build-only nodecontroller stack overflow hit getting the cluster up
to capture this) which blocked deployment until worked around with a
`LimitSTACK=infinity` systemd override.

## Regenerating a flamegraph elsewhere

```
perl stackcollapse-perf.pl <(perf script --no-inline -i idle-capture/perf.data) > out.folded
perl flamegraph.pl out.folded > flamegraph.svg
```

(`out.folded` and `flamegraph.svg` are already included in `idle-capture/`
for convenience — regenerate only if you want a different view, e.g. with
inline frames resolved on a box with more memory headroom than this one
had.)
