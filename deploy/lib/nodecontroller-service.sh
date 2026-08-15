# lib/nodecontroller-service.sh — installs nodecontroller
# (kube-controller-manager's job) as a real, persistent, auto-restarting
# service.
#
# Same three tiers, and the same reasoning, as nodelet-service.sh and
# nodeproxy-service.sh:
#   1. systemd (Restart=always, enabled on boot) — the common case.
#   2. OpenRC (supervise-daemon, respawn, added to the default runlevel).
#   3. Neither: a self-restarting background loop + cron @reboot. Not a real
#      service — surfaced with a warning, not silently accepted.
#
# Ordering differs from nodestore's unit. nodestore is `Before=k3s.service`
# because the apiserver *stores into* it and cannot start without it. A
# controller manager is the other way round: it is a plain apiserver client,
# so it orders `After=k3s.service` like nodelet and nodeproxy do, and needs a
# KUBECONFIG (which nodestore deliberately has none of).
#
# A restarting controller manager is not an outage the way a restarting
# datastore is: node taints/podCIDR/GC/etc. simply stop advancing until it
# comes back, then catch up. That is the same failure mode upstream
# kube-controller-manager has, and it is why this unit can be restarted
# freely without draining anything first.

NODECONTROLLER_UNIT_SYSTEMD=/etc/systemd/system/nodecontroller.service
NODECONTROLLER_UNIT_OPENRC=/etc/init.d/nodecontroller
NODECONTROLLER_SUPERVISOR_SCRIPT="$WORK_DIR/nodecontroller-supervisor.sh"

install_nodecontroller_service() {
    if command -v systemctl &>/dev/null; then
        install_nodecontroller_service_systemd
    elif command -v rc-service &>/dev/null && command -v rc-update &>/dev/null; then
        install_nodecontroller_service_openrc
    else
        install_nodecontroller_service_fallback
    fi
}

# Values are quoted for the style being generated, never emitted raw — same
# reasoning as nodestore_env_lines(): these are pass-through operator values
# this file never validates, and one carrying a space, `$`, backtick or `;`
# would otherwise break the generated init script or run a command at
# service start.
nodecontroller_env_lines() { # $1 = "shell" (export VAR=value) or "systemd" (Environment=VAR=value)
    local style="$1" out="" name value
    out+="$(nodecontroller_env_line "$style" KUBECONFIG "$KUBECONFIG")"$'\n'
    # Forwarded only if the bootstrap's own shell set it — a systemd service
    # gets no shell environment of its own, so without this the operator has
    # no way to raise verbosity short of hand-editing the installed unit
    # (run-nodecontroller.sh's own `${RUST_LOG:-info}` default still applies
    # when unset here).
    if [[ -n "${RUST_LOG:-}" ]]; then
        out+="$(nodecontroller_env_line "$style" RUST_LOG "$RUST_LOG")"$'\n'
    fi
    # Everything else the operator set. `compgen -v` rather than `env`, so
    # this sees shell variables the bootstrap set as well as exported ones.
    for name in $(compgen -v | grep '^NODECONTROLLER_' | sort); do
        value="${!name:-}"
        [[ -n "$value" ]] || continue
        out+="$(nodecontroller_env_line "$style" "$name" "$value")"$'\n'
    done
    printf '%s' "$out"
}

# nodecontroller_env_line <style> <name> <value> — one environment assignment,
# quoted for the file it is going into. Identical rules to
# nodestore_env_line(); see that function's comment for why systemd needs `%`
# doubled as well as `"` and `\` escaped.
nodecontroller_env_line() {
    local style="$1" name="$2" value="$3"
    if [[ "$style" == "systemd" ]]; then
        local escaped="${value//\\/\\\\}"
        escaped="${escaped//\"/\\\"}"
        escaped="${escaped//%/%%}"
        printf 'Environment=%s="%s"' "$name" "$escaped"
    else
        printf 'export %s=%q' "$name" "$value"
    fi
}

install_nodecontroller_service_systemd() {
    log "Installing nodecontroller as a systemd service (Restart=always, enabled on boot)..."
    cat > "$NODECONTROLLER_UNIT_SYSTEMD" <<EOF
[Unit]
Description=nodecontroller — not-k8s kube-controller-manager replacement
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
ExecStart=$SCRIPT_DIR/run-nodecontroller.sh
Restart=always
RestartSec=5s
$(nodecontroller_env_lines systemd)
[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
    systemctl enable nodecontroller.service
    # `restart`, not `start`: `start` is a no-op against an already-running
    # unit, which would keep a previous install's old binary and env running
    # instead of what this run just built. Same trap nodelet.service hit.
    systemctl restart nodecontroller.service
    sleep 3
    systemctl is-active --quiet nodecontroller.service \
        || warn "nodecontroller.service didn't come up cleanly — check: journalctl -u nodecontroller -n 50 (it needs a reachable apiserver and a readable \$KUBECONFIG; it exits non-zero without them)"
}

install_nodecontroller_service_openrc() {
    log "Installing nodecontroller as an OpenRC service (supervised, auto-restart, added to boot)..."
    cat > "$NODECONTROLLER_UNIT_OPENRC" <<EOF
#!/sbin/openrc-run
description="nodecontroller — not-k8s kube-controller-manager replacement"

$(nodecontroller_env_lines shell)
supervisor="supervise-daemon"
command="$SCRIPT_DIR/run-nodecontroller.sh"
# Without these, supervise-daemon discards the child's stdout/stderr
# entirely: there is no OpenRC equivalent of journalctl, so a crash-looping
# nodecontroller leaves nothing behind to read except "Child command line: ..."
# repeating in /var/log/messages. See nodeproxy-service.sh for the same note.
output_log="/var/log/nodecontroller.log"
error_log="/var/log/nodecontroller.log"
respawn_max=0
respawn_delay=5

depend() {
    need net
    after k3s
}
EOF
    chmod +x "$NODECONTROLLER_UNIT_OPENRC"
    rc-update add nodecontroller default 2>/dev/null || true
    rc-service nodecontroller restart
    sleep 3
    rc-service nodecontroller status 2>&1 | grep -qi started \
        || warn "nodecontroller OpenRC service didn't come up cleanly — check: rc-service nodecontroller status"
}

install_nodecontroller_service_fallback() {
    warn "No systemd or OpenRC on this system — falling back to a self-restarting background loop \
for nodecontroller. This recovers from a crash but is NOT a real service; set up this system's actual \
init/service manager to run '$SCRIPT_DIR/run-nodecontroller.sh' persistently when you can."

    # A re-run without tearing down the previous loop would leave two
    # controller managers racing to write the same Node taints/podCIDR/GC.
    if [[ -f "$WORK_DIR/nodecontroller.pid" ]]; then
        kill "$(cat "$WORK_DIR/nodecontroller.pid")" 2>/dev/null || true
    fi
    pkill -f "$NODECONTROLLER_SUPERVISOR_SCRIPT" 2>/dev/null || true
    pkill -f "$SCRIPT_DIR/run-nodecontroller.sh" 2>/dev/null || true

    cat > "$NODECONTROLLER_SUPERVISOR_SCRIPT" <<EOF
#!/usr/bin/env bash
$(nodecontroller_env_lines shell)
while true; do
    "$SCRIPT_DIR/run-nodecontroller.sh"
    sleep 5
done
EOF
    chmod +x "$NODECONTROLLER_SUPERVISOR_SCRIPT"
    nohup "$NODECONTROLLER_SUPERVISOR_SCRIPT" >"$LOG_DIR/nodecontroller.log" 2>&1 &
    echo $! > "$WORK_DIR/nodecontroller.pid"
    sleep 3

    if command -v crontab &>/dev/null; then
        ( crontab -l 2>/dev/null | grep -vF "$NODECONTROLLER_SUPERVISOR_SCRIPT"
          echo "@reboot $NODECONTROLLER_SUPERVISOR_SCRIPT >>$LOG_DIR/nodecontroller.log 2>&1 &" ) | crontab - \
            && log "Added a cron @reboot entry, so nodecontroller also restarts after a reboot." \
            || warn "Couldn't add a cron @reboot entry — nodecontroller will NOT survive a reboot on this system."
    else
        warn "No cron either — nodecontroller will NOT survive a reboot on this system."
    fi
}

# Undoes install_nodecontroller_service() — best-effort across all three tiers,
# since we don't track which one a given machine got.
#
# Note this does NOT re-enable k3s's own bundled kube-controller-manager:
# that is decided by setup-control-plane.sh from $CONTROLLER_MANAGER, and a
# run that wants ours gone should re-run the bootstrap without
# --controller-manager=nodecontroller rather than only removing this unit.
# Removing the unit alone leaves a cluster with no controller manager at
# all, which looks exactly like "nothing but existing pods keeps working —
# no new Node taints, no podCIDR for new Nodes, no GC".
remove_nodecontroller_service() {
    if command -v systemctl &>/dev/null; then
        systemctl disable --now nodecontroller.service 2>/dev/null || true
        rm -f "$NODECONTROLLER_UNIT_SYSTEMD"
        systemctl daemon-reload 2>/dev/null || true
    fi
    if command -v rc-update &>/dev/null; then
        rc-service nodecontroller stop 2>/dev/null || true
        rc-update del nodecontroller default 2>/dev/null || true
        rm -f "$NODECONTROLLER_UNIT_OPENRC"
    fi
    if command -v crontab &>/dev/null; then
        ( crontab -l 2>/dev/null | grep -vF "$NODECONTROLLER_SUPERVISOR_SCRIPT" ) | crontab - 2>/dev/null || true
    fi
    pkill -f "$SCRIPT_DIR/run-nodecontroller.sh" 2>/dev/null || true
    [[ -f "$WORK_DIR/nodecontroller.pid" ]] && kill "$(cat "$WORK_DIR/nodecontroller.pid")" 2>/dev/null || true
    # Drop the pid file itself, not just the process it named — see
    # nodeproxy-service.sh for the recycled-PID trap this avoids.
    rm -f "$WORK_DIR/nodecontroller.pid" "$NODECONTROLLER_SUPERVISOR_SCRIPT"
}
