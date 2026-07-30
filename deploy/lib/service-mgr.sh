# lib/service-mgr.sh — generic persistent-service install/remove, shared by
# containerd (when we start it ourselves) and flanneld. nodelet gets its own
# dedicated install/remove in nodelet-service.sh (different unit content,
# and it's the one thing this project always owns outright), but both use
# the same three-tier strategy:
#
#   systemd (Restart=always, enabled on boot) -> OpenRC (supervise-daemon,
#   respawn, added to boot) -> a self-restarting background loop + cron
#   @reboot as a last resort, clearly logged as not a real service rather
#   than silently accepted as good enough.
#
# Learned the hard way: nodelet was originally just `nohup`'d, which meant
# it silently died on any crash/reboot/terminal-close with nothing to bring
# it back — the same bug class applies to anything else this script starts
# and runs long-term.

# $1 name, $2 description, $3 exec command (a single string — run through
# `sh -c` in every tier so it doesn't need re-parsing per init system) —
# MUST use an absolute path for the binary, never a bare command name:
# systemd/OpenRC services get a fresh, minimal PATH that won't include
# wherever this script's own PATH additions put a fetched/built binary
# ($TOOLCHAIN_DIR/bin), so a bare name resolves fine in this script's own
# shell and then fails with "not found" (exit 127) the moment the service
# manager actually runs it — this happened for real with flanneld, use
# "$(command -v the-binary)" at the call site like that fix does,
# $4 extra After=/depend() unit name or "" for none, $@ (from $5) KEY=VALUE
# environment pairs (zero or more).
install_supervised_service() {
    local name="$1" desc="$2" exec_cmd="$3" after="$4"
    shift 4
    local envs=("$@") env_systemd="" env_shell="" kv
    for kv in "${envs[@]}"; do
        env_systemd+="Environment=$kv"$'\n'
        env_shell+="export $kv"$'\n'
    done

    if command -v systemctl &>/dev/null; then
        log "Installing $name as a systemd service (Restart=always, enabled on boot)..."
        cat > "/etc/systemd/system/$name.service" <<EOF
[Unit]
Description=$desc
After=network-online.target${after:+ $after}
Wants=network-online.target${after:+ $after}

[Service]
Type=simple
ExecStart=/bin/sh -c '$exec_cmd'
Restart=always
RestartSec=5s
$env_systemd
[Install]
WantedBy=multi-user.target
EOF
        systemctl daemon-reload
        systemctl enable --now "$name.service"
        sleep 2
        systemctl is-active --quiet "$name.service" \
            || warn "$name.service didn't come up cleanly — check: journalctl -u $name -n 50"
    elif command -v rc-service &>/dev/null && command -v rc-update &>/dev/null; then
        log "Installing $name as an OpenRC service (supervised, auto-restart, added to boot)..."
        cat > "/etc/init.d/$name" <<EOF
#!/sbin/openrc-run
description="$desc"

$env_shell
supervisor="supervise-daemon"
command="/bin/sh"
command_args="-c '$exec_cmd'"
respawn_max=0
respawn_delay=5

depend() {
    need net
$( [[ -n "$after" ]] && echo "    after ${after%.service}" )
}
EOF
        chmod +x "/etc/init.d/$name"
        rc-update add "$name" default 2>/dev/null || true
        rc-service "$name" start
        sleep 2
        rc-service "$name" status 2>&1 | grep -qi started \
            || warn "$name OpenRC service didn't come up cleanly — check: rc-service $name status"
    else
        warn "No systemd or OpenRC on this system — falling back to a self-restarting background loop \
for $name. Not a real service; set up this system's actual init/service manager to run \
'$exec_cmd' persistently when you can."
        local supervisor="$WORK_DIR/$name-supervisor.sh"
        cat > "$supervisor" <<EOF
#!/usr/bin/env bash
$env_shell
while true; do
    $exec_cmd
    sleep 5
done
EOF
        chmod +x "$supervisor"
        nohup "$supervisor" >"$LOG_DIR/$name.log" 2>&1 &
        echo $! > "$WORK_DIR/$name.pid"
        sleep 2
        if command -v crontab &>/dev/null; then
            ( crontab -l 2>/dev/null | grep -vF "$supervisor"
              echo "@reboot $supervisor >>$LOG_DIR/$name.log 2>&1 &" ) | crontab - \
                && log "Added a cron @reboot entry, so $name also restarts after a reboot." \
                || warn "Couldn't add a cron @reboot entry — $name will NOT survive a reboot on this system."
        else
            warn "No cron either — $name will NOT survive a reboot on this system."
        fi
    fi
}

# Undoes install_supervised_service() for a given name — stops/disables/
# removes whichever tier was used, best-effort across all three since we
# don't track which one a given machine got. Every step is guarded (`if`
# conditions or explicit `|| true`) rather than a bare `&&`/bare command:
# under `set -e`, a bare failing command — e.g. `pkill` finding nothing to
# kill, or `systemctl stop` on a unit that's already stopped/missing — kills
# the whole uninstall right there, silently short-circuiting everything
# after it. Confirmed for real: exactly this class of bug (in the
# nodelet-specific twin of this function, stop_running_components() in
# uninstall.sh) made --uninstall stop nodelet and then exit without ever
# reaching the k3s-uninstall.sh call.
remove_supervised_service() {
    local name="$1"
    if command -v systemctl &>/dev/null; then
        systemctl disable --now "$name.service" 2>/dev/null || true
        rm -f "/etc/systemd/system/$name.service"
        systemctl daemon-reload 2>/dev/null || true
    fi
    if command -v rc-update &>/dev/null; then
        rc-service "$name" stop 2>/dev/null || true
        rc-update del "$name" default 2>/dev/null || true
        rm -f "/etc/init.d/$name"
    fi
    if command -v crontab &>/dev/null; then
        ( crontab -l 2>/dev/null | grep -vF "$WORK_DIR/$name-supervisor.sh" ) | crontab - 2>/dev/null || true
    fi
    pkill -f "$WORK_DIR/$name-supervisor.sh" 2>/dev/null || true
    rm -f "$WORK_DIR/$name-supervisor.sh"
}
