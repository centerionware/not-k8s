# lib/test/cases/stats.sh — /stats/summary (server::stats). Note this only
# proves nodelet's endpoint itself works — `kubectl top` additionally needs
# metrics-server (or another metrics.k8s.io implementation) deployed and
# configured to scrape it, which this suite doesn't set up.

test_stats_summary_returns_real_pod_usage() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="stats-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
EOF
    wait_until 90 "$name Running" pod_is_phase "$name" Running

    local node_ip token json
    node_ip="$(kubectl get node "$(node_name)" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')"
    token="$(kubectl create token default --duration=5m 2>/dev/null || kubectl -n default create token default 2>/dev/null)"
    if [[ -z "$token" ]]; then
        delete_pod_if_exists "$name"
        skip_test "couldn't mint a bearer token (kubectl create token) to call the server directly — needs a cluster new enough to support the TokenRequest-backed 'kubectl create token'"
    fi

    if ! json="$(try_wait_until 90 bash -c "curl -ksS --max-time 5 -H 'Authorization: Bearer $token' https://$node_ip:${NODELET_SERVER_PORT:-10250}/stats/summary | grep -q '\"$name\"'" \
        && curl -ksS --max-time 5 -H "Authorization: Bearer $token" "https://$node_ip:${NODELET_SERVER_PORT:-10250}/stats/summary")"; then
        delete_pod_if_exists "$name"
        die "/stats/summary never mentioned pod '$name' — check nodelet's server logs, and that this node's firewall allows the test script to reach NODELET_SERVER_PORT directly"
    fi

    assert_contains "$json" "\"nodeName\"" "Summary.node.nodeName present"
    assert_contains "$json" "\"podRef\"" "Summary.pods[].podRef present"
    assert_contains "$json" "$name" "the running pod appears in the summary"
    delete_pod_if_exists "$name"
}

register_test test_stats_summary_returns_real_pod_usage
