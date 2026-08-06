# version

Tracks the next release version for not-k8s. `VERSION` holds a single
`MAJOR.MINOR.PATCH` line — the release workflow reads it, uses it to tag
the release it's about to publish, then bumps the patch component and
commits the new value back here for the next run.

Bump MINOR/MAJOR manually (edit `VERSION` directly, commit, push) ahead
of a release that should carry one — the automated bump only ever
increments PATCH.
