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

## `postfix-capture/` + `postfix-e2e-output.txt` — after #527/#532/#533

Same box, same 11-test set, single pass (not looped this time), captured
against a fresh debug build of `nodeapiserver` commit `4fc1d08` — the
first commit after #527 (RBAC cache + protobuf field-index memoization),
#532 (Node authorizer field selector + watch cache), and #533 (Codex's
APF configuration-lookup cache, landed independently during this same
investigation) all merged.

**Results, same test set, comparable single-pass baseline:**

| | pre-fix (churn iteration 1) | post-fix (single pass) |
|---|---|---|
| failed | 7 / 11 | 4 / 11 |
| elapsed | 775s | 665s |
| `decode_message` (with children) | ~48.8% of samples | ~26.1% |
| dominated by | `fields_by_number` rebuilding a `HashMap` per call (~38.5%) | (fixed) |

Real, measured improvement — not eliminated. `postfix-e2e-output.txt`
still shows 4 of the original 11 failing, so whatever's left is a real,
separate problem, not noise.

**What's now on top** (`postfix-capture/top-functions.txt`,
`SUMMARY.txt`): with the old dominant cost gone, `decode_message` itself
dropped to ~7.3% self-time, but a *second* linear scan over a generated
static table surfaced right behind it — `PROTO_MESSAGES.contains(&message)`
(the same class of bug as `fields_by_number`, just a different table:
membership-checking against every message name this build knows about,
on every decode call), ~16.5% of all samples. Filed and fixed:
[#534](https://github.com/centerionware/not-k8s/issues/534) /
[PR #535](https://github.com/centerionware/not-k8s/pull/535) — same
`OnceLock`-memoized-once pattern as before, this time a `HashSet`. **Not
yet re-profiled with that fix in place** — natural next step once #535
lands, to see whether the remaining 4 failures close out or whether a
third thing surfaces once this one's gone too.

Below that: `malloc`/`free`/`memcpy`/`memcmp` and kernel spin-lock
contention (`queued_spin_lock_slowpath`, `_raw_spin_unlock_irqrestore`)
remain, similar proportions to the very first idle capture — still not
individually root-caused. Everything else in the top 20 is generic
overhead (TLS/crypto, JSON value handling, tokio scheduler bookkeeping),
not an obvious bug.

This capture's raw sample data is `postfix-capture/perf.data.zst`, not
plain `perf.data` — the real recording was 250MB, over GitHub's 100MB
hard file-size limit, so it's `zstd -19 --long=27`-compressed (8.4% of
original size). Decompress with `zstd -d perf.data.zst` before pointing
`perf report`/`perf script` at it.

## Regenerating a flamegraph elsewhere

```
perl stackcollapse-perf.pl <(perf script --no-inline -i idle-capture/perf.data) > out.folded
perl flamegraph.pl out.folded > flamegraph.svg
```

(`out.folded` and `flamegraph.svg` are already included in `idle-capture/`
for convenience — regenerate only if you want a different view, e.g. with
inline frames resolved on a box with more memory headroom than this one
had.)

## `idle-orphan-cleanup-2026-09-03/`

Captured 2026-09-03 investigating why nodestore and nodeapiserver were
both idling at massively more CPU than upstream kube-apiserver+nodestore
(user-reported live during a session working through #541/#548/#549/#550/
#551). Two independent 30s `perf record` captures (`nodestore/`,
`nodeapiserver/`), taken back to back, on the same box, at genuine idle:
zero client traffic, all e2e test debris (29 orphaned pods left behind by
#541's missing namespace finalizer — see #557) manually cleaned up
immediately beforehand. Debug build of `main` commit `c52bb701` (#544/#545/
#546/#547 all merged), deployed via `nodebootstrap` with
`NOTK8S_NODELET_PREBUILT`-style prebuilt seam.

Real (not `ps`'s lifetime-average) CPU, measured via `/proc/<pid>/stat`
`utime+stime` deltas across these capture windows:

| process | before orphan cleanup | after orphan cleanup, before this fix |
|---|---|---|
| nodestore | ~60-70% | ~20-29% |
| nodeapiserver | ~44-60% | ~15-30% |

### `nodestore/`

`top-functions.txt` is dominated by `sqlite3VdbeExec` (16.87%) and
`vdbeSorterSort` (0.67%) — genuine SQL execution and sort cost, not raft
or networking overhead. `perf.script`'s Rust call stacks (grep for
`nodestore5store8range_in`) trace essentially all of it through
`EtcdApi::range` -> `Store::range` -> `range_in()`.

Root cause found by reading `range_in()` in `crates/nodestore/src/
store.rs` alongside this: the query answering a non-`count_only` `Range`
call ran the same windowed `ROW_NUMBER() OVER (PARTITION BY key ORDER BY
revision DESC, sub DESC)` subquery **twice** — once wrapped in
`COUNT(*)` for the response's `count` field, once more for the actual
page — and that subquery sorts/partitions every historical MVCC revision
of every key in the requested range, not just the live ones.
`sqlite3 state.db "SELECT COUNT(*) FROM kv"` on this box: **10502** total
rows behind only **489** live keys (`SELECT COUNT(DISTINCT key)`) — over
2 hours of accumulated e2e-test churn, none of it ever compacted away.

Fixed in #558 (targets `main`, independent of the `nodeapiserver`
integration branch): compute `count` in the same single pass as the page,
via a second window function (`COUNT(*) OVER ()`) alongside the existing
`ROW_NUMBER()` one.

### `nodeapiserver/`

Flatter profile — no single function above ~4.3% (`alloc_slot`), the rest
spread across `memcpy`/`malloc`/`free`, protobuf decode, base64 encode/
decode, TLS (`rustls` SipHash), and `serde_json::Value`'s btree traversal.
Reads as real per-connection/per-request overhead (this build's
still-transitional identity/audit-logging/watch-cache machinery), not one
dominant bug the way nodestore's `range_in()` was — consistent with the
user's own expectation that nodeapiserver running noticeably hotter than
upstream kube-apiserver here (roughly 16-30%) is plausible for this stage
of the project, unlike nodestore's confirmed-fixable 20-29%.
