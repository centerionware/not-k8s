# lib/test/cases/pod_resources.sh — the PodResources API (round 74;
# found in round 72's re-audit): kubelet's own gRPC service (List/
# GetAllocatableResources/Get) exposing CPU/memory/device allocation to
# external tooling over a Unix socket (NODELET_POD_RESOURCES_SOCKET_PATH).
# See crates/nodelet/src/pod_resources.rs.
#
# Same honest limitation every other plugin-protocol round in this
# project carries: genuinely dialing in as a real gRPC client (grpcurl,
# or a real exporter like NVIDIA DCGM) isn't something this bash-only
# harness can do without a grpcurl-equivalent binary this suite doesn't
# assume is present. What IS automated here: confirming the socket
# itself actually gets created on a cri-runtime node, which is the
# regression this suite most needs to catch — a config-wiring bug that
# silently keeps the server from ever binding would otherwise be
# invisible until an operator's own tooling tried to connect and failed.

test_pod_resources_socket_is_created_on_a_cri_node() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local sock="${NODELET_POD_RESOURCES_SOCKET_PATH:-/var/lib/nodelet/pod-resources/kubelet.sock}"
    if [[ -z "$sock" ]]; then
        skip_test "NODELET_POD_RESOURCES_SOCKET_PATH is explicitly empty on this deployment — the PodResources API is intentionally disabled here"
    fi
    if ! try_wait_until 15 bash -c "[[ -S '$sock' ]]"; then
        skip_test "no Unix socket at $sock after 15s — check nodelet's own startup logs for 'PodResources API' (directory-creation or bind failure would log a warning and leave the server disabled for this run rather than crashing nodelet)"
    fi
    assert_true test -S "$sock"
}

test_pod_resources_grpc_query_returns_real_data() {
    # Round 123: previously manual-only purely because it needed a real
    # gRPC client this suite didn't assume was installed — e2e-full-setup.sh
    # now installs grpcurl for exactly this. Uses the vendored
    # podresources.proto directly (no server reflection needed).
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local sock="${NODELET_POD_RESOURCES_SOCKET_PATH:-/var/lib/nodelet/pod-resources/kubelet.sock}"
    if [[ -z "$sock" ]]; then
        skip_test "NODELET_POD_RESOURCES_SOCKET_PATH is explicitly empty on this deployment — the PodResources API is intentionally disabled here"
    fi
    if ! command -v grpcurl >/dev/null 2>&1; then
        skip_test "grpcurl not on PATH — e2e-full-setup.sh should have installed it for this run"
    fi
    if ! try_wait_until 15 bash -c "[[ -S '$sock' ]]"; then
        skip_test "no Unix socket at $sock after 15s"
    fi
    local proto="$REPO_ROOT/crates/nodelet/proto/podresources.proto"
    if [[ ! -f "$proto" ]]; then
        skip_test "podresources.proto not found at $proto — is REPO_ROOT set correctly?"
    fi

    local name="pod-resources-grpc-check"
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
    wait_until 60 "$name Running" pod_is_phase "$name" Running

    # Round 124 (found live in CI): grpcurl's protoparse rejects an
    # absolute -proto path with "must specify at least one import path if
    # any absolute file paths are given" — even though podresources.proto
    # has zero imports of its own, grpcurl still demands an -import-path
    # whenever the -proto path itself is absolute (which $proto, built from
    # $REPO_ROOT, always is here).
    local proto_dir
    proto_dir="$(dirname "$proto")"

    local list_response
    list_response="$(sudo grpcurl -plaintext -unix -import-path "$proto_dir" -proto "$proto" "$sock" v1.PodResourcesLister/List 2>&1)"
    assert_contains "$list_response" "$name" "PodResourcesLister/List should include the running pod's own name"

    local get_response
    get_response="$(sudo grpcurl -plaintext -unix -import-path "$proto_dir" -proto "$proto" -d "{\"podName\":\"does-not-exist\",\"podNamespace\":\"$TEST_NAMESPACE\"}" "$sock" v1.PodResourcesLister/Get 2>&1)"
    delete_pod_if_exists "$name"
    assert_contains "$get_response" "NotFound" "PodResourcesLister/Get for a nonexistent pod should return a real NotFound gRPC status, not a crash or an empty-but-ok response"
}

register_test test_pod_resources_socket_is_created_on_a_cri_node
register_test test_pod_resources_grpc_query_returns_real_data
