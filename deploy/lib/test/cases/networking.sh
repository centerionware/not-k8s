# lib/test/cases/networking.sh — spec.containers[].ports[].hostPort
# (round 82; found in round 80's re-audit). See runtime/cri/sandbox.rs's
# port_mappings_for().

test_host_port_publishes_the_container_on_the_nodes_own_ip() {
    # Real, structural proof: curl the NODE's own IP (not the pod's IP,
    # not localhost inside the container) at the hostPort and confirm
    # the response actually comes from this container -- if hostPort
    # weren't wired up at all, nothing would be listening there.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="host-port-check"
    local host_port=18080
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      # Not 'busybox httpd' — alpine:3.20's busybox build doesn't compile
      # that applet in ('httpd: applet not found', confirmed live), which
      # made the container exit immediately (CrashLoopBackOff) and this
      # test misreport that as "pod never reached Running" — nothing to
      # do with hostPort/port_mappings_for() wiring at all. busybox nc's
      # -lp does listen-on-port, so a tiny loop around it makes a
      # good-enough one-shot-per-connection HTTP responder without
      # needing a real web server in the test image.
      command: ["sh", "-c", "printf 'HTTP/1.1 200 OK\\r\\nContent-Type: text/plain\\r\\nConnection: close\\r\\n\\r\\nhost-port-marker\\n' > /tmp/resp && while true; do nc -lp 8080 < /tmp/resp; done"]
      ports:
        - containerPort: 8080
          hostPort: $host_port
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        die "pod never reached Running with hostPort set — check nodelet's logs for a RunPodSandbox error (the runtime may not support CRI's port_mappings, or the host port may already be in use on this node)"
    fi

    local node_ip body
    node_ip="$(kubectl get node "$(node_name)" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')"
    if ! body="$(try_wait_until 30 bash -c "curl -sS --max-time 5 http://$node_ip:$host_port/marker | grep -q host-port-marker" \
        && curl -sS --max-time 5 "http://$node_ip:$host_port/marker")"; then
        delete_pod_if_exists "$name"
        die "curling the node's own IP at hostPort $host_port never reached this pod's container — check port_mappings_for()/sandbox_config() wiring in runtime/cri/sandbox.rs, and that this node's firewall allows the test script to reach that port directly"
    fi
    assert_contains "$body" "host-port-marker" "response body from curling the node's own IP at the configured hostPort"
    delete_pod_if_exists "$name"
}

test_host_network_pod_needs_no_explicit_port_mapping() {
    # hostNetwork pods share the host's own network namespace already --
    # containerPort == hostPort trivially there, and real kubelet never
    # sends explicit port_mappings for them either (port_mappings_for()
    # returns empty for host_network: true). This just confirms such a
    # pod still reaches Running and its own containerPort is directly
    # reachable on the node's IP with no hostPort set at all.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="host-network-port-check"
    local container_port=18081
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  hostNetwork: true
  containers:
    - name: app
      image: $TEST_IMAGE
      # See the hostPort test above for why this isn't 'busybox httpd'.
      command: ["sh", "-c", "printf 'HTTP/1.1 200 OK\\r\\nContent-Type: text/plain\\r\\nConnection: close\\r\\n\\r\\nhost-network-marker\\n' > /tmp/resp && while true; do nc -lp $container_port < /tmp/resp; done"]
      ports:
        - containerPort: $container_port
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        die "pod never reached Running with hostNetwork: true"
    fi

    local node_ip body
    node_ip="$(kubectl get node "$(node_name)" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')"
    if ! body="$(try_wait_until 20 bash -c "curl -sS --max-time 5 http://$node_ip:$container_port/marker | grep -q host-network-marker" \
        && curl -sS --max-time 5 "http://$node_ip:$container_port/marker")"; then
        delete_pod_if_exists "$name"
        die "curling the node's own IP at containerPort $container_port never reached this hostNetwork pod's container"
    fi
    assert_contains "$body" "host-network-marker" "response body from a hostNetwork pod's own containerPort"
    delete_pod_if_exists "$name"
}

register_test test_host_port_publishes_the_container_on_the_nodes_own_ip
register_test test_host_network_pod_needs_no_explicit_port_mapping
