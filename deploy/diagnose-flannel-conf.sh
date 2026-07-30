#!/usr/bin/env bash
# diagnose-flannel-conf.sh — is the fixed net-conf.json write actually on
# disk and did it actually run? Three separate questions that look the
# same from the outside but need different fixes:
#   1. Is the checked-out run-flanneld.sh the fixed (self-healing) version?
#   2. Does /etc/kube-flannel/net-conf.json exist, and when was it written?
#   3. Was ensure_cni() (deploy/lib/cni.sh) even reached on the last run
#      (WITH_CRI and CNI_PLUGIN gate it — wrong flags silently skip it)?
set -uo pipefail

echo "=== git commit checked out ==="
git -C "$(dirname "$0")/.." log -1 --oneline

echo "=== does run-flanneld.sh on disk contain the self-healing net-conf.json write? ==="
grep -n "on every.*single start\|net-conf.json" "$(dirname "$0")/run-flanneld.sh" || echo "STRING NOT FOUND — grep for context below:"
sed -n '1,55p' "$(dirname "$0")/run-flanneld.sh"

echo "=== does deploy/lib/cni.sh's ensure_cni() call start_flanneld()? ==="
grep -n "start_flanneld\|ensure_cni()" "$(dirname "$0")/lib/cni.sh"

echo "=== /etc/kube-flannel/net-conf.json ==="
sudo ls -la /etc/kube-flannel/ 2>&1
sudo cat /etc/kube-flannel/net-conf.json 2>&1

echo "=== /run/flannel/subnet.env ==="
cat /run/flannel/subnet.env 2>&1
