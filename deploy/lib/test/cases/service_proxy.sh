# lib/test/cases/service_proxy.sh — ClusterIP/NodePort routing, i.e. the
# `nodeproxy` binary (crates/nodeproxy/src/svc.rs), kube-proxy's job.
#
# Written when that code was split out of nodelet into its own binary, and
# overdue independently of the split: before this file, service networking
# had NO e2e coverage at all. Its only verification was svc.rs's three
# inline unit tests, and those self-skip when `nft` is missing OR when
# `nft -c` produces no output (no CAP_NET_ADMIN) — so a broken ruleset, a
# proxy that never started, or a Service whose backends never resolved
# would all have passed CI silently.
#
# These tests curl real addresses from the host: a ClusterIP (which no
# interface owns — reaching it at all proves the nat/output DNAT rule
# exists) and the node's own IP at a NodePort. Nothing here inspects
# nodeproxy's internals; if the traffic lands in the container, the whole
# chain worked.

# A one-shot-per-connection HTTP responder, same busybox `nc -lp` trick
# cases/networking.sh uses and for the same reason: alpine's busybox has no
# httpd applet (confirmed live), which used to surface as a misleading
# "pod never reached Running".
_svc_responder_command() { # _svc_responder_command <marker> <port>
    echo "[\"sh\", \"-c\", \"printf 'HTTP/1.1 200 OK\\\\r\\\\nContent-Type: text/plain\\\\r\\\\nConnection: close\\\\r\\\\n\\\\r\\\\n$1\\\\n' > /tmp/resp && while true; do nc -lp $2 < /tmp/resp; done\"]"
}

# nft needs CAP_NET_ADMIN and this suite runs unprivileged (other cases —
# credential_provider.sh, log_rotation.sh — reach for sudo the same way).
# Try direct first so a root-run suite doesn't need sudo installed at all.
_nft() {
    if nft "$@" 2>/dev/null; then return 0; fi
    command -v sudo >/dev/null 2>&1 && sudo nft "$@"
}

_delete_svc_if_exists() {
    kctl delete service "$1" --ignore-not-found --wait=false >/dev/null 2>&1 || true
}

# Every test here needs a real CRI runtime (pods with real IPs to route to),
# a usable nft, and a service proxy actually installed on this node —
# --proxy=none is a legitimate deployment (something else owns the
# datapath) and must skip, not fail.
_require_service_proxy() {
    node_uses_cri_runtime || skip_test "needs cri runtime (mock pods have no real IPs to route to)"
    command -v nft >/dev/null 2>&1 || skip_test "needs nft (nftables) on the host"
    _nft list table inet not_k8s_svc >/dev/null 2>&1 \
        || skip_test "no readable 'inet not_k8s_svc' table — either nodeproxy isn't running on this node (deployed with --proxy=none?) or this suite can't reach nftables (needs root or passwordless sudo)"
}

# Waits until the Service's EndpointSlice actually has a ready address.
# nodeproxy programs nothing for a Service with no backends (dnat_target()
# returns None for zero backends), so curling before this is just a race
# against the control plane's own EndpointSlice controller, not a proxy bug.
_wait_for_ready_endpoint() { # _wait_for_ready_endpoint <service-name> <timeout>
    try_wait_until "$2" bash -c \
        "kubectl get endpointslices -n '$TEST_NAMESPACE' -l kubernetes.io/service-name='$1' \
         -o jsonpath='{.items[*].endpoints[*].addresses[0]}' 2>/dev/null | grep -q ."
}

test_clusterip_service_routes_to_its_backend_pod() {
    _require_service_proxy
    local name="svc-clusterip-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
  labels:
    app: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: $(_svc_responder_command clusterip-marker 8080)
      ports:
        - containerPort: 8080
---
apiVersion: v1
kind: Service
metadata:
  name: $name
spec:
  selector:
    app: $name
  ports:
    - port: 80
      targetPort: 8080
EOF
    if ! try_wait_until 90 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"; _delete_svc_if_exists "$name"
        die "backend pod never reached Running — this is a pod-lifecycle failure, not a service-routing one; check nodelet's logs first"
    fi
    if ! _wait_for_ready_endpoint "$name" 60; then
        delete_pod_if_exists "$name"; _delete_svc_if_exists "$name"
        die "the Service's EndpointSlice never got a ready address — that's the control plane's endpointslice controller, upstream of nodeproxy, which programs nothing for a backend-less Service by design"
    fi

    local cluster_ip body
    cluster_ip="$(kctl get service "$name" -o jsonpath='{.spec.clusterIP}')"
    # A ClusterIP is owned by no interface anywhere — if the nat/output
    # DNAT rule isn't there, this connection has nowhere to go at all.
    if ! body="$(try_wait_until 60 bash -c "curl -sS --max-time 5 http://$cluster_ip:80/ | grep -q clusterip-marker" \
        && curl -sS --max-time 5 "http://$cluster_ip:80/")"; then
        echo "--- nft ruleset at failure ---"; _nft list table inet not_k8s_svc 2>&1 || true
        delete_pod_if_exists "$name"; _delete_svc_if_exists "$name"
        die "curling ClusterIP $cluster_ip never reached the backend pod — check nodeproxy's log (journalctl -u nodeproxy) and build_ruleset()/dnat_target() in crates/nodeproxy/src/svc.rs against the ruleset dumped above"
    fi
    assert_contains "$body" "clusterip-marker" "response body from curling the Service's ClusterIP"
    delete_pod_if_exists "$name"; _delete_svc_if_exists "$name"
}

test_nodeport_service_is_reachable_on_the_node_ip() {
    _require_service_proxy
    local name="svc-nodeport-check"
    local node_port=31890
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
  labels:
    app: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: $(_svc_responder_command nodeport-marker 8080)
      ports:
        - containerPort: 8080
---
apiVersion: v1
kind: Service
metadata:
  name: $name
spec:
  type: NodePort
  selector:
    app: $name
  ports:
    - port: 80
      targetPort: 8080
      nodePort: $node_port
EOF
    if ! try_wait_until 90 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"; _delete_svc_if_exists "$name"
        die "backend pod never reached Running — a pod-lifecycle failure, not a service-routing one"
    fi
    if ! _wait_for_ready_endpoint "$name" 60; then
        delete_pod_if_exists "$name"; _delete_svc_if_exists "$name"
        die "the Service's EndpointSlice never got a ready address"
    fi

    # The NodePort rule is the one that needed an explicit family qualifier
    # (`dnat ip to ...`) because `fib daddr type local` doesn't imply one —
    # see svc.rs's module doc. Without that, nft fails silently (non-zero
    # exit, no error text), which is exactly the kind of break this test
    # exists to catch.
    local node_ip body
    node_ip="$(kubectl get node "$(node_name)" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')"
    if ! body="$(try_wait_until 60 bash -c "curl -sS --max-time 5 http://$node_ip:$node_port/ | grep -q nodeport-marker" \
        && curl -sS --max-time 5 "http://$node_ip:$node_port/")"; then
        echo "--- nft ruleset at failure ---"; _nft list table inet not_k8s_svc 2>&1 || true
        delete_pod_if_exists "$name"; _delete_svc_if_exists "$name"
        die "curling $node_ip:$node_port never reached the backend pod — check the 'fib daddr type local ... dnat ip to' rules in the ruleset dumped above, and that this host's firewall allows the test script to reach that port"
    fi
    assert_contains "$body" "nodeport-marker" "response body from curling the node's own IP at the NodePort"
    delete_pod_if_exists "$name"; _delete_svc_if_exists "$name"
}

test_service_with_no_endpoints_does_not_wedge_the_ruleset() {
    _require_service_proxy
    # The whole ruleset is rebuilt and re-applied atomically on every
    # Service/EndpointSlice event. A Service that resolves to zero backends
    # is the case most likely to emit something nft rejects — and because
    # `nft -f -` applies the file as a unit, one bad rule takes down ALL
    # service routing on the node, not just that Service's. So: stand up a
    # working Service, add a backend-less one beside it, and confirm the
    # working one still works.
    local live="svc-endpointless-live" dead="svc-endpointless-dead"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $live
  labels:
    app: $live
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: $(_svc_responder_command endpointless-marker 8080)
      ports:
        - containerPort: 8080
---
apiVersion: v1
kind: Service
metadata:
  name: $live
spec:
  selector:
    app: $live
  ports:
    - port: 80
      targetPort: 8080
EOF
    if ! try_wait_until 90 pod_is_phase "$live" Running; then
        delete_pod_if_exists "$live"; _delete_svc_if_exists "$live"
        die "backend pod never reached Running"
    fi
    if ! _wait_for_ready_endpoint "$live" 60; then
        delete_pod_if_exists "$live"; _delete_svc_if_exists "$live"
        die "the live Service's EndpointSlice never got a ready address"
    fi

    apply_manifest <<EOF
apiVersion: v1
kind: Service
metadata:
  name: $dead
spec:
  selector:
    app: nothing-matches-this-selector
  ports:
    - port: 80
      targetPort: 8080
EOF

    local cluster_ip body
    cluster_ip="$(kctl get service "$live" -o jsonpath='{.spec.clusterIP}')"
    if ! body="$(try_wait_until 60 bash -c "curl -sS --max-time 5 http://$cluster_ip:80/ | grep -q endpointless-marker" \
        && curl -sS --max-time 5 "http://$cluster_ip:80/")"; then
        echo "--- nft ruleset at failure ---"; _nft list table inet not_k8s_svc 2>&1 || true
        delete_pod_if_exists "$live"; _delete_svc_if_exists "$live"; _delete_svc_if_exists "$dead"
        die "a backend-less Service broke routing for an unrelated, healthy Service — the whole ruleset is applied atomically, so a rule nft rejects takes everything down together. Check build_ruleset() against the dump above"
    fi
    assert_contains "$body" "endpointless-marker" "healthy Service still routes with a backend-less Service present"
    # And the table itself is still parseable, not left in some half state.
    assert_true _nft list table inet not_k8s_svc

    delete_pod_if_exists "$live"; _delete_svc_if_exists "$live"; _delete_svc_if_exists "$dead"
}

test_nodeproxy_runs_as_its_own_service_separate_from_nodelet() {
    # Cheap packaging assertion: the split's whole point is that Service
    # routing is a separate, independently replaceable process. If the unit
    # never got installed, the routing tests above would fail with a
    # confusing "curl never reached the pod"; this fails with the real
    # reason instead.
    node_uses_cri_runtime || skip_test "needs cri runtime"
    command -v systemctl >/dev/null 2>&1 || skip_test "not a systemd host (OpenRC/fallback tiers aren't asserted here)"
    systemctl list-unit-files nodeproxy.service >/dev/null 2>&1 \
        && systemctl cat nodeproxy.service >/dev/null 2>&1 \
        || skip_test "no nodeproxy.service installed (deployed with --proxy=none?)"

    assert_true systemctl is-active --quiet nodeproxy.service
    # Independent of nodelet, deliberately: neither unit orders against the
    # other (see deploy/lib/nodeproxy-service.sh's header).
    local unit
    unit="$(systemctl cat nodeproxy.service)"
    assert_not_contains "$unit" "nodelet.service" "nodeproxy.service must not order against nodelet.service — they're independent components"
    assert_contains "$unit" "run-nodeproxy.sh" "nodeproxy.service ExecStart"
}

register_test test_clusterip_service_routes_to_its_backend_pod
register_test test_nodeport_service_is_reachable_on_the_node_ip
register_test test_service_with_no_endpoints_does_not_wedge_the_ruleset
register_test test_nodeproxy_runs_as_its_own_service_separate_from_nodelet
