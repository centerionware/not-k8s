#!/usr/bin/env bash
# diagnose-oom.sh — what got killed / what triggered a machine-wide OOM
# reboot, and roughly how much memory each of our own processes is using
# right now so a repeat can be caught before it happens again.
#
# Usage:
#   sudo ./deploy/diagnose-oom.sh
set -uo pipefail

echo "=== kernel OOM-killer events (this boot + previous, if kept) ==="
sudo journalctl -k --no-pager | grep -iE "oom|out of memory|killed process" | tail -60

echo "=== journalctl -b -1 tail (last ~80 lines before the reboot, if the previous boot's log survived) ==="
sudo journalctl -b -1 --no-pager 2>&1 | tail -80

echo "=== current memory ==="
free -h

echo "=== current RSS of our own processes ==="
ps -eo pid,ppid,rss,pcpu,etime,cmd --sort=-rss | grep -iE "nodelet|k3s|flanneld|containerd|pause|PID" | grep -v grep

echo "=== systemd-managed cgroup memory (if systemd is in use) ==="
for u in k3s nodelet flanneld containerd; do
    systemctl show "$u" -p MemoryCurrent -p MemoryMax 2>/dev/null | sed "s/^/$u: /"
done
