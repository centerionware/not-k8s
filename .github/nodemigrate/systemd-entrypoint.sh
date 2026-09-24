#!/usr/bin/env bash
set -Eeuo pipefail

machine_id="$(tr -d '-' </proc/sys/kernel/random/uuid)"
printf '%s\n' "$machine_id" >/etc/machine-id
chmod 0444 /etc/machine-id
exec /sbin/init
