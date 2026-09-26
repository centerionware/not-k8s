#!/usr/bin/env bash
set -Eeuo pipefail

machine_id="$(tr -d '-' </proc/sys/kernel/random/uuid)"
printf '%s\n' "$machine_id" >/etc/machine-id
chmod 0444 /etc/machine-id
systemd="$(readlink -f /sbin/init)"
echo "starting systemd at $systemd with console logging"
exec "$systemd" --system --log-target=console --log-level=debug
