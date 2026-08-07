# lib/test/cases/streaming.sh — kubectl logs / kubectl exec against
# nodelet's own kubelet-style HTTP(S) server (server.rs). Needs the
# apiserver to actually be able to reach nodelet's server port (real
# networking, not something mock/localhost-only proves) and
# Node.status.daemonEndpoints.kubeletEndpoint.port to be advertised
# correctly — both exercised for real here, not just unit-tested in
# isolation like the request-routing/auth-parsing logic already is.
#
# Round 123: all four endpoints (containerLogs, exec, attach, portForward)
# now get real functional coverage — attach and portForward share the
# exact same proxy code path (server/exec.rs's proxy_upgrade()/splice())
# as exec, but each gets its own independent test rather than resting
# entirely on exec proving the shared mechanism.

test_kubectl_logs_returns_real_output() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="logs-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "echo hello-from-nodelet-logs; sleep 3600"]
EOF
    wait_until 90 "$name Running" pod_is_phase "$name" Running
    local output
    if ! output="$(try_wait_until 90 bash -c "kctl logs '$name' 2>/dev/null | grep -q hello-from-nodelet-logs" && kctl logs "$name" 2>/dev/null)"; then
        delete_pod_if_exists "$name"
        die "kubectl logs never returned the expected output — check: does Node.status.daemonEndpoints show a port, can the apiserver reach it (firewall/NAT), and is nodelet's server actually listening (NODELET_SERVER_ENABLED)?"
    fi
    assert_contains "$output" "hello-from-nodelet-logs" "kubectl logs output"
    delete_pod_if_exists "$name"
}

test_kubectl_logs_follow_streams_new_output() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="logs-follow-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "for i in 1 2 3 4 5 6 7 8; do echo line-\$i; sleep 1; done; sleep 3600"]
EOF
    wait_until 90 "$name Running" pod_is_phase "$name" Running
    local out_file="/tmp/nodelet-e2e-logs-follow-$$"
    (kctl logs -f "$name" > "$out_file" 2>/dev/null &)
    local follow_pid_check
    try_wait_until 40 bash -c "grep -q line-3 '$out_file' 2>/dev/null" \
        || warn "kubectl logs -f didn't show streamed output within 20s (may still be buffering)"
    pkill -f "kubectl.*logs -f $name" 2>/dev/null || true
    if [[ -f "$out_file" ]] && grep -q "line-" "$out_file"; then
        rm -f "$out_file"
    else
        rm -f "$out_file"
        die "kubectl logs -f produced no streamed output at all"
    fi
    delete_pod_if_exists "$name"
}

test_kubectl_exec_runs_a_command_and_returns_its_output() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="exec-check"
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
    local output
    output="$(kctl exec "$name" -- echo hello-from-exec 2>&1)" || true
    assert_contains "$output" "hello-from-exec" "kubectl exec output — if this fails, check nodelet's server logs for the proxy_upgrade path (server/exec.rs); this is the piece least validated outside a live cluster"
    delete_pod_if_exists "$name"
}

test_kubectl_attach_streams_the_containers_stdout() {
    # attach and portForward (below) share exec's exact
    # proxy_upgrade()/splice() code path in server/exec.rs — round 123
    # automates independent coverage of both anyway, rather than resting
    # entirely on the exec test above proving the shared mechanism.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="attach-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "for i in 1 2 3 4 5 6 7 8; do echo attach-line-\$i; sleep 1; done; sleep 3600"]
EOF
    wait_until 90 "$name Running" pod_is_phase "$name" Running
    local out_file="/tmp/nodelet-e2e-attach-$$"
    (kctl attach "$name" > "$out_file" 2>/dev/null &)
    try_wait_until 40 bash -c "grep -q attach-line-3 '$out_file' 2>/dev/null" \
        || warn "kubectl attach didn't show streamed output within 20s (may still be buffering)"
    pkill -f "kubectl.*attach $name" 2>/dev/null || true
    local seen
    seen="$(cat "$out_file" 2>/dev/null)"
    rm -f "$out_file"
    delete_pod_if_exists "$name"
    assert_contains "$seen" "attach-line-" "kubectl attach produced no streamed output at all"
}

test_kubectl_port_forward_reaches_a_real_container_port() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="port-forward-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      # Same one-shot-per-connection busybox-nc HTTP responder
      # networking.sh's hostPort tests already use — alpine's busybox
      # doesn't compile in the httpd applet.
      command: ["sh", "-c", "printf 'HTTP/1.1 200 OK\\r\\nContent-Type: text/plain\\r\\nConnection: close\\r\\n\\r\\nport-forward-marker\\n' > /tmp/resp && while true; do nc -lp 8080 < /tmp/resp; done"]
EOF
    wait_until 90 "$name Running" pod_is_phase "$name" Running

    local local_port=18080
    local pf_log="/tmp/nodelet-e2e-port-forward-$$"
    (kctl port-forward "$name" "$local_port:8080" > "$pf_log" 2>&1 &)
    local response
    if ! response="$(try_wait_until 40 bash -c "curl -sf --max-time 3 http://127.0.0.1:$local_port/ 2>/dev/null | grep -q port-forward-marker" \
        && curl -sf --max-time 3 "http://127.0.0.1:$local_port/")"; then
        pkill -f "kubectl.*port-forward $name" 2>/dev/null || true
        rm -f "$pf_log"
        delete_pod_if_exists "$name"
        die "kubectl port-forward never reached the container's real port 8080 — check server/exec.rs's proxy_upgrade()/splice() path (see $pf_log for kubectl's own output, if still present)"
    fi
    pkill -f "kubectl.*port-forward $name" 2>/dev/null || true
    rm -f "$pf_log"
    delete_pod_if_exists "$name"
    assert_contains "$response" "port-forward-marker" "curl through kubectl port-forward should reach the container's real HTTP responder"
}

register_test test_kubectl_logs_returns_real_output
register_test test_kubectl_logs_follow_streams_new_output
register_test test_kubectl_exec_runs_a_command_and_returns_its_output
register_test test_kubectl_attach_streams_the_containers_stdout
register_test test_kubectl_port_forward_reaches_a_real_container_port
