# v0.8.0 validation

**Validation: SUCCESS**

[Release](https://github.com/centerionware/not-k8s/releases/tag/v0.8.0) · [Workflow](https://github.com/centerionware/not-k8s/actions/runs/34229885934)

| Job | Result |
| --- | --- |
| prepare-validation | success |
| release-e2e | success |
| release-flamegraphs | success |
| cleanup-profile-artifact | success |
| release-comparison | success |
| release-comparison-publish | success |
| release-comparison-report | success |

[Full e2e shard logs](e2e/34229885934-1/)

[Latest stack flamegraphs](latest-stack.md)

[Latest three-way comparison](latest-comparison.md)

[This attempt's comparison data](comparisons/34229885934-1/)

Links may be absent when their producing job failed before publication.
Latest-profile links can refer to an earlier attempt; check run/attempt identity.

Both profiles use heavy load with 300-second idle and loaded windows.
E2e and comparisons execute the checksum-verified published release runtime.
Flamegraphs execute the run-scoped optimized symbolized profiling artifact.
Single-run measurements are diagnostic evidence, not universal performance claims.
