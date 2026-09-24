# nodemigrate release status

Last updated: 2026-09-24

This is the living release record for
[the full nodemigrate goal](NODEMIGRATION_GOAL.md).

## Release invariants

- `nodemigrate` ships as its own standalone artifact and is not linked into
  the combined `notk8s` binary.
- Use the exact version number of the latest regular not-k8s release.
- A nodemigrate-only publication must not change or advance shared `VERSION`,
  create a new regular release version, or alter the regular release cadence.
- A nodemigrate-only GitHub release must not become the repository's `latest`
  release, so the next publication still reads the latest regular release.
- The ordinary release workflow and its standard gates are outside this
  task's routine validation policy. Run a publication only under the separate
  release authorization and record its exact source SHA and release identity.

## Current status

| Item | Status |
| --- | --- |
| Standalone crate and artifact packaging | Present in release workflow; needs current-SHA verification |
| Excluded from combined `notk8s` binary | Intended and documented; verify package contents with the release artifact when available |
| Version follows latest regular release | Release identity logic present; current version/tag not recorded here |
| Shared `VERSION` unchanged by nodemigrate-only release | Required invariant; no nodemigrate-only publication recorded |
| Regular-release pointer unchanged by nodemigrate-only publication | `gh release create --latest=false` prevents the standalone release from replacing GitHub's latest-release pointer; workflow policy check enforces the flag |
| Published nodemigrate release | Not published/recorded for this migration task |

## Publication record

| Date | Source SHA | Latest regular release/version | Nodemigrate artifact/tag | VERSION changed? | Evidence |
| --- | --- | --- | --- | --- | --- |
| — | — | — | — | — | No publication recorded |

Before recording a release as complete, verify that the artifact version equals
the latest regular release version and that the regular release pointer and
shared `VERSION` did not advance because of the nodemigrate-only publication.
