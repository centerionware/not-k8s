---
name: not-k8s-ci
description: Select, dispatch, monitor, and report not-k8s CI checks with exact commit/run identity and constrained-host artifact handling. Use for build/e2e requests, CI status, or validation of an existing fix; use not-k8s-stabilize to diagnose failures.
---

# Run not-k8s CI economically

Read [AGENTS.md](../../../AGENTS.md) and the user's active handoff. Preserve
the chosen branch, PR, apiserver target, and existing authorization. A status
request authorizes read-only inspection, not a fresh dispatch or cancellation.

## Choose the smallest check that answers the question

- Reuse existing results only when their SHA, target, feature set, and test
  scope match the claim being made. Check an already-running relevant run
  before starting another.
- For Rust iteration, pass the changed crates to quick-check. Do not build
  the whole workspace out of habit, download an artifact to answer a
  compile question, or build every architecture for one test host.
- Target known runtime failures during iteration when the user permits it.
  Keep the full unfiltered run as the completion gate; do not substitute
  repeated filtered passes. An established request for full runs takes priority.
- Do not dispatch release.yml while chasing individual failures. It includes
  expensive release builds and publication; releasing needs its own authority.
- Avoid duplicated API polling, repeated remote log reads, and recompiling
  the same unchanged code. Do not weaken verification to reduce cost.
- For documentation/skills, validate content and references locally without
  Cargo. State the absence of runtime behavior explicitly. This does not waive
  the repository's main-merge gate.

## Validate the exact branch

No local Cargo builds/tests or local e2e on the development host. Follow the
workflow files on the chosen ref; dispatch inputs come from that ref's copy.
`Unexpected inputs provided` can mean an old branch workflow, not a broken
GitHub API. Do not merge/rebase other work merely to fix dispatch without
checking the user's branch constraints.

After the PR is open and the substantive fix is committed/pushed, examples
for an apiserver-only Rust change are:

```bash
gh workflow run quick-check.yml --repo centerionware/not-k8s --ref "$BRANCH" -f components=nodeapiserver
gh workflow run e2e.yml --repo centerionware/not-k8s --ref "$BRANCH" -f apiserver=nodeapiserver
```

Set `BRANCH` to the verified PR branch. Include every changed crate in
`components` (comma-separated). When the user requires simultaneous quick-check
and e2e, dispatch both together. `only=pattern1,pattern2` matches test names,
not filenames; omit it for the full gate. Do not dispatch quick-check solely
for a documentation change or treat it as covering workflow/install changes.

Capture each URL/run ID returned by dispatch and the pushed SHA. Do not use
an unscoped "latest run" lookup. If the CLI returns no run URL, identify the
dispatch by workflow, branch, head SHA, and creation time; ambiguity requires
resolution before watching, downloading, or reporting it.

### Watch without API spam

```bash
gh run watch "$RUN_ID" --repo centerionware/not-k8s --interval 60 --exit-status
```

Use the user's specified cadence; the repository default is 60 seconds.
For a long-lived shell task use `setsid nohup`, a run-specific log, and `disown`;
read that local watcher log between updates. Avoid a second polling loop that
calls `gh run view/list` alongside the watcher.

Do not inspect active shard logs. After the entire e2e run completes, retrieve
the job/result metadata once, save each needed job log once, then search those
files locally. Example, with verified IDs and a run-specific directory:

```bash
gh run view "$RUN_ID" --repo centerionware/not-k8s --json headSha,conclusion,jobs
gh run view "$RUN_ID" --repo centerionware/not-k8s --job "$JOB_ID" --log > "$LOG_FILE"
```

Preserve full logs, not just grep excerpts. Confirm the checkout SHA and
test-start timestamps. Record every shard's pass/fail/skip result; one green
shard or an aggregate build result is not a full e2e pass. Distinguish legitimate
environment-gated skips from newly missing coverage.

Repeat on the selected PR until the requested full suite passes or an external
blocker prevents further authorized progress. Do not stop at "fix pushed" or
"CI started" when the task is to obtain green. Do not cancel useful runs or
redispatch merely because compilation is slow.

## Artifacts and real hosts, only when in scope

Read the branch's `build.yml`, `nodebootstrap/src/fetch.rs`, and component
table for actual artifact names, architecture, layout, and prebuilt env vars.
Use quick-check's debug build for the compile/unit loop. The current e2e
workflow deliberately builds stripped release binaries: debug artifacts have
approached 900 MB, so faster compilation alone is not an acceptable tradeoff.
Preserve that default unless an alternative's artifact size and storage budget
are verified. Reuse the combined installer applet instead of compiling and
uploading a redundant standalone installer. Real nodelet needs `cri`; nodestore and
nodeapiserver also require protoc. A mock-runtime pass does not prove CRI.

- Check free memory/disk and target architecture before downloading. On the
  phone VM, `/tmp` is a small RAM-backed filesystem. Store large artifacts on
  a verified disk-backed path; stream the download and extraction. Do not use
  `gh run download` for large debug archives on that host: it can buffer the
  archive and OOM the VM. Verify integrity before installing.
- Use the prebuilt seam. Do not invoke a source bootstrap that accidentally
  compiles on the device. Install/restart only the components in scope; account
  for shared executable paths in combined layouts.
- `/usr/sbin/nft` may exist outside the unprivileged PATH. Missing `nft_fib`,
  `nft_numgen`, or `nft_hash` can be expected host degradation paths; inspect
  `nodeproxy::probe_caps()` rather than assuming the proxy cannot run.
- A running flanneld can still be attached to an old cluster. Inspect its
  kubeconfig/logs and `/run/flannel/subnet.env` before attributing Pending
  Pods to the apiserver. Host restarts require the deployment to be in scope.
- If git is being killed, inspect `git count-objects -vH`; do not launch
  unbounded repacking. Repository-local pack limits used on the phone are
  `pack.windowMemory=32m`, `pack.deltaCacheSize=16m`, `pack.threads=1`.

## Report what the run actually proves

Record the pushed SHA, workflow inputs, returned run IDs, final job/shard
conclusions, log locations, and remaining gates. Use
[the stabilization skill](../not-k8s-stabilize/SKILL.md) when a failure needs
diagnosis. Do not silently switch test target, trim the suite, broaden scope,
or declare the work complete because the dispatch succeeded.
