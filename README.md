# v0.9.0 validation

**Status: running. Publication is not a validation pass.**

- [Published release](https://github.com/centerionware/not-k8s/releases/tag/v0.9.0)
- [Workflow and job status](https://github.com/centerionware/not-k8s/actions/runs/40249184021)
- Source: 286dba19eee628a13ad8b326fe062bee860cc6a6
- [E2e shards](e2e/40249184021-1/)
- [Latest stack flamegraphs](latest-stack.md)
- [Latest three-way comparison](latest-comparison.md)

Heavy workload, 300 seconds idle and 300 seconds load per profile.
E2e and metrics use the published release binary. Flamegraphs use the
run-scoped optimized symbolized profiling artifact, not a
release-performance ratio. Failed/partial captures are not passes.
