#!/usr/bin/env bash
set -Eeuo pipefail

machine_id="$(tr -d '-' </proc/sys/kernel/random/uuid)"
printf '%s\n' "$machine_id" >/etc/machine-id
chmod 0444 /etc/machine-id
if ! mount --make-rshared /; then
    echo "failed to make the simulated node root mount recursively shared" >&2
    exit 1
fi
if [[ "$(findmnt -n -o PROPAGATION /)" != shared ]]; then
    echo "simulated node root mount is not recursively shared" >&2
    exit 1
fi
systemd="$(readlink -f /sbin/init)"
echo "starting systemd at $systemd with console logging"
exec "$systemd" --system --log-target=console --log-level=debug
