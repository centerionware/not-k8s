# lib/nodeproxy-service.sh — installs nodeproxy (the Service/ClusterIP/
# NodePort proxy) as a real, persistent, auto-restarting service.
#
# Same three tiers, and the same reasoning, as nodelet-service.sh:
#   1. systemd (Restart=always, enabled on boot) — the common case.
#   2. OpenRC (supervise-daemon, respawn, added to the default runlevel).
#   3. Neither: a self-restarting background loop + cron @reboot. Not a real
#      service — surfaced with a warning, not silently accepted.
#
# Deliberately its own unit rather than something nodelet's unit starts:
# nodeproxy is a replaceable component (a node can run Cilium or a real
# kube-proxy instead, or nothing — see bootstrap-source.sh --proxy=none), and
# a node with a wedged service proxy should still have a working node agent.
# Neither unit orders against the other; both only need k3s.
#
# Env is much smaller than nodelet's: no NODELET_RUNTIME (nodeproxy has no
# container runtime), no client CA (it serves no HTTP endpoints at all —
# it only watches the apiserver and writes nftables rules).

NODEPROXY_UNIT_SYSTEMD=/etc/systemd/system/nodeproxy.service
NODEPROXY_UNIT_OPENRC=/etc/init.d/nodeproxy
NODEPROXY_SUPERVISOR_SCRIPT="$WORK_DIR/nodeproxy-supervisor.sh"

install_nodeproxy_service() {
    if command -v systemctl &>/dev/null; then
        install_nodeproxy_service_systemd
    elif command -v rc-service &>/dev/null && command -v rc-update &>/dev/null; then
        install_nodeproxy_service_openrc
    else
        install_nodeproxy_service_fallback
    fi
}

nodeproxy_env_lines() { # $1 = "shell" (export VAR=value) or "systemd" (Environment=VAR=value)
    local style="$1" out=""
    for kv in "KUBECONFIG=$KUBECONFIG" \
              "NODEPROXY_IP_FAMILY=$IP_FAMILY" "NODEPROXY_LB_METHOD=$LB_METHOD"; do
        [[ "$style" == "systemd" ]] && out+="Environment=$kv"$'\n' || out+="export $kv"$'\n'
    done
    printf '%s' "$out"
}

install_nodeproxy_service_systemd() {
    log "Installing nodeproxy as a systemd service (Restart=always, enabled on boot)..."
    cat > "$NODEPROXY_UNIT_SYSTEMD" <<EOF
[Unit]
Description=nodeproxy — not-k8s Service routing (kube-proxy replacement)
Documentation=https://github.com/centerionware/not-k8s
After=k3s.service network-online.target
Wants=k3s.service network-online.target
# Same reasoning as nodelet.service: Restart=always/RestartSec=5s below is
# this unit's real crash-loop backoff, and systemd's default start limit
# (5 starts / 10s) only gets in the way of intentional external restarts —
# which the e2e suite issues back-to-back.
StartLimitIntervalSec=0

[Service]
Type=simple
WorkingDirectory=$REPO_ROOT
ExecStart=$SCRIPT_DIR/run-nodeproxy.sh
Restart=always
RestartSec=5s
$(nodeproxy_env_lines systemd)
[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
    systemctl enable nodeproxy.service
    # `restart`, not `start`: `start` is a no-op against an already-running
    # unit, which would keep a previous install's old binary and env running
    # instead of what this run just built. Same trap nodelet.service hit.
    systemctl restart nodeproxy.service
    sleep 3
    systemctl is-active --quiet nodeproxy.service \
        || warn "nodeproxy.service didn't come up cleanly — check: journalctl -u nodeproxy -n 50 (nodeproxy needs nft + CAP_NET_ADMIN; it exits non-zero without them)"
}

install_nodeproxy_service_openrc() {
    log "Installing nodeproxy as an OpenRC service (supervised, auto-restart, added to boot)..."
    cat > "$NODEPROXY_UNIT_OPENRC" <<EOF
#!/sbin/openrc-run
description="nodeproxy — not-k8s Service routing (kube-proxy replacement)"

$(nodeproxy_env_lines shell)
supervisor="supervise-daemon"
command="$SCRIPT_DIR/run-nodeproxy.sh"
# Without these, supervise-daemon discards the child's stdout/stderr
# entirely: there is no OpenRC equivalent of journalctl, so a crash-looping
# nodeproxy leaves nothing behind to read except "Child command line: ..."
# repeating in /var/log/messages. systemd users get the real error from
# journalctl -u nodeproxy; this is what gives OpenRC users the same thing.
output_log="/var/log/nodeproxy.log"
error_log="/var/log/nodeproxy.log"
respawn_max=0
respawn_delay=5

depend() {
    need net
    after k3s
}
EOF
    chmod +x "$NODEPROXY_UNIT_OPENRC"
    rc-update add nodeproxy default 2>/dev/null || true
    rc-service nodeproxy restart
    sleep 3
    rc-service nodeproxy status 2>&1 | grep -qi started \
        || warn "nodeproxy OpenRC service didn't come up cleanly — check: rc-service nodeproxy status"
}

install_nodeproxy_service_fallback() {
    warn "No systemd or OpenRC on this system — falling back to a self-restarting background loop \
for nodeproxy. This recovers from a crash but is NOT a real service; set up this system's actual \
init/service manager to run '$SCRIPT_DIR/run-nodeproxy.sh' persistently when you can."

    # A re-run without tearing down the previous loop would leave two
    # nodeproxy processes fighting over the same nftables table.
    if [[ -f "$WORK_DIR/nodeproxy.pid" ]]; then
        kill "$(cat "$WORK_DIR/nodeproxy.pid")" 2>/dev/null || true
    fi
    pkill -f "$NODEPROXY_SUPERVISOR_SCRIPT" 2>/dev/null || true
    pkill -f "$SCRIPT_DIR/run-nodeproxy.sh" 2>/dev/null || true

    cat > "$NODEPROXY_SUPERVISOR_SCRIPT" <<EOF
#!/usr/bin/env bash
$(nodeproxy_env_lines shell)
while true; do
    "$SCRIPT_DIR/run-nodeproxy.sh"
    sleep 5
done
EOF
    chmod +x "$NODEPROXY_SUPERVISOR_SCRIPT"
    nohup "$NODEPROXY_SUPERVISOR_SCRIPT" >"$LOG_DIR/nodeproxy.log" 2>&1 &
    echo $! > "$WORK_DIR/nodeproxy.pid"
    sleep 3

    if command -v crontab &>/dev/null; then
        ( crontab -l 2>/dev/null | grep -vF "$NODEPROXY_SUPERVISOR_SCRIPT"
          echo "@reboot $NODEPROXY_SUPERVISOR_SCRIPT >>$LOG_DIR/nodeproxy.log 2>&1 &" ) | crontab - \
            && log "Added a cron @reboot entry, so nodeproxy also restarts after a reboot." \
            || warn "Couldn't add a cron @reboot entry — nodeproxy will NOT survive a reboot on this system."
    else
        warn "No cron either — nodeproxy will NOT survive a reboot on this system."
    fi
}

# Undoes install_nodeproxy_service() — best-effort across all three tiers,
# since we don't track which one a given machine got. Does NOT flush the
# nftables table; that's stop_service_proxy_nft() in uninstall.sh, which is
# also what a --proxy=none reinstall needs.
remove_nodeproxy_service() {
    if command -v systemctl &>/dev/null; then
        systemctl disable --now nodeproxy.service 2>/dev/null || true
        rm -f "$NODEPROXY_UNIT_SYSTEMD"
        systemctl daemon-reload 2>/dev/null || true
    fi
    if command -v rc-update &>/dev/null; then
        rc-service nodeproxy stop 2>/dev/null || true
        rc-update del nodeproxy default 2>/dev/null || true
        rm -f "$NODEPROXY_UNIT_OPENRC"
    fi
    if command -v crontab &>/dev/null; then
        ( crontab -l 2>/dev/null | grep -vF "$NODEPROXY_SUPERVISOR_SCRIPT" ) | crontab - 2>/dev/null || true
    fi
    pkill -f "$SCRIPT_DIR/run-nodeproxy.sh" 2>/dev/null || true
    [[ -f "$WORK_DIR/nodeproxy.pid" ]] && kill "$(cat "$WORK_DIR/nodeproxy.pid")" 2>/dev/null || true
    # Drop the pid file itself, not just the process it named. Leaving it
    # behind means the next fallback-tier install reads a PID that's already
    # dead — or, worse, one the kernel has since recycled onto an unrelated
    # process — and kills that instead.
    rm -f "$WORK_DIR/nodeproxy.pid" "$NODEPROXY_SUPERVISOR_SCRIPT"
}
