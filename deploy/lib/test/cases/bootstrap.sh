# lib/test/cases/bootstrap.sh — NODELET_BOOTSTRAP_KUBECONFIG (round 96):
# the CSR-based initial-client-cert-issuance flow real kubelet runs when
# started with --bootstrap-kubeconfig. Nodelet generates a keypair,
# submits a certificates.k8s.io/v1 CertificateSigningRequest (signerName
# kubernetes.io/kube-apiserver-client-kubelet, CN=system:node:<node>,
# O=system:nodes), waits for the apiserver's own node-authorizer/
# csrapproving controller to approve+sign it, then writes a real
# client-cert kubeconfig to NODELET_KUBECONFIG before the normal client
# is built. Nodelet never self-approves.
#
# The pure decision logic (node_identity_dn(), build_csr_object(),
# csr_outcome(), build_output_kubeconfig()) has full unit-test coverage
# in bootstrap.rs's own tests_pure_functions module.
#
# Round 123: the real flow WAS automatable after all — the original
# "manual only" call was written assuming this suite runs against an
# already-provisioned, shared cluster where you can't touch nodelet's own
# startup. That's not this suite's actual situation: it bootstraps a
# fresh, fully self-owned VM/cluster itself (deploy/bootstrap-source.sh,
# real root), so it genuinely can spin up a second, throwaway nodelet
# process with its own env vars without disturbing the real
# systemd-managed one at all. Runs the throwaway process with the mock
# runtime (this flow is apiserver-only — no CRI/containerd involvement),
# under a distinct node name so it can't collide with the real node.

test_tls_bootstrap_issues_a_real_client_certificate() {
    if ! command -v kubectl >/dev/null 2>&1; then skip_test "needs kubectl"; fi
    local nodelet_bin
    nodelet_bin="$(test_component_binary nodelet || true)"
    [[ -x "$nodelet_bin" ]] || skip_test "no nodelet binary found in the active nodelet.service or checkout build paths"

    # Deliberately NOT `local` below, despite being set inside this
    # function: they're read from bootstrap_test_cleanup(), which runs as
    # an EXIT trap. Confirmed live (round 123) that a trap can fire after
    # this function's own local scope is already gone — e.g. when the
    # subshell run_test() wraps each test in unwinds normally — at which
    # point a `local`-scoped variable referenced from the trap is
    # genuinely unbound under `set -u`, not just empty. That crashed
    # bootstrap_test_cleanup() at its very first line (an unbound `[[ -n
    # "$nodelet_pid" ]]` aborts the whole trap immediately, `|| true` and
    # all — the error happens evaluating the test, before either branch
    # runs), which skipped every subsequent cleanup step, leaking the
    # throwaway nodelet process and its RBAC/Node/CSR objects into the
    # cluster for the rest of the suite. Plain (subshell-global, not
    # `local`) assignment sidesteps the whole scoping hazard: these
    # variables live for the lifetime of the subshell run_test() already
    # isolates this test in, so nothing leaks across tests either way.
    sa="tls-bootstrap-test-sa"
    role="tls-bootstrap-test-role"
    binding="tls-bootstrap-test-binding"
    node_name="tls-bootstrap-test-node"
    scratch="$(mktemp -d)"
    nodelet_pid=""
    csr_name=""
    local bootstrap_kubeconfig="$scratch/bootstrap-kubeconfig"
    local output_kubeconfig="$scratch/output-kubeconfig"
    local log_file="$scratch/nodelet.log"

    bootstrap_test_cleanup() {
        # ${var:-} everywhere too, belt-and-suspenders on top of the
        # plain-assignment fix above: a genuinely unbound reference here
        # must never abort the trap partway through and skip the rest of
        # cleanup.
        [[ -n "${nodelet_pid:-}" ]] && kill "$nodelet_pid" 2>/dev/null; true
        [[ -n "${csr_name:-}" ]] && kubectl delete csr "$csr_name" --ignore-not-found >/dev/null 2>&1; true
        kubectl delete node "${node_name:-}" --ignore-not-found >/dev/null 2>&1 || true
        kubectl delete clusterrolebinding "${binding:-}" --ignore-not-found >/dev/null 2>&1 || true
        kubectl delete clusterrole "${role:-}" --ignore-not-found >/dev/null 2>&1 || true
        kctl delete serviceaccount "${sa:-}" --ignore-not-found >/dev/null 2>&1 || true
        [[ -n "${scratch:-}" ]] && rm -rf "$scratch"; true
    }
    trap bootstrap_test_cleanup EXIT

    # A low-privilege identity that can only create/read
    # CertificateSigningRequest objects — the standard kubelet TLS
    # bootstrapping RBAC shape (real upstream's own documented
    # ClusterRoleBinding for this, scoped to a throwaway ServiceAccount
    # here instead of the usual bootstrap-token approach, since a
    # ServiceAccount token is simpler to mint from inside a test).
    apply_manifest <<EOF
apiVersion: v1
kind: ServiceAccount
metadata:
  name: $sa
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: $role
rules:
  - apiGroups: ["certificates.k8s.io"]
    resources: ["certificatesigningrequests"]
    verbs: ["create", "get", "list", "watch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: $binding
subjects:
  - kind: ServiceAccount
    name: $sa
    namespace: $TEST_NAMESPACE
roleRef:
  kind: ClusterRole
  name: $role
  apiGroup: rbac.authorization.k8s.io
EOF

    local token server ca_data
    token="$(kctl create token "$sa" --duration=10m)"
    assert_not_empty "$token" "minted ServiceAccount token"
    server="$(kubectl config view --minify --raw -o jsonpath='{.clusters[0].cluster.server}')"
    ca_data="$(kubectl config view --minify --raw -o jsonpath='{.clusters[0].cluster.certificate-authority-data}')"
    assert_not_empty "$server" "cluster server URL"
    assert_not_empty "$ca_data" "cluster CA data"

    cat > "$bootstrap_kubeconfig" <<EOF
apiVersion: v1
kind: Config
clusters:
  - name: bootstrap
    cluster:
      server: $server
      certificate-authority-data: $ca_data
users:
  - name: bootstrap
    user:
      token: $token
contexts:
  - name: bootstrap
    context:
      cluster: bootstrap
      user: bootstrap
current-context: bootstrap
EOF

    # A second, throwaway nodelet process — distinct node name, mock
    # runtime, its own bootstrap/output kubeconfig paths — entirely
    # independent of the real systemd-managed one.
    env -u KUBECONFIG \
        NODELET_BOOTSTRAP_KUBECONFIG="$bootstrap_kubeconfig" \
        NODELET_KUBECONFIG="$output_kubeconfig" \
        NODELET_NODE_NAME="$node_name" \
        NODELET_RUNTIME=mock \
        RUST_LOG=info \
        "$nodelet_bin" > "$log_file" 2>&1 &
    nodelet_pid=$!

    try_wait_until 40 bash -c "kubectl get csr -o name 2>/dev/null | grep -q 'nodelet-$node_name-'" \
        || die "no CertificateSigningRequest named nodelet-$node_name-* appeared within 20s — log: $(cat "$log_file")"
    csr_name="$(kubectl get csr -o name | grep "nodelet-$node_name-" | head -1 | sed 's#certificatesigningrequest.certificates.k8s.io/##')"
    assert_not_empty "$csr_name" "CSR object name"

    local signer requested_cn
    signer="$(kubectl get csr "$csr_name" -o jsonpath='{.spec.signerName}')"
    assert_eq "$signer" "kubernetes.io/kube-apiserver-client-kubelet" "CSR signerName"
    requested_cn="$(kubectl get csr "$csr_name" -o jsonpath='{.spec.request}' | base64 -d | openssl req -noout -subject 2>/dev/null || true)"
    assert_contains "$requested_cn" "system:node:$node_name" "CSR's requested Subject CN"

    # No auto-approver is configured on this cluster for this signer, so
    # approve it directly — matching the manual-note's own documented
    # step for exactly this case.
    kubectl certificate approve "$csr_name" >/dev/null

    try_wait_until 40 bash -c "[[ -f '$output_kubeconfig' ]] && grep -q 'client-certificate-data' '$output_kubeconfig'" \
        || die "nodelet never wrote $output_kubeconfig with a client certificate after CSR approval — log: $(cat "$log_file")"
    assert_contains "$(cat "$log_file")" "issued client certificate" \
        "nodelet's own log should confirm it wrote the issued certificate"

    kill "$nodelet_pid" 2>/dev/null || true
    wait "$nodelet_pid" 2>/dev/null || true
    nodelet_pid=""
}

register_test test_tls_bootstrap_issues_a_real_client_certificate
