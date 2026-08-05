# lib/nodelet-service.sh — installs nodelet as a real, persistent,
# auto-restarting service — an earlier version of this script only
# `nohup`'d nodelet tied to the invoking shell, which died with the
# terminal, a reboot, or any crash, with nothing to bring it back. k3s
# itself already gets exactly this treatment (a real service); there was no
# reason nodelet shouldn't too.
#
# Three tiers, matching what's actually available:
#   1. systemd (Restart=always, enabled on boot) — the common case.
#   2. OpenRC (supervise-daemon, respawn, added to the default runlevel) —
#      the only other init system k3s's own installer supports, so nothing
#      running k3s at all should lack both this and systemd.
#   3. Neither: a self-restarting background loop, persisted across reboots
#      via a cron @reboot entry if cron exists. Not equivalent to a real
#      service — surfaced with a clear warning, not silently accepted as
#      good enough — but still means a crash recovers on its own instead of
#      needing a human to notice and restart it by hand.
#
# nodelet gets its own dedicated install/remove (rather than reusing
# service-mgr.sh's generic install_supervised_service/remove_supervised_service)
# because its unit needs a specific env-var block (nodelet_env_lines) and a
# k3s.service dependency that the generic helper doesn't model.

NODELET_UNIT_SYSTEMD=/etc/systemd/system/nodelet.service
NODELET_UNIT_OPENRC=/etc/init.d/nodelet
NODELET_SUPERVISOR_SCRIPT="$WORK_DIR/nodelet-supervisor.sh"

install_nodelet_service() {
    if command -v systemctl &>/dev/null; then
        install_nodelet_service_systemd
    elif command -v rc-service &>/dev/null && command -v rc-update &>/dev/null; then
        install_nodelet_service_openrc
    else
        install_nodelet_service_fallback
    fi
}

# k3s's own client CA — signs the client cert its apiserver presents when
# it dials OUT to a kubelet (containerLogs/exec/attach/port-forward
# proxying), separate from --client-ca-file (which verifies clients
# connecting INTO the apiserver). Fixed by k3s's own installer, not
# something this project configures — this is just where k3s always puts
# it. Pointing NODELET_CLIENT_CA_FILE at it here has no ordering problem
# the way the reverse direction (apiserver trusting nodelet's cert) does:
# k3s writes this file as part of its own first startup, which always
# happens before nodelet's service unit is ever installed/started.
K3S_CLIENT_CA_FILE=/var/lib/rancher/k3s/server/tls/client-ca.crt

nodelet_env_lines() { # $1 = "export VAR=value" (shell) or "Environment=VAR=value" (systemd)
    local style="$1" out=""
    for kv in "KUBECONFIG=$KUBECONFIG" "NODELET_RUNTIME=$NODELET_RUNTIME" \
              "NODELET_IP_FAMILY=$IP_FAMILY" "NODELET_LB_METHOD=$LB_METHOD"; do
        [[ "$style" == "systemd" ]] && out+="Environment=$kv"$'\n' || out+="export $kv"$'\n'
    done
    if [[ -n "${NODELET_CRI_ENDPOINT:-}" ]]; then
        [[ "$style" == "systemd" ]] && out+="Environment=NODELET_CRI_ENDPOINT=$NODELET_CRI_ENDPOINT"$'\n' \
            || out+="export NODELET_CRI_ENDPOINT=$NODELET_CRI_ENDPOINT"$'\n'
    fi
    # Only meaningful for the CRI runtime's kubelet-style server
    # (containerLogs/exec/attach/port-forward) — the mock runtime doesn't
    # run it at all (config.rs's server_enabled default). Without this,
    # nodelet's server has no client CA configured, falls back to
    # bearer-token-only auth, and every proxied request from the
    # apiserver (which authenticates via this client cert, not a bearer
    # token) fails with "missing or malformed Authorization" — confirmed
    # for real. Best-effort: if k3s hasn't written this file yet for some
    # reason, set the var anyway (nodelet just logs a warning and runs
    # bearer-token-only, same as if this were never set at all) rather
    # than conditioning the whole service install on it.
    if [[ "${NODELET_RUNTIME:-}" == "cri" ]]; then
        [[ "$style" == "systemd" ]] && out+="Environment=NODELET_CLIENT_CA_FILE=$K3S_CLIENT_CA_FILE"$'\n' \
            || out+="export NODELET_CLIENT_CA_FILE=$K3S_CLIENT_CA_FILE"$'\n'
    fi
    printf '%s' "$out"
}

install_nodelet_service_systemd() {
    log "Installing nodelet as a systemd service (Restart=always, enabled on boot)..."
    cat > "$NODELET_UNIT_SYSTEMD" <<EOF
[Unit]
Description=nodelet — not-k8s node agent (kubelet replacement)
Documentation=https://github.com/centerionware/not-k8s
After=k3s.service network-online.target
Wants=k3s.service network-online.target

[Service]
Type=simple
WorkingDirectory=$REPO_ROOT
ExecStart=$SCRIPT_DIR/run-nodelet.sh
Restart=always
RestartSec=5s
$(nodelet_env_lines systemd)
[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
    systemctl enable nodelet.service
    # `enable --now` (equivalently `start` on an already-active unit) is a
    # no-op if nodelet was already running from a previous install — it
    # would keep the OLD process (old binary in memory, old env vars)
    # running forever, silently ignoring whatever this run just built and
    # wrote to the unit file. `restart` always re-execs, whether nodelet
    # was already running or not, so a re-run of this script actually
    # picks up a freshly built binary / changed NODELET_RUNTIME / etc.
    systemctl restart nodelet.service
    sleep 3
    systemctl is-active --quiet nodelet.service \
        || warn "nodelet.service didn't come up cleanly — check: journalctl -u nodelet -n 50"
}

install_nodelet_service_openrc() {
    log "Installing nodelet as an OpenRC service (supervised, auto-restart, added to boot)..."
    cat > "$NODELET_UNIT_OPENRC" <<EOF
#!/sbin/openrc-run
description="nodelet — not-k8s node agent (kubelet replacement)"

$(nodelet_env_lines shell)
supervisor="supervise-daemon"
command="$SCRIPT_DIR/run-nodelet.sh"
respawn_max=0
respawn_delay=5

depend() {
    need net
    after k3s
}
EOF
    chmod +x "$NODELET_UNIT_OPENRC"
    rc-update add nodelet default 2>/dev/null || true
    # Same reasoning as the systemd tier's `restart` above: `start` is a
    # no-op against an already-running service, which would leave a
    # previous install's old binary/env running instead of this run's.
    # OpenRC's own `restart` action already tolerates "not currently
    # running" (falls through to just starting it), so this is safe on a
    # fresh install too, not just a re-run.
    rc-service nodelet restart
    sleep 3
    rc-service nodelet status 2>&1 | grep -qi started \
        || warn "nodelet OpenRC service didn't come up cleanly — check: rc-service nodelet status"
}

install_nodelet_service_fallback() {
    warn "No systemd or OpenRC on this system (k3s's own installer only supports those two, so \
this is unusual for a box running k3s) — falling back to a self-restarting background loop. \
This recovers from a crash but is NOT a real service; set up this system's actual init/service \
manager to run '$SCRIPT_DIR/run-nodelet.sh' persistently when you can."

    # A re-run of this tier without killing the previous loop/nodelet
    # first would leave two supervisor loops and two nodelet processes
    # running side by side — both trying to bind the same ports/sockets —
    # instead of the new build actually replacing the old one. Same
    # tear-down remove_nodelet_service() already does, just inline here
    # since a re-install needs it too, not only an uninstall.
    if [[ -f "$WORK_DIR/nodelet.pid" ]]; then
        kill "$(cat "$WORK_DIR/nodelet.pid")" 2>/dev/null || true
    fi
    pkill -f "$NODELET_SUPERVISOR_SCRIPT" 2>/dev/null || true
    pkill -f "$SCRIPT_DIR/run-nodelet.sh" 2>/dev/null || true

    cat > "$NODELET_SUPERVISOR_SCRIPT" <<EOF
#!/usr/bin/env bash
$(nodelet_env_lines shell)
while true; do
    "$SCRIPT_DIR/run-nodelet.sh"
    sleep 5
done
EOF
    chmod +x "$NODELET_SUPERVISOR_SCRIPT"
    nohup "$NODELET_SUPERVISOR_SCRIPT" >"$LOG_DIR/nodelet.log" 2>&1 &
    echo $! > "$WORK_DIR/nodelet.pid"
    sleep 3

    if command -v crontab &>/dev/null; then
        ( crontab -l 2>/dev/null | grep -vF "$NODELET_SUPERVISOR_SCRIPT"
          echo "@reboot $NODELET_SUPERVISOR_SCRIPT >>$LOG_DIR/nodelet.log 2>&1 &" ) | crontab - \
            && log "Added a cron @reboot entry, so nodelet also restarts after a reboot." \
            || warn "Couldn't add a cron @reboot entry — nodelet will NOT survive a reboot on this system."
    else
        warn "No cron either — nodelet will NOT survive a reboot on this system."
    fi
}

# Undoes install_nodelet_service() — stops/disables/removes whichever tier
# was used, best-effort across all three since we don't track which one a
# given machine got. nodelet is entirely this project's own thing (unlike
# containerd/runc/CNI/flannel, which other software might also depend on),
# so it's always removed outright — never held back for "next run" the way
# shared runtime infra is.
remove_nodelet_service() {
    if command -v systemctl &>/dev/null; then
        systemctl disable --now nodelet.service 2>/dev/null || true
        rm -f "$NODELET_UNIT_SYSTEMD"
        systemctl daemon-reload 2>/dev/null || true
    fi
    if command -v rc-update &>/dev/null; then
        rc-service nodelet stop 2>/dev/null || true
        rc-update del nodelet default 2>/dev/null || true
        rm -f "$NODELET_UNIT_OPENRC"
    fi
    if command -v crontab &>/dev/null; then
        ( crontab -l 2>/dev/null | grep -vF "$NODELET_SUPERVISOR_SCRIPT" ) | crontab - 2>/dev/null || true
    fi
    pkill -f "$SCRIPT_DIR/run-nodelet.sh" 2>/dev/null || true
    [[ -f "$WORK_DIR/nodelet.pid" ]] && kill "$(cat "$WORK_DIR/nodelet.pid")" 2>/dev/null || true
    rm -f "$NODELET_SUPERVISOR_SCRIPT"
}
