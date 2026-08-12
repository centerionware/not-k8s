# lib/nodestore-service.sh — installs nodestore (the datastore: etcd v3 API
# over sqlite) as a real, persistent, auto-restarting service.
#
# Same three tiers, and the same reasoning, as nodelet-service.sh and
# nodeproxy-service.sh:
#   1. systemd (Restart=always, enabled on boot) — the common case.
#   2. OpenRC (supervise-daemon, respawn, added to the default runlevel).
#   3. Neither: a self-restarting background loop + cron @reboot. Not a real
#      service — surfaced with a warning, not silently accepted.
#
# The one way this differs from the other two units, and it matters:
# **ordering runs the other way round.** nodelet and nodeproxy are clients of
# the apiserver, so their units come After=k3s.service. nodestore is what the
# apiserver *stores into* — k3s cannot serve at all until this is listening —
# so this unit declares Before=k3s.service, and the bootstrap additionally
# waits for the port to accept a connection before installing k3s. Getting
# this backwards doesn't fail loudly; it makes k3s crash-loop on startup
# against a datastore that isn't there yet, which reads like a k3s problem.
#
# Env is smaller than nodelet's and unlike both others has no KUBECONFIG:
# nodestore never talks to the apiserver. Everything else comes from
# NODESTORE_* (crates/nodestore/src/config.rs), passed through verbatim by
# nodestore_env_lines() rather than enumerated, so configuring a replicated
# cluster needs no change here.

NODESTORE_UNIT_SYSTEMD=/etc/systemd/system/nodestore.service
NODESTORE_UNIT_OPENRC=/etc/init.d/nodestore
NODESTORE_SUPERVISOR_SCRIPT="$WORK_DIR/nodestore-supervisor.sh"

# Where the store listens, and therefore what the control plane is pointed
# at. Loopback by default, matching config.rs's own default and its
# defaults_are_loopback_only() test: a datastore reachable from the network
# without authentication is not something to opt anyone into silently.
NODESTORE_LISTEN_DEFAULT="127.0.0.1:2379"

install_nodestore_service() {
    if command -v systemctl &>/dev/null; then
        install_nodestore_service_systemd
    elif command -v rc-service &>/dev/null && command -v rc-update &>/dev/null; then
        install_nodestore_service_openrc
    else
        install_nodestore_service_fallback
    fi
}

# nodestore_env_lines <style> — "shell" (export VAR=value) or "systemd"
# (Environment=VAR=value).
#
# Passes through every NODESTORE_* variable set in this bootstrap's own
# environment instead of listing the ones known today. That's what lets a
# replicated deployment work without touching this file:
#
#   NODESTORE_INITIAL_CLUSTER=1=http://a:2380,2=http://b:2380 \
#   NODESTORE_MEMBER_ID=1 ./deploy/bootstrap-source.sh --datastore=nodestore
#
# NODESTORE_LISTEN and NODESTORE_DATA_DIR are defaulted rather than passed
# through only when set, so the unit is self-describing — reading it tells
# you where the store listens without having to know config.rs's defaults.
#
# Values are quoted for the style being generated, never emitted raw. These
# are pass-through values this file never validates, and NODESTORE_INITIAL_
# CLUSTER above is already a comma-and-equals-laden string — a value carrying
# a space, `$`, backtick or `;` would break the generated init script or run a
# command at service start, and in a systemd unit a value with a space is word
# split so only the first word reaches the process.
nodestore_env_lines() {
    local style="$1" out="" kv name value
    for kv in "NODESTORE_LISTEN=${NODESTORE_LISTEN:-$NODESTORE_LISTEN_DEFAULT}" \
              "NODESTORE_DATA_DIR=${NODESTORE_DATA_DIR:-/var/lib/nodestore}"; do
        out+="$(nodestore_env_line "$style" "${kv%%=*}" "${kv#*=}")"$'\n'
    done
    # Everything else the operator set. `compgen -v` rather than `env`, so
    # this sees shell variables the bootstrap set as well as exported ones.
    for name in $(compgen -v | grep '^NODESTORE_' | sort); do
        case "$name" in
            NODESTORE_LISTEN|NODESTORE_DATA_DIR) continue ;;  # already emitted above
        esac
        value="${!name:-}"
        [[ -n "$value" ]] || continue
        out+="$(nodestore_env_line "$style" "$name" "$value")"$'\n'
    done
    printf '%s' "$out"
}

# nodestore_env_line <style> <name> <value> — one environment assignment,
# quoted for the file it is going into.
#
# systemd: double quotes, with `"` and `\` escaped. systemd's own unquoting
# handles the rest, and this is what keeps a value with a space intact.
# shell: printf %q, which is bash's own "safe to re-read as shell input".
nodestore_env_line() {
    local style="$1" name="$2" value="$3"
    if [[ "$style" == "systemd" ]]; then
        local escaped="${value//\\/\\\\}"
        escaped="${escaped//\"/\\\"}"
        printf 'Environment=%s="%s"' "$name" "$escaped"
    else
        printf 'export %s=%q' "$name" "$value"
    fi
}

# nodestore_listen_port <listen> — the port from a listen address. Last colon
# wins, so an IPv6 literal's own colons don't confuse it.
nodestore_listen_port() {
    local listen="$1"
    printf '%s' "${listen##*:}"
}

# nodestore_dialable_host <listen> — a host that can actually be connected to
# *and* that the generated certificate covers.
#
# A wildcard bind is not an address. Dialing 0.0.0.0 happens to work on Linux,
# but no SAN names it, so the TLS handshake fails with a certificate error
# that reads as a PKI problem; "[::]" is not dialable at all. Both map to
# their own loopback, which nodestore's tls_sans() always includes.
#
# One helper because two callers had grown their own string surgery — this one
# and bootstrap-source.sh's NOTK8S_DATASTORE_ENDPOINT — and they disagreed.
nodestore_dialable_host() {
    local host="${1%:*}"
    case "$host" in
        ''|'0.0.0.0'|'*') host="127.0.0.1" ;;
        '[::]'|'::') host="[::1]" ;;
    esac
    # A bare IPv6 literal has to be bracketed to be usable in a URL.
    case "$host" in
        *:*) [[ "$host" == \[*\] ]] || host="[$host]" ;;
    esac
    printf '%s' "$host"
}

install_nodestore_service_systemd() {
    log "Installing nodestore as a systemd service (Restart=always, enabled on boot)..."
    cat > "$NODESTORE_UNIT_SYSTEMD" <<EOF
[Unit]
Description=nodestore — not-k8s datastore (etcd v3 API over sqlite)
Documentation=https://github.com/centerionware/not-k8s
# Before k3s, not After: the apiserver stores into this. k3s starting first
# just crash-loops against a datastore that isn't listening yet.
Before=k3s.service
After=network-online.target
Wants=network-online.target
# Same reasoning as nodelet.service: Restart=always/RestartSec=5s below is
# this unit's real crash-loop backoff, and systemd's default start limit
# (5 starts / 10s) only gets in the way of intentional external restarts —
# which the e2e suite issues back-to-back.
StartLimitIntervalSec=0

[Service]
Type=simple
WorkingDirectory=$REPO_ROOT
ExecStart=$SCRIPT_DIR/run-nodestore.sh
Restart=always
RestartSec=5s
$(nodestore_env_lines systemd)
[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
    systemctl enable nodestore.service
    # `restart`, not `start`: `start` is a no-op against an already-running
    # unit, which would keep a previous install's old binary and env running
    # instead of what this run just built. Same trap nodelet.service hit.
    systemctl restart nodestore.service
    sleep 3
    systemctl is-active --quiet nodestore.service \
        || warn "nodestore.service didn't come up cleanly — check: journalctl -u nodestore -n 50"
}

install_nodestore_service_openrc() {
    log "Installing nodestore as an OpenRC service (supervised, auto-restart, added to boot)..."
    cat > "$NODESTORE_UNIT_OPENRC" <<EOF
#!/sbin/openrc-run
description="nodestore — not-k8s datastore (etcd v3 API over sqlite)"

$(nodestore_env_lines shell)
supervisor="supervise-daemon"
command="$SCRIPT_DIR/run-nodestore.sh"
respawn_max=0
respawn_delay=5

depend() {
    need net
    before k3s
}
EOF
    chmod +x "$NODESTORE_UNIT_OPENRC"
    rc-update add nodestore default 2>/dev/null || true
    rc-service nodestore restart
    sleep 3
    rc-service nodestore status 2>&1 | grep -qi started \
        || warn "nodestore OpenRC service didn't come up cleanly — check: rc-service nodestore status"
}

install_nodestore_service_fallback() {
    warn "No systemd or OpenRC on this system — falling back to a self-restarting background loop \
for nodestore. This recovers from a crash but is NOT a real service; set up this system's actual \
init/service manager to run '$SCRIPT_DIR/run-nodestore.sh' persistently when you can."

    # A re-run without tearing down the previous loop would leave two
    # nodestore processes fighting over the same sqlite database.
    if [[ -f "$WORK_DIR/nodestore.pid" ]]; then
        kill "$(cat "$WORK_DIR/nodestore.pid")" 2>/dev/null || true
    fi
    pkill -f "$NODESTORE_SUPERVISOR_SCRIPT" 2>/dev/null || true
    pkill -f "$SCRIPT_DIR/run-nodestore.sh" 2>/dev/null || true

    cat > "$NODESTORE_SUPERVISOR_SCRIPT" <<EOF
#!/usr/bin/env bash
$(nodestore_env_lines shell)
while true; do
    "$SCRIPT_DIR/run-nodestore.sh"
    sleep 5
done
EOF
    chmod +x "$NODESTORE_SUPERVISOR_SCRIPT"
    nohup "$NODESTORE_SUPERVISOR_SCRIPT" >"$LOG_DIR/nodestore.log" 2>&1 &
    echo $! > "$WORK_DIR/nodestore.pid"
    sleep 3

    if command -v crontab &>/dev/null; then
        ( crontab -l 2>/dev/null | grep -vF "$NODESTORE_SUPERVISOR_SCRIPT"
          echo "@reboot $NODESTORE_SUPERVISOR_SCRIPT >>$LOG_DIR/nodestore.log 2>&1 &" ) | crontab - \
            && log "Added a cron @reboot entry, so nodestore also restarts after a reboot." \
            || warn "Couldn't add a cron @reboot entry — nodestore will NOT survive a reboot on this system."
    else
        warn "No cron either — nodestore will NOT survive a reboot on this system."
    fi
}

# wait_for_nodestore [timeout_secs] — block until the store accepts a TCP
# connection on its listen address.
#
# The unit's Before=k3s.service only orders process *start*, not readiness:
# systemd considers a Type=simple service started the instant it forks, so
# k3s would race a store that hasn't opened its socket yet. k3s's failure in
# that race is a connection-refused crash-loop that reads like a k3s bug, so
# the bootstrap pays a few seconds here to make it impossible.
wait_for_nodestore() {
    local timeout="${1:-30}" listen="${NODESTORE_LISTEN:-$NODESTORE_LISTEN_DEFAULT}"
    local port waited=0 host
    port="$(nodestore_listen_port "$listen")"
    # Brackets are URL syntax, not part of the address — /dev/tcp/[::1]/2379
    # is not a path that resolves, and the failure is silent, so an IPv6
    # listen address would burn the whole timeout before "succeeding" by
    # warning.
    host="$(nodestore_dialable_host "$listen")"
    host="${host#[}"
    host="${host%]}"

    log "Waiting for nodestore to accept connections on $listen (dialing $host:$port)..."
    while (( waited < timeout )); do
        # bash's /dev/tcp — no netcat dependency, which is exactly the sort of
        # tool a minimal edge image doesn't ship.
        if (exec 3<>"/dev/tcp/$host/$port") 2>/dev/null; then
            exec 3<&- 2>/dev/null || true
            log "nodestore is listening on $listen."
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done

    warn "nodestore never accepted a connection on $listen within ${timeout}s. The control plane \
is about to be pointed at it and will crash-loop until it comes up — check: journalctl -u nodestore -n 50"
    return 1
}

# Undoes install_nodestore_service() — best-effort across all three tiers,
# since we don't track which one a given machine got. Deliberately does NOT
# delete $NODESTORE_DATA_DIR: that's the cluster's entire state, and an
# uninstall that silently destroys it is unrecoverable.
remove_nodestore_service() {
    if command -v systemctl &>/dev/null; then
        systemctl disable --now nodestore.service 2>/dev/null || true
        rm -f "$NODESTORE_UNIT_SYSTEMD"
        systemctl daemon-reload 2>/dev/null || true
    fi
    if command -v rc-update &>/dev/null; then
        rc-service nodestore stop 2>/dev/null || true
        rc-update del nodestore default 2>/dev/null || true
        rm -f "$NODESTORE_UNIT_OPENRC"
    fi
    if command -v crontab &>/dev/null; then
        ( crontab -l 2>/dev/null | grep -vF "$NODESTORE_SUPERVISOR_SCRIPT" ) | crontab - 2>/dev/null || true
    fi
    pkill -f "$SCRIPT_DIR/run-nodestore.sh" 2>/dev/null || true
    [[ -f "$WORK_DIR/nodestore.pid" ]] && kill "$(cat "$WORK_DIR/nodestore.pid")" 2>/dev/null || true
    # Drop the pid file itself, not just the process it named — a stale PID
    # can have been recycled onto an unrelated process by the next install.
    rm -f "$WORK_DIR/nodestore.pid" "$NODESTORE_SUPERVISOR_SCRIPT"
}
