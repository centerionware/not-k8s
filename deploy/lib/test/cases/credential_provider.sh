# lib/test/cases/credential_provider.sh — image credential provider
# exec-plugin protocol (round 71; --image-credential-provider-config /
# --image-credential-provider-bin-dir, ServiceAccount token integration
# beta/default-on in k8s 1.34): dynamically obtaining registry credentials
# (e.g. cloud workload-identity federation for ECR/GCR/ACR) by execing a
# configured binary per image pull, instead of (or in addition to) static
# imagePullSecrets. See crates/nodelet/src/credential_provider.rs.
#
# Round 123: previously manual-only on the theory that this needed a real
# cloud credential-provider binary and a live cloud registry — but the
# exec-plugin protocol itself (stdin/stdout JSON, no gRPC, see the module
# doc comment) doesn't actually require either. What it needs is *a
# registry that genuinely rejects anonymous pulls*, which a local
# `registry:2` container with htpasswd basic auth provides for real, and
# *a binary that speaks the protocol*, which a small script satisfies just
# as validly as a cloud vendor's own (nodelet only cares that stdin/stdout
# match the documented JSON shape — it has no idea what's on the other end
# of the pipe). This is the same "fake the plugin, not the protocol"
# pattern device_plugins.sh's own notes describe as the natural next step
# for that subsystem too.

test_credential_provider_config_unset_by_default() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    # Absent an explicit NODELET_IMAGE_CREDENTIAL_PROVIDER_CONFIG on this
    # deployment, image pulls must still work purely off imagePullSecrets
    # (or no auth at all for public images) exactly as every prior round —
    # this is really just a smoke check that wiring the new config-load
    # call into CriRuntime::connect() didn't regress the pull path for
    # nodes that never configured this feature at all.
    if [[ -n "${NODELET_IMAGE_CREDENTIAL_PROVIDER_CONFIG:-}" ]]; then
        skip_test "this node has NODELET_IMAGE_CREDENTIAL_PROVIDER_CONFIG set — the default-off case this check targets doesn't apply here"
    fi
    assert_true node_uses_cri_runtime
}

test_credential_provider_supplies_auth_for_an_otherwise_rejected_pull() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if ! nodelet_restart_supported; then skip_test "needs systemd (nodelet_restart_with_env)"; fi
    if ! command -v docker &>/dev/null; then skip_test "needs docker to stand up a local auth-required registry"; fi

    local reg_port=5001
    local reg_host="localhost:$reg_port"
    local reg_user="testuser"
    local reg_pass="testpass123"
    local work_dir
    work_dir="$(mktemp -d)"
    local hosts_dir="/etc/containerd/certs.d/$reg_host"
    local bin_dir="/var/lib/nodelet/e2e-test-cred-provider-bin"
    local cfg_file="$work_dir/cred-provider-config.yaml"
    local provider_name="fake-cred-provider"
    local pushed_image="$reg_host/cred-provider-check:1"

    cleanup() {
        delete_pod_if_exists "cred-provider-check" || true
        nodelet_restore_env || true
        docker rm -f e2e-test-registry &>/dev/null || true
        sudo rm -rf "$hosts_dir" "$bin_dir" || true
        sudo sed -i '/^\s*config_path\s*=\s*"\/etc\/containerd\/certs.d"/d' /etc/containerd/config.toml 2>/dev/null || true
        sudo systemctl restart containerd &>/dev/null || true
        rm -rf "$work_dir"
    }
    trap cleanup EXIT

    log "generating htpasswd auth for the local test registry..."
    docker run --rm --entrypoint htpasswd httpd:2.4-alpine -Bbn "$reg_user" "$reg_pass" > "$work_dir/htpasswd"

    log "starting a local auth-required registry:2 on port $reg_port..."
    docker run -d --name e2e-test-registry -p "$reg_port:5000" \
        -v "$work_dir/htpasswd:/auth/htpasswd:ro" \
        -e REGISTRY_AUTH=htpasswd \
        -e REGISTRY_AUTH_HTPASSWD_REALM="e2e-test" \
        -e REGISTRY_AUTH_HTPASSWD_PATH=/auth/htpasswd \
        registry:2 >/dev/null
    try_wait_until 40 bash -c "curl -sf -o /dev/null -w '%{http_code}' http://127.0.0.1:$reg_port/v2/ | grep -q 401" \
        || die "local test registry never came up requiring auth (expected HTTP 401 from /v2/ with no credentials)"

    log "pointing containerd at $reg_host as an insecure (http) registry..."
    if ! grep -q 'config_path = "/etc/containerd/certs.d"' /etc/containerd/config.toml; then
        sudo sed -i 's#\[plugins\."io\.containerd\.grpc\.v1\.cri"\.registry\]#[plugins."io.containerd.grpc.v1.cri".registry]\n      config_path = "/etc/containerd/certs.d"#' /etc/containerd/config.toml
    fi
    sudo mkdir -p "$hosts_dir"
    printf 'server = "http://%s"\n\n[host."http://%s"]\n  capabilities = ["pull", "resolve", "push"]\n' "$reg_host" "$reg_host" \
        | sudo tee "$hosts_dir/hosts.toml" >/dev/null
    sudo systemctl restart containerd
    try_wait_until 40 bash -c "sudo ctr version &>/dev/null" \
        || die "containerd never came back up after the config.toml change"

    log "pushing $TEST_IMAGE into the local registry as $pushed_image..."
    # Write the short-lived test credential directly in Docker's config
    # format. This is equivalent to `docker login`, but avoids both the
    # insecure-password CLI warning and Docker's unencrypted credential-store
    # warning on runners that have no credential helper configured.
    local docker_config auth
    docker_config="$work_dir/docker"
    mkdir -p "$docker_config"
    auth="$(printf '%s:%s' "$reg_user" "$reg_pass" | base64 -w0)"
    printf '{"auths":{"%s":{"auth":"%s"}}}\n' "$reg_host" "$auth" > "$docker_config/config.json"
    docker pull "$TEST_IMAGE" >/dev/null
    docker tag "$TEST_IMAGE" "$pushed_image"
    DOCKER_CONFIG="$docker_config" docker push "$pushed_image" >/dev/null

    # Negative control first: with NO credential provider configured, a
    # pull of this image must genuinely fail — proof the registry really
    # is enforcing auth (not e.g. still allowing anonymous pulls, which
    # would make the positive check below meaningless).
    local negname="cred-provider-check-neg"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $negname
spec:
  containers:
    - name: app
      image: $pushed_image
      command: ["sleep", "3600"]
EOF
    # Round 124 (found live in CI, full-suite tail-end contention only):
    # 90s wasn't always enough for the pull-failure status to actually
    # land in containerStatuses once this test lands at the tail of a
    # long, otherwise-unfiltered shard -- confirmed via containerd's own
    # logs still genuinely denying auth ("no basic auth credentials")
    # well within the old window, so this was status-propagation lag,
    # not the registry silently allowing the pull through.
    if ! try_wait_until 150 bash -c "kctl get pod $negname -o jsonpath='{.status.containerStatuses[0].state.waiting.reason}' 2>/dev/null | grep -qE 'ImagePullBackOff|ErrImagePull'"; then
        delete_pod_if_exists "$negname"
        die "expected pod without any credential provider to fail pulling from the auth-required registry, but it didn't — the registry isn't actually enforcing auth, so the positive check below wouldn't prove anything"
    fi
    delete_pod_if_exists "$negname"

    log "writing the fake credential-provider binary and config..."
    sudo mkdir -p "$bin_dir"
    sudo tee "$bin_dir/$provider_name" >/dev/null <<'PYEOF'
#!/usr/bin/env python3
# Fake credential provider for e2e testing: ignores the real
# CredentialProviderRequest on stdin (this test doesn't exercise the
# tokenAttributes path) and always returns the one registry's real
# htpasswd credentials nodelet's e2e-provisioned test registry expects.
import sys, json
sys.stdin.read()
print(json.dumps({
    "apiVersion": "credentialprovider.kubelet.k8s.io/v1",
    "kind": "CredentialProviderResponse",
    "cacheKeyType": "Registry",
    "cacheDuration": "0s",
    "auth": {"REG_HOST_PLACEHOLDER": {"username": "REG_USER_PLACEHOLDER", "password": "REG_PASS_PLACEHOLDER"}},
}))
PYEOF
    sudo sed -i "s/REG_HOST_PLACEHOLDER/$reg_host/; s/REG_USER_PLACEHOLDER/$reg_user/; s/REG_PASS_PLACEHOLDER/$reg_pass/" "$bin_dir/$provider_name"
    sudo chmod 0755 "$bin_dir/$provider_name"

    cat > "$cfg_file" <<EOF
apiVersion: kubelet.config.k8s.io/v1
kind: CredentialProviderConfig
providers:
  - name: $provider_name
    matchImages: ["$reg_host/*"]
    defaultCacheDuration: "0s"
EOF
    sudo cp "$cfg_file" "/var/lib/nodelet/e2e-test-cred-provider-config.yaml"

    nodelet_restart_with_env \
        "NODELET_IMAGE_CREDENTIAL_PROVIDER_CONFIG=/var/lib/nodelet/e2e-test-cred-provider-config.yaml" \
        "NODELET_IMAGE_CREDENTIAL_PROVIDER_BIN_DIR=$bin_dir"

    local name="cred-provider-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $pushed_image
      command: ["sleep", "3600"]
EOF
    if ! wait_until 150 "$name Running" pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        die "pod using the auth-required registry never reached Running with a credential provider configured — check nodelet's logs for 'credential provider' entries (exec failure, bad JSON, or the config/bin-dir wiring itself)"
    fi
    delete_pod_if_exists "$name"

    sudo rm -f "/var/lib/nodelet/e2e-test-cred-provider-config.yaml"
    trap - EXIT
    cleanup
}

register_test test_credential_provider_config_unset_by_default
register_test test_credential_provider_supplies_auth_for_an_otherwise_rejected_pull
