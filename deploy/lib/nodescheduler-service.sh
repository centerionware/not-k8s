# lib/nodescheduler-service.sh — installs nodescheduler (pod placement,
# kube-scheduler's job) as a real, persistent, auto-restarting service.
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
# scheduler is the other way round: it is a plain apiserver client, so it
# orders `After=k3s.service` like nodelet and nodeproxy do, and needs a
# KUBECONFIG (which nodestore deliberately has none of).
#
# A restarting scheduler is not an outage the way a restarting datastore is:
# pods simply stay Pending until it comes back, then get placed. That is the
# same failure mode upstream kube-scheduler has, and it is why this unit can
# be restarted freely without draining anything first.

NODESCHEDULER_UNIT_SYSTEMD=/etc/systemd/system/nodescheduler.service
NODESCHEDULER_UNIT_OPENRC=/etc/init.d/nodescheduler
NODESCHEDULER_SUPERVISOR_SCRIPT="$WORK_DIR/nodescheduler-supervisor.sh"

# Shared informer inputs read directly as system:kube-scheduler by
# nodescheduler's watch.rs. The upstream scheduler role is not sufficient
# for this replacement's unconditional storage/CSI/DRA watch set. Keep this
# in sync with crates/nodebootstrap/src/rbac.rs's
# NODESCHEDULER_READ_GRANTS (Finding #5, release pipeline run 50).
NODESCHEDULER_READ_GRANTS=(
    "'' namespaces"
    "'' nodes"
    "'' pods"
    "'' services"
    "'' replicationcontrollers"
    "'' persistentvolumes"
    "'' persistentvolumeclaims"
    "apps replicasets"
    "apps statefulsets"
    "policy poddisruptionbudgets"
    "storage.k8s.io storageclasses"
    "storage.k8s.io csinodes"
    "storage.k8s.io csidrivers"
    "storage.k8s.io csistoragecapacities"
    "storage.k8s.io volumeattachments"
    "resource.k8s.io deviceclasses"
    "resource.k8s.io resourceclaims"
    "resource.k8s.io resourceslices"
)

apply_nodescheduler_rbac() {
    local entry group resource manifest="---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: nodebootstrap:nodescheduler-dra
rules:
"
    for entry in "${NODESCHEDULER_READ_GRANTS[@]}"; do
        # shellcheck disable=SC2086 # deliberately word-split: "group resource"
        set -- $entry
        group="$1" resource="$2"
        [[ "$group" == "''" ]] && group=""
        manifest+="- apiGroups: [\"${group}\"]
  resources: [\"${resource}\"]
  verbs: [\"get\", \"list\", \"watch\"]
"
    done
    manifest+="---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: nodebootstrap:nodescheduler-dra
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: nodebootstrap:nodescheduler-dra
subjects:
- kind: User
  name: system:kube-scheduler
  apiGroup: rbac.authorization.k8s.io
"
    echo "$manifest" | kubectl apply -f - \
        || die "applying nodescheduler's storage/CSI/DRA RBAC grant failed"
}

install_nodescheduler_service() {
    if command -v systemctl &>/dev/null; then
        install_nodescheduler_service_systemd
    elif command -v rc-service &>/dev/null && command -v rc-update &>/dev/null; then
        install_nodescheduler_service_openrc
    else
        install_nodescheduler_service_fallback
    fi
}

# Values are quoted for the style being generated, never emitted raw — same
# reasoning as nodestore_env_lines(): these are pass-through operator values
# this file never validates, and one carrying a space, `$`, backtick or `;`
# would otherwise break the generated init script or run a command at
# service start.
nodescheduler_env_lines() { # $1 = "shell" (export VAR=value) or "systemd" (Environment=VAR=value)
    local style="$1" out="" name value
    out+="$(nodescheduler_env_line "$style" KUBECONFIG "$KUBECONFIG")"$'\n'
    # Forwarded only if the bootstrap's own shell set it — a systemd service
    # gets no shell environment of its own, so without this the operator has
    # no way to raise verbosity short of hand-editing the installed unit
    # (run-nodescheduler.sh's own `${RUST_LOG:-info}` default still applies
    # when unset here). e2e.yml's `debug_scheduler_log` input is the
    # dispatch-time knob that sets this for chasing a live flake.
    if [[ -n "${RUST_LOG:-}" ]]; then
        out+="$(nodescheduler_env_line "$style" RUST_LOG "$RUST_LOG")"$'\n'
    fi
    # Everything else the operator set. `compgen -v` rather than `env`, so
    # this sees shell variables the bootstrap set as well as exported ones.
    for name in $(compgen -v | grep '^NODESCHEDULER_' | sort); do
        value="${!name:-}"
        [[ -n "$value" ]] || continue
        out+="$(nodescheduler_env_line "$style" "$name" "$value")"$'\n'
    done
    printf '%s' "$out"
}

# nodescheduler_env_line <style> <name> <value> — one environment assignment,
# quoted for the file it is going into. Identical rules to
# nodestore_env_line(); see that function's comment for why systemd needs `%`
# doubled as well as `"` and `\` escaped.
nodescheduler_env_line() {
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

install_nodescheduler_service_systemd() {
    log "Installing nodescheduler as a systemd service (Restart=always, enabled on boot)..."
    cat > "$NODESCHEDULER_UNIT_SYSTEMD" <<EOF
[Unit]
Description=nodescheduler — not-k8s pod placement (kube-scheduler replacement)
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
ExecStart=$SCRIPT_DIR/run-nodescheduler.sh
Restart=always
RestartSec=5s
$(nodescheduler_env_lines systemd)
[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
    systemctl enable nodescheduler.service
    # `restart`, not `start`: `start` is a no-op against an already-running
    # unit, which would keep a previous install's old binary and env running
    # instead of what this run just built. Same trap nodelet.service hit.
    systemctl restart nodescheduler.service
    sleep 3
    systemctl is-active --quiet nodescheduler.service \
        || warn "nodescheduler.service didn't come up cleanly — check: journalctl -u nodescheduler -n 50 (it needs a reachable apiserver and a readable \$KUBECONFIG; it exits non-zero without them)"
}

install_nodescheduler_service_openrc() {
    log "Installing nodescheduler as an OpenRC service (supervised, auto-restart, added to boot)..."
    cat > "$NODESCHEDULER_UNIT_OPENRC" <<EOF
#!/sbin/openrc-run
description="nodescheduler — not-k8s pod placement (kube-scheduler replacement)"

$(nodescheduler_env_lines shell)
supervisor="supervise-daemon"
command="$SCRIPT_DIR/run-nodescheduler.sh"
# Without these, supervise-daemon discards the child's stdout/stderr
# entirely: there is no OpenRC equivalent of journalctl, so a crash-looping
# nodescheduler leaves nothing behind to read except "Child command line: ..."
# repeating in /var/log/messages. See nodeproxy-service.sh for the same note.
output_log="/var/log/nodescheduler.log"
error_log="/var/log/nodescheduler.log"
respawn_max=0
respawn_delay=5

depend() {
    need net
    after k3s
}
EOF
    chmod +x "$NODESCHEDULER_UNIT_OPENRC"
    rc-update add nodescheduler default 2>/dev/null || true
    rc-service nodescheduler restart
    sleep 3
    rc-service nodescheduler status 2>&1 | grep -qi started \
        || warn "nodescheduler OpenRC service didn't come up cleanly — check: rc-service nodescheduler status"
}

install_nodescheduler_service_fallback() {
    warn "No systemd or OpenRC on this system — falling back to a self-restarting background loop \
for nodescheduler. This recovers from a crash but is NOT a real service; set up this system's actual \
init/service manager to run '$SCRIPT_DIR/run-nodescheduler.sh' persistently when you can."

    # A re-run without tearing down the previous loop would leave two
    # schedulers racing to bind the same pods.
    if [[ -f "$WORK_DIR/nodescheduler.pid" ]]; then
        kill "$(cat "$WORK_DIR/nodescheduler.pid")" 2>/dev/null || true
    fi
    pkill -f "$NODESCHEDULER_SUPERVISOR_SCRIPT" 2>/dev/null || true
    pkill -f "$SCRIPT_DIR/run-nodescheduler.sh" 2>/dev/null || true

    cat > "$NODESCHEDULER_SUPERVISOR_SCRIPT" <<EOF
#!/usr/bin/env bash
$(nodescheduler_env_lines shell)
while true; do
    "$SCRIPT_DIR/run-nodescheduler.sh"
    sleep 5
done
EOF
    chmod +x "$NODESCHEDULER_SUPERVISOR_SCRIPT"
    nohup "$NODESCHEDULER_SUPERVISOR_SCRIPT" >"$LOG_DIR/nodescheduler.log" 2>&1 &
    echo $! > "$WORK_DIR/nodescheduler.pid"
    sleep 3

    if command -v crontab &>/dev/null; then
        ( crontab -l 2>/dev/null | grep -vF "$NODESCHEDULER_SUPERVISOR_SCRIPT"
          echo "@reboot $NODESCHEDULER_SUPERVISOR_SCRIPT >>$LOG_DIR/nodescheduler.log 2>&1 &" ) | crontab - \
            && log "Added a cron @reboot entry, so nodescheduler also restarts after a reboot." \
            || warn "Couldn't add a cron @reboot entry — nodescheduler will NOT survive a reboot on this system."
    else
        warn "No cron either — nodescheduler will NOT survive a reboot on this system."
    fi
}

# Undoes install_nodescheduler_service() — best-effort across all three tiers,
# since we don't track which one a given machine got.
#
# Note this does NOT re-enable k3s's own bundled kube-scheduler: that is
# decided by setup-control-plane.sh from $SCHEDULER, and a run that wants ours
# gone should re-run the bootstrap without --scheduler=nodescheduler rather
# than only removing this unit. Removing the unit alone leaves a cluster with
# no scheduler at all, which looks exactly like "pods stay Pending forever".
remove_nodescheduler_service() {
    if command -v systemctl &>/dev/null; then
        systemctl disable --now nodescheduler.service 2>/dev/null || true
        rm -f "$NODESCHEDULER_UNIT_SYSTEMD"
        systemctl daemon-reload 2>/dev/null || true
    fi
    if command -v rc-update &>/dev/null; then
        rc-service nodescheduler stop 2>/dev/null || true
        rc-update del nodescheduler default 2>/dev/null || true
        rm -f "$NODESCHEDULER_UNIT_OPENRC"
    fi
    if command -v crontab &>/dev/null; then
        ( crontab -l 2>/dev/null | grep -vF "$NODESCHEDULER_SUPERVISOR_SCRIPT" ) | crontab - 2>/dev/null || true
    fi
    pkill -f "$SCRIPT_DIR/run-nodescheduler.sh" 2>/dev/null || true
    [[ -f "$WORK_DIR/nodescheduler.pid" ]] && kill "$(cat "$WORK_DIR/nodescheduler.pid")" 2>/dev/null || true
    # Drop the pid file itself, not just the process it named — see
    # nodeproxy-service.sh for the recycled-PID trap this avoids.
    rm -f "$WORK_DIR/nodescheduler.pid" "$NODESCHEDULER_SUPERVISOR_SCRIPT"
}
