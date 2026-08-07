# lib/test/cases/graceful_shutdown.sh — systemd-logind inhibitor lock +
# pod drain on PrepareForShutdown (shutdown.rs). Deliberately NOT an
# automated test: the only way to trigger the real signal is an actual
# `systemctl poweroff`/`reboot` (or a privileged, policy-bypassing D-Bus
# call impersonating logind), and this suite must never do that to a host
# it doesn't own. See docs/GAP_CLOSURE.md's round 9 notes for why this is
# the single least-validated piece of this round, same caveat streaming.sh
# already carries for the exec/attach proxy.
#
# Round 123 re-evaluated this against the "only skip if the environment
# TRULY cannot be configured" bar the rest of this suite now meets: a real
# `poweroff`/`reboot` on a GitHub Actions hosted runner would terminate the
# runner process itself mid-job, with no way to report this test's own
# pass/fail (let alone let build-release/publish-release run afterward) —
# genuinely not something a single CI job can do to itself, not merely
# inconvenient to set up. D-Bus signal spoofing remains rejected as
# policy-bypassing, not a legitimate substitute for the real signal.

test_graceful_node_shutdown_manual_note() {
    skip_test "graceful node shutdown needs a real systemd-logind PrepareForShutdown signal, which only happens on an actual host shutdown/reboot — not something this suite triggers automatically. Manual spot-check: set NODELET_SHUTDOWN_GRACE_PERIOD_SECS (and optionally NODELET_SHUTDOWN_GRACE_PERIOD_CRITICAL_SECS) on nodelet, run 'loginctl list-inhibitors' and confirm nodelet holds a 'shutdown'/'delay' entry, apply a few test pods, then 'systemctl poweroff' (or 'reboot') the node and watch nodelet's logs for 'terminating pods' before the box actually goes down — non-critical pods (no system-node-critical/system-cluster-critical priorityClassName) should get preStop + termination before critical ones, all within NODELET_SHUTDOWN_GRACE_PERIOD_SECS total."
}

register_test test_graceful_node_shutdown_manual_note
