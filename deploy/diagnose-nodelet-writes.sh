#!/usr/bin/env bash
# diagnose-nodelet-writes.sh — capture nodelet's actual per-pod reconcile
# activity (ensure_pod/write_status — logged at `debug!`, invisible at the
# default RUST_LOG=info) during a real k3s cold start, correlated with k3s's
# own ReplicaSet-controller sync failures, to settle whether nodelet's write
# traffic against the shared apiserver/kine backend is what's driving the
# coredns pod pile-up.
#
# This DOES restart k3s and nodelet on this machine (that's what reproduces
# the cold-start window the bug happens in) — same disruption as the
# uninstall/reinstall or reboot cycles already being used to trigger it.
#
# Usage:
#   ./deploy/diagnose-nodelet-writes.sh
set -uo pipefail

OUT="/tmp/notk8s-diagnose-writes-$(date +%Y%m%d-%H%M%S).txt"
DROPIN_DIR=/etc/systemd/system/nodelet.service.d
DROPIN="$DROPIN_DIR/debug-logging.conf"

exec > >(tee "$OUT") 2>&1

echo "=== enabling debug logging for nodelet (temporary systemd drop-in) ==="
sudo mkdir -p "$DROPIN_DIR"
sudo tee "$DROPIN" > /dev/null <<'EOF'
[Service]
Environment=RUST_LOG=nodelet=debug,info
EOF
sudo systemctl daemon-reload

echo "=== restarting k3s (this is what triggers the cold-start burst) ==="
sudo systemctl restart k3s

echo "=== restarting nodelet with debug logging ==="
sudo systemctl restart nodelet

echo "=== capturing 10s of activity from both, interleaved by real time ==="
sleep 10

echo "=== nodelet journal (debug level) for this window ==="
sudo journalctl -u nodelet --since "20 seconds ago" --no-pager -o short-iso-precise

echo "=== k3s journal (replicaset/coredns lines only) for this window ==="
sudo journalctl -u k3s --since "20 seconds ago" --no-pager -o short-iso-precise | grep -iE "replicaset|coredns|taint-eviction"

echo "=== current coredns pod count ==="
sudo k3s kubectl get pods -n kube-system -l k8s-app=kube-dns --no-headers | wc -l

echo "=== reverting nodelet to normal (info-level) logging ==="
sudo rm -f "$DROPIN"
sudo systemctl daemon-reload
sudo systemctl restart nodelet

echo "=== done ==="
echo "Full output saved to: $OUT"
echo "Paste this whole file back, or run: cat $OUT"
