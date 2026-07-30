#!/usr/bin/env bash
# diagnose-flannel-conf.sh — is the fixed write_flannel_net_conf() actually
# on disk and did it actually run? Three separate questions that look the
# same from the outside but need different fixes:
#   1. Is the checked-out bootstrap-test.sh the fixed version at all?
#   2. Does /etc/kube-flannel/net-conf.json exist, and when was it written?
#   3. Was ensure_cni()/write_flannel_net_conf() even reached on the last
#      run (WITH_CRI and CNI_PLUGIN gate it — wrong flags silently skip it)?
set -uo pipefail

echo "=== git commit checked out ==="
git -C "$(dirname "$0")/.." log -1 --oneline

echo "=== does bootstrap-test.sh on disk contain the unconditional net-conf.json write? ==="
grep -n "No custom config needed\|net-conf.json is required for every IP family" "$(dirname "$0")/bootstrap-test.sh" || echo "NEITHER STRING FOUND — grep for context below:"
sed -n '1055,1080p' "$(dirname "$0")/bootstrap-test.sh"

echo "=== /etc/kube-flannel/net-conf.json ==="
sudo ls -la /etc/kube-flannel/ 2>&1
sudo cat /etc/kube-flannel/net-conf.json 2>&1

echo "=== /run/flannel/subnet.env ==="
cat /run/flannel/subnet.env 2>&1
