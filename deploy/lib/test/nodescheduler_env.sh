# lib/test/nodescheduler_env.sh — restart nodescheduler with extra/overridden
# env vars. The nodescheduler counterpart of nodelet_env.sh/nodeproxy_env.sh,
# for the same reason: settings like NODESCHEDULER_EXTENDERS_JSON are only
# read at startup, so a suite that starts one nodescheduler before any test
# runs has no way to exercise them otherwise.
#
# Readiness signal: nodescheduler owns no Node object and programs no
# nftables table, but it does hold and renew the `kube-scheduler` Lease in
# kube-system for as long as it's the active leader (see
# test_scheduler_holds_the_leader_lease) — a restart drops the lease
# momentarily and re-acquires it, so "the lease's renewTime has moved past
# the moment we restarted" is the earliest honest "it's back up and
# scheduling" signal, mirroring nodeproxy_env.sh's "table has a nat chain
# again" rather than a bare `systemctl is-active`.
#
# systemd-only, same posture as nodelet_env.sh/nodeproxy_env.sh: this suite's
# real targets are GitHub Actions runners and whatever systemd host
# bootstrap-source.sh installed onto.
#
# Every test using this MUST call nodescheduler_restore_env before
# returning, on success AND on failure (a `trap ... EXIT` inside the test
# function), or the rest of the suite runs against a non-default
# nodescheduler config.
#
# A value containing a literal `"` (NODESCHEDULER_EXTENDERS_JSON, most
# likely) needs `\"` in the value passed here — confirmed live: systemd's
# own Environment= parsing silently strips a bare `"`, so
# `nodescheduler_restart_with_env 'V=[{"a":"b"}]'` reaches the process as
# `[{a:b}]`, not `[{"a":"b"}]`, and fails JSON parsing with no obvious
# reason why. See cases/scheduler.sh's extender tests for the pattern.

NODESCHEDULER_OVERRIDE_DROPIN_DIR=/etc/systemd/system/nodescheduler.service.d
NODESCHEDULER_OVERRIDE_DROPIN="$NODESCHEDULER_OVERRIDE_DROPIN_DIR/99-e2e-test-override.conf"

nodescheduler_restart_supported() {
    command -v systemctl >/dev/null 2>&1 \
        && systemctl list-unit-files nodescheduler.service >/dev/null 2>&1 \
        && systemctl cat nodescheduler.service >/dev/null 2>&1
}

_nodescheduler_wait_leading() { # internal — polls the lease past a given renewTime
    local desc="$1" before="$2"
    wait_until 90 "$desc" bash -c \
        "renew=\$(kubectl get lease kube-scheduler -n kube-system -o jsonpath='{.spec.renewTime}' 2>/dev/null); [[ -n \"\$renew\" && \"\$renew\" != '$before' ]]"
}

# nodescheduler_restart_with_env VAR=value [VAR=value ...] — overlays these
# on nodescheduler's startup environment via a systemd drop-in and restarts
# it. Overwrites any previous override outright (not additive).
nodescheduler_restart_with_env() {
    local before
    before="$(kubectl get lease kube-scheduler -n kube-system -o jsonpath='{.spec.renewTime}' 2>/dev/null)"

    sudo mkdir -p "$NODESCHEDULER_OVERRIDE_DROPIN_DIR"
    {
        echo "[Service]"
        for kv in "$@"; do
            echo "Environment=$kv"
        done
    } | sudo tee "$NODESCHEDULER_OVERRIDE_DROPIN" >/dev/null
    sudo systemctl daemon-reload
    sudo systemctl reset-failed nodescheduler.service 2>/dev/null || true
    sudo systemctl restart nodescheduler.service
    _nodescheduler_wait_leading "nodescheduler re-acquired the leader lease after restart with env override ($*)" "$before" \
        || die "nodescheduler never re-acquired the leader lease after the env override ($*) — check: journalctl -u nodescheduler -n 50"
}

nodescheduler_restore_env() {
    local before
    before="$(kubectl get lease kube-scheduler -n kube-system -o jsonpath='{.spec.renewTime}' 2>/dev/null)"

    sudo rm -f "$NODESCHEDULER_OVERRIDE_DROPIN"
    sudo systemctl daemon-reload
    sudo systemctl reset-failed nodescheduler.service 2>/dev/null || true
    sudo systemctl restart nodescheduler.service
    _nodescheduler_wait_leading "nodescheduler re-acquired the leader lease (env override removed)" "$before" \
        || die "nodescheduler never re-acquired the leader lease after removing the env override — check: journalctl -u nodescheduler -n 50"
}

export -f nodescheduler_restart_supported nodescheduler_restart_with_env \
           nodescheduler_restore_env _nodescheduler_wait_leading
