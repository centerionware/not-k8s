# nodemigrate status dashboard

Last updated: 2026-09-24

This dashboard tracks the full nodemigrate goal in
[NODEMIGRATION_GOAL.md](NODEMIGRATION_GOAL.md). Detailed status is kept in the
separate living documents below.

## Current state

| Area | State | Detail |
| --- | --- | --- |
| Full bidirectional migration implementation | In progress; code present, reverse paths not runtime-verified | [Migration status](NODEMIGRATE_MIGRATION_STATUS.md) |
| Existing nodestore member replacement | Replacement ordering and Raft learner catch-up guard implemented; focused nodemigrate, nodebootstrap, and nodestore checks passed at `c468627045cae98360bed8a93397407ff934ed86`; runtime scenario unverified | [Migration status](NODEMIGRATE_MIGRATION_STATUS.md) |
| K3s/Cilium and upstream Kubernetes/Cilium test lanes | Workflow and script drafted; manual runtime lanes not run | [CI status](NODEMIGRATE_CI_STATUS.md) |
| Nodemigrate merge gates | Single-node K3s+Cilium and upstream Kubernetes with 3 control planes + 2 workers are required; isolated round trips and no-difference assertions are not yet implemented or verified | [CI status](NODEMIGRATE_CI_STATUS.md) |
| Standalone artifact and shared-version behavior | Release design present; nodemigrate publication not recorded | [Release status](NODEMIGRATE_RELEASE_STATUS.md) |

## Current verification

- Targeted quick-check passed at `daac9ad05285e6ff749d4c51a18f9b71c8fb7dee`
  before bidirectional migration changes.
- The crate check at `55704b1c202c248ab633aac8c74877c46499e59f` failed to
  compile; those compiler errors were corrected. At
  `4e932917080e21412c79625fb2b79d08f5e62c0f`, compilation succeeded but the
  upstream control-plane detection test exposed a rooted-path bug. The fix
  passed nodemigrate crate tests at `4ef3585eaea6beae51ffd1116134435b0edc305e`.
- Source CNI detection and the joined replacement node-agent path passed the
  focused nodemigrate tests at `63cb20cbe75ebdfafee861c41137b334fb6ff95b`.
- The Cilium health assertions passed shell syntax validation at
  `21d532991835ff587a918e3bf665ed4f601e6fdf`; the manual migration lanes have
  not run.
- At `8518a99537cc4b98f2cbf8184f49c9927a8d78cf`, nodemigrate checks, shell
  syntax validation, and commit convention passed (runs `35956413580`,
  `35956413566`, and `35956412012`). The replacement-membership changes now
  committed at `c468627045cae98360bed8a93397407ff934ed86` passed nodemigrate
  checks (`35957207376`), quick-check for `nodebootstrap,nodestore`
  (`35957212012`), PR shell validation (`35957207356`), and commit convention
  (`35957206090`). The real replacement-member runtime case remains unverified.
- No migration integration lane has been dispatched. The user directed that
  general e2e and build gates not run; the dedicated runtime workflow also
  remains undispatched pending explicit authorization.

## Next actions

1. Keep both real-cluster lanes and the existing-cluster join/replacement case
   marked unverified until their authorized runtime checks pass.
2. Track release readiness and publication separately; do not bump the shared
   version for nodemigrate-only publication.
