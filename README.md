# v0.9.0 validation

**Status: running. Publication is not a validation pass.**

- [Published release](https://github.com/centerionware/not-k8s/releases/tag/v0.9.0)
- [Workflow and job status](https://github.com/centerionware/not-k8s/actions/runs/40253258540)
- Source: a01ad6bd0066894b5b2ed9991d5b2072ad6f452a
- [E2e shards](e2e/40253258540-1/)
- [Latest stack flamegraphs](latest-stack.md)
- [Latest three-way comparison](latest-comparison.md)

Heavy workload, 300 seconds idle and 300 seconds load per profile.
E2e and metrics use the published release binary. Flamegraphs use the
run-scoped optimized symbolized profiling artifact, not a
release-performance ratio. Failed/partial captures are not passes.
