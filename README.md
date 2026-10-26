# v0.9.0 validation

**Status: running. Publication is not a validation pass.**

- [Published release](https://github.com/centerionware/not-k8s/releases/tag/v0.9.0)
- [Workflow and job status](https://github.com/centerionware/not-k8s/actions/runs/40242497736)
- Source: e2f8f124b976528d76671f3991f1648522fda219
- [E2e shards](e2e/40242497736-1/)
- [Latest stack flamegraphs](latest-stack.md)
- [Latest three-way comparison](latest-comparison.md)

Heavy workload, 300 seconds idle and 300 seconds load per profile.
E2e and metrics use the published release binary. Flamegraphs use the
run-scoped optimized symbolized profiling artifact, not a
release-performance ratio. Failed/partial captures are not passes.
