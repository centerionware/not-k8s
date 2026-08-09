# lib/test/nodeproxy_env.sh — restart nodeproxy with extra/overridden env
# vars, and restart it plain. The nodeproxy counterpart of nodelet_env.sh,
# and for the same reason: settings like NODEPROXY_LB_METHOD are only read
# at startup, so a suite that starts one nodeproxy before any test runs has
# no way to exercise them otherwise.
#
# Simpler than nodelet_env.sh in one way that matters: nodeproxy owns no
# Node object, publishes no conditions, and serves nothing. There's no
# "wait for Ready" equivalent — the observable signal that it came back is
# that the nftables table exists again and has been rebuilt, which is
# exactly what the tests using this assert on anyway.
#
# systemd-only, same posture as nodelet_env.sh: this suite's real targets
# are GitHub Actions runners and whatever systemd host bootstrap-source.sh
# installed onto. An OpenRC or fallback-tier host skips these tests
# cleanly rather than pretending to run them.
#
# Every test using this MUST call nodeproxy_restore_env before returning,
# on success AND on failure (a `trap ... EXIT` inside the test function),
# or the rest of the suite runs against a non-default service proxy.

NODEPROXY_OVERRIDE_DROPIN_DIR=/etc/systemd/system/nodeproxy.service.d
NODEPROXY_OVERRIDE_DROPIN="$NODEPROXY_OVERRIDE_DROPIN_DIR/99-e2e-test-override.conf"

nodeproxy_restart_supported() {
    command -v systemctl >/dev/null 2>&1 && systemctl list-unit-files nodeproxy.service >/dev/null 2>&1
}

# Waits until nodeproxy has re-established its own nftables table after a
# restart. On startup it applies nothing until its first watch event, so
# "the table exists and has a nat chain" is the earliest honest signal that
# it is up and has reconciled at least once — a plain `systemctl is-active`
# would go true well before any rule existed.
_nodeproxy_wait_programmed() {
    local desc="$1"
    wait_until 90 "$desc" bash -c \
        '(nft list table inet not_k8s_svc 2>/dev/null || sudo nft list table inet not_k8s_svc 2>/dev/null) | grep -q "chain prerouting"'
}

# nodeproxy_restart_with_env VAR=value [VAR=value ...] — overlays these on
# nodeproxy's startup environment via a systemd drop-in and restarts it.
# Overwrites any previous override outright (not additive).
nodeproxy_restart_with_env() {
    sudo mkdir -p "$NODEPROXY_OVERRIDE_DROPIN_DIR"
    {
        echo "[Service]"
        for kv in "$@"; do
            echo "Environment=$kv"
        done
    } | sudo tee "$NODEPROXY_OVERRIDE_DROPIN" >/dev/null
    sudo systemctl daemon-reload
    # nodeproxy.service carries StartLimitIntervalSec=0 for exactly this
    # reason, but reset-failed costs nothing and covers a unit installed
    # before that (or one that crash-looped earlier in the run because nft
    # was briefly unavailable).
    sudo systemctl reset-failed nodeproxy.service 2>/dev/null || true
    sudo systemctl restart nodeproxy.service
    _nodeproxy_wait_programmed "nodeproxy reprogrammed its nftables table after restart with env override ($*)"
}

# nodeproxy_restart_plain — restart with no config change, to prove the
# ruleset is rebuilt from the apiserver rather than depending on whatever
# happened to already be in the kernel.
nodeproxy_restart_plain() {
    sudo systemctl reset-failed nodeproxy.service 2>/dev/null || true
    sudo systemctl restart nodeproxy.service
    _nodeproxy_wait_programmed "nodeproxy reprogrammed its nftables table after a plain restart"
}

nodeproxy_restore_env() {
    sudo rm -f "$NODEPROXY_OVERRIDE_DROPIN"
    sudo systemctl daemon-reload
    sudo systemctl reset-failed nodeproxy.service 2>/dev/null || true
    sudo systemctl restart nodeproxy.service
    _nodeproxy_wait_programmed "nodeproxy reprogrammed its nftables table (env override removed)"
}

export -f nodeproxy_restart_supported nodeproxy_restart_with_env \
           nodeproxy_restart_plain nodeproxy_restore_env _nodeproxy_wait_programmed
