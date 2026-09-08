# Post-publication release validation

The release workflow runs unit tests, builds static assets for x86_64, aarch64
and armv7l, then publishes. The old pre-publication e2e path and `skip_e2e`
switch are removed. **Publication no longer implies e2e passed.** The final
workflow and release-specific results index must both be checked.

After publication, independent jobs on disposable x86_64 runners execute:

- The full five-shard e2e registry, including real CSI/DRA driver setup.
- One all-component CPU flamegraph capture, including bootstrap and five-minute
  idle/heavy-load phases.
- One three-way whole-stack comparison: not-k8s, latest stable Kubernetes,
  and the latest k3s channel. Each leg has five-minute idle/heavy-load phases.
  The dependent report draws component and combined CPU/RSS/PSS charts.

Nine validation jobs can run concurrently. No job-level serialization or
`max-parallel` cap is imposed; GitHub's account concurrency allowance determines
when runners are available. Build jobs have a 180-minute timeout, validation
jobs 120 minutes. GitHub-hosted jobs allow at most six hours each; this is not
a six-hour limit on the sum of parallel job durations.

## Exact release identity and executable provenance

The workflow freezes VERSION before builds, checks the stamped workspace
version, and passes the published tag through job outputs. Validation never
reads the subsequently advanced VERSION branch or downloads `latest` not-k8s.
The workflow is serialized against another release to protect version mutation.

Every validation runner downloads the exact tagged asset and verifies its entry
in the release's `SHA256SUMS` before execution. E2e and comparison use the normal
optimized, stripped combined release binary and its nodebootstrap applet.
Flamegraphs use an **additional published x86_64 `profiling` asset**, built with
release optimization, debug symbols and frame pointers. It is diagnostic, not
byte-identical to the stripped release asset; flamegraph-run CPU numbers must
not be presented as release benchmark ratios. No post-publication Rust build or
Actions build-artifact transfer is needed. Release artifacts can be cleaned up
as soon as publication completes.

Comparison measurement jobs are read-only; small metrics/diagnostic artifacts
(one-day retention, no binaries) pass to a dedicated publication job. E2e and
flamegraph jobs publish directly and retain job-scoped write access. Builds and
release-identity lookup inherit the workflow's read-only default.

Retrying publication reuses a matching remote tag and existing release assets,
uploading only missing assets. A different tag target or published checksum
bundle fails closed rather than overwriting shipped bytes. Use rerun-failed-jobs
to reuse the frozen version and original build artifacts after partial failure.

## One results branch

The release page starts with a link to `e2e-prof-{release-tag}`, for example
`e2e-prof-v0.8.0`, above the changelog. The branch initially reports **running**.
Its final index records every validation job's success/failure/skipped state;
any missing or non-successful validation job fails the final validation check.
This also runs if publication succeeded but a later VERSION/README update failed.
A cancelled workflow can leave the index at running; follow its Actions link.
Validation failure does not silently delete or roll back the published release.

Results include e2e logs under `e2e/{run}-{attempt}/`, compressed full perf
captures and browsable flamegraphs under `history/`, and three-way results
under `comparisons/{run}-{attempt}/`. Top-level links point to the latest
published stack and comparison reports. On retries, check attempt identity:
a failed attempt may not replace an earlier successful report. Raw perf bundles
are compressed and split below GitHub's per-file limit, never silently truncated.

Publishers use separate sparse checkouts and bounded push/rebase retries, never
force-push a shared results branch. Each matrix leg owns distinct result files.
The normal manual e2e/profiling workflows retain their existing results branches.

## Shared implementation and validation limits

Release jobs do not dispatch or call another workflow. Local composite actions
share the existing bootstrap/test/capture/report steps with the manual workflows.
`deploy/test-release-validation.py` checks the job graph, composite shell syntax,
failure aggregation, exact-tag downloads and checksum rejection with fixtures.
`deploy/test-profile-stack.py` checks measurement and chart behavior.

These checks and actionlint reduce wiring errors, but cannot prove a first live
release will succeed. Real runner provisioning, asset publication/downloads,
cross-builds and concurrent remote result pushes still require live validation.
