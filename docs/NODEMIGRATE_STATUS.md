# nodemigrate status dashboard

Last updated: 2026-09-24

This dashboard tracks the full nodemigrate goal in
[NODEMIGRATE_GOAL.md](NODEMIGRATE_GOAL.md). Detailed status is kept in the
separate living documents below.

## Current state

| Area | State | Detail |
| --- | --- | --- |
| Full bidirectional migration implementation | In progress; code present, reverse paths not runtime-verified | [Migration status](NODEMIGRATE_MIGRATION_STATUS.md) |
| K3s/Cilium and upstream Kubernetes/Cilium test lanes | Workflow and script drafted; manual runtime lanes not run | [CI status](NODEMIGRATE_CI_STATUS.md) |
| Standalone artifact and shared-version behavior | Release design present; nodemigrate publication not recorded | [Release status](NODEMIGRATE_RELEASE_STATUS.md) |

## Current verification

- Targeted quick-check passed at `daac9ad05285e6ff749d4c51a18f9b71c8fb7dee`
  before bidirectional migration changes.
- The crate check at `55704b1c202c248ab633aac8c74877c46499e59f` failed to
  compile; those compiler errors were corrected. At `4e932917080e21412c79625fb2b79d08f5e62c0f`, compilation succeeded but the new upstream control-plane detection test exposed a rooted-path bug. The fix is in the worktree; follow-up CI is pending.
- No migration integration lane has been dispatched. The user directed that
  general e2e and build gates not run; the dedicated runtime workflow also
  remains undispatched pending explicit authorization.

## Next actions

1. Rerun the focused nodemigrate tests after the manifest-path correction.
2. Update the migration and CI status pages with the new exact SHA and run
   results.
3. Keep both real-cluster lanes and the existing-cluster join/replacement case
   marked unverified until their authorized runtime checks pass.
4. Track release readiness and publication separately; do not bump the shared
   version for nodemigrate-only publication.
