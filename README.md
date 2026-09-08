# v0.8.0 validation

**Status: running. Publication is not a validation pass.**

- [Published release](https://github.com/centerionware/not-k8s/releases/tag/v0.8.0)
- [Workflow and job status](https://github.com/centerionware/not-k8s/actions/runs/34225806745)
- Source: ccca2458772f71fe3010e0f5b77b0198dfac62c7
- [E2e shards](e2e/34225806745-1/)
- [Latest stack flamegraphs](latest-stack.md)
- [Latest three-way comparison](latest-comparison.md)

Heavy workload, 300 seconds idle and 300 seconds load per profile.
E2e and metrics use the published release binary. Flamegraphs use the
run-scoped optimized symbolized profiling artifact, not a
release-performance ratio. Failed/partial captures are not passes.
