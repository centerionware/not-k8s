# lib/test/nodelet_env.sh — restart nodelet with extra/overridden env vars
# for tests that need a specific NODELET_* setting only nodelet's own
# startup environment controls (config file path, CPU/memory/topology
# manager policy, pressure/GC thresholds, TLS client CA, etc.). Round 123:
# this e2e suite otherwise runs entirely against one nodelet instance
# started once before any test runs, with no way to inject settings per
# test — the reason a dozen-plus tests used to be "manual spot-check only"
# rather than real automated coverage. This is the fix: a real nodelet
# restart + wait-for-Ready cycle, systemd-only (drop-in override under
# nodelet.service.d/) — this suite's only real target is GitHub Actions
# runners and any systemd host bootstrap-source.sh installs onto; a host
# on OpenRC/the fallback tier just can't run these specific tests, same
# "skip cleanly with a clear message" posture every other
# infrastructure-gated test in this suite already has.
#
# Every test using this MUST call nodelet_restore_env before returning —
# on success AND on failure (a `trap ... EXIT` inside the test function,
# same pattern gc.sh's finalizer test already uses), or a crash mid-test
# leaves the REST of the suite running against a non-default nodelet
# config, silently corrupting every test that runs after it.

NODELET_OVERRIDE_DROPIN_DIR=/etc/systemd/system/nodelet.service.d
NODELET_OVERRIDE_DROPIN="$NODELET_OVERRIDE_DROPIN_DIR/99-e2e-test-override.conf"

nodelet_restart_supported() {
    command -v systemctl >/dev/null 2>&1 && systemctl list-unit-files nodelet.service >/dev/null 2>&1
}

_nodelet_wait_ready() { # internal — polls Node Ready after a restart
    local desc="$1"
    wait_until 60 "$desc" bash -c '[[ "$(node_condition_status Ready)" == "True" ]]'
}

# nodelet_restart_with_env VAR1=value1 [VAR2=value2 ...] — overlays these
# on top of nodelet's normal startup environment via a systemd drop-in,
# restarts nodelet, and waits up to 60s for the Node object to report
# Ready again. Overwrites any previous override outright (not additive
# across calls) — one override in effect at a time.
nodelet_restart_with_env() {
    sudo mkdir -p "$NODELET_OVERRIDE_DROPIN_DIR"
    {
        echo "[Service]"
        for kv in "$@"; do
            echo "Environment=$kv"
        done
    } | sudo tee "$NODELET_OVERRIDE_DROPIN" >/dev/null
    sudo systemctl daemon-reload
    # Defensive even with nodelet-service.sh's StartLimitIntervalSec=0 fix
    # in the unit itself — an already-installed node from before that fix
    # (or any unit still carrying systemd's own default start-limit) would
    # otherwise fail this restart outright with "start-limit-hit" after
    # enough back-to-back calls, confirmed for real in CI (round 123).
    sudo systemctl reset-failed nodelet.service 2>/dev/null || true
    # Temporary diagnostic (round 123): nodelet-service.sh's
    # StartLimitIntervalSec=0 plus this reset-failed call were both
    # supposed to make "start-limit-hit" impossible here, but it happened
    # anyway in CI with both fixes already in place — print what systemd
    # actually has loaded for this unit right before the restart that
    # keeps failing, instead of guessing further.
    echo "[diag] nodelet.service start-limit right before restart: $(sudo systemctl show nodelet.service -p StartLimitIntervalUSec -p StartLimitBurst -p NRestarts --value | tr '\n' ' ')"
    sudo systemctl restart nodelet.service
    _nodelet_wait_ready "node Ready after nodelet restart with env override ($*)"
}

# nodelet_restore_env — removes any override from nodelet_restart_with_env
# and restarts nodelet back to its normal startup environment.
nodelet_restore_env() {
    sudo rm -f "$NODELET_OVERRIDE_DROPIN"
    sudo systemctl daemon-reload
    sudo systemctl reset-failed nodelet.service 2>/dev/null || true
    # Temporary diagnostic (round 123): nodelet-service.sh's
    # StartLimitIntervalSec=0 plus this reset-failed call were both
    # supposed to make "start-limit-hit" impossible here, but it happened
    # anyway in CI with both fixes already in place — print what systemd
    # actually has loaded for this unit right before the restart that
    # keeps failing, instead of guessing further.
    echo "[diag] nodelet.service start-limit right before restart: $(sudo systemctl show nodelet.service -p StartLimitIntervalUSec -p StartLimitBurst -p NRestarts --value | tr '\n' ' ')"
    sudo systemctl restart nodelet.service
    _nodelet_wait_ready "node Ready after nodelet restart (env override removed)"
}

export -f nodelet_restart_supported nodelet_restart_with_env nodelet_restore_env _nodelet_wait_ready
