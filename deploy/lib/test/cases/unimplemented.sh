# lib/test/cases/unimplemented.sh — documents known gaps as *active*
# checks rather than silent absence of coverage. `kubectl exec`/`kubectl
# logs` used to live here as "assert this still fails" — they now have
# real functional tests in streaming.sh instead, since server.rs
# implements them. What's left here still doesn't exist at all.

test_stats_summary_endpoint_manual_check() {
    skip_test "server.rs implements containerLogs/exec/attach/portForward but not /stats/summary (or /metrics/resource, /metrics/cadvisor) — there's no per-pod usage stats collector behind it yet (see docs/GAP_CLOSURE.md). Once that lands, replace this with a real check against https://<node-ip>:<NODELET_SERVER_PORT>/stats/summary."
}

register_test test_stats_summary_endpoint_manual_check
