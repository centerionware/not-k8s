# lib/test/cases/unimplemented.sh — documents known gaps as *active*
# checks rather than silent absence of coverage, so this suite starts
# failing loudly the moment a gap closes instead of silently staying wrong.
# `kubectl exec`/`kubectl logs` and `/stats/summary` used to live here as
# "assert this still doesn't exist" placeholders — they now have real
# functional tests (streaming.sh, stats.sh) since server.rs implements them.

test_metrics_resource_endpoint_manual_check() {
    skip_test "server.rs implements containerLogs/exec/attach/portForward/statsSummary but not /metrics/resource or /metrics/cadvisor (the Prometheus-format alternatives to /stats/summary) — see docs/GAP_CLOSURE.md's 'Everything else' list. Once one lands, replace this with a real check against https://<node-ip>:<NODELET_SERVER_PORT>/metrics/resource."
}

register_test test_metrics_resource_endpoint_manual_check
