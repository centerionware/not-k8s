#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TEST_DIR="$(mktemp -d)"
trap 'rm -rf "$TEST_DIR"' EXIT

cat > "$TEST_DIR/kubectl" <<'STUB'
#!/usr/bin/env bash
case "${KUBECTL_FIXTURE:-}" in
    failure)
        echo 'Error from server (NotFound): services "cert-manager-webhook" not found' >&2
        exit 1
        ;;
    invalid)
        printf 'not-json\n'
        ;;
    success)
        echo 'warning: harmless kubectl warning' >&2
        printf '{"items":[{"metadata":{"name":"test"},"data":{"ca.crt":"Y2E="}}]}\n'
        ;;
    *)
        echo "unknown fixture" >&2
        exit 2
        ;;
esac
STUB
chmod +x "$TEST_DIR/kubectl"

cat > "$TEST_DIR/systemctl" <<'STUB'
#!/usr/bin/env bash
if [[ "${1:-}" == cat && "${2:-}" == k3s ]]; then
    printf '%s\n' \
        '--kube-apiserver-arg=audit-policy-file=/etc/rancher/k3s/nodemigrate-audit-policy.yaml' \
        "--kube-apiserver-arg=audit-log-path=${NODEMIGRATE_K3S_AUDIT_LOG:?}"
    exit 0
fi
echo "unexpected systemctl invocation: $*" >&2
exit 2
STUB
chmod +x "$TEST_DIR/systemctl"

export NODEMIGRATE_INTEGRATION_LIBRARY=true
export GITHUB_WORKSPACE="$ROOT"
export PATH="$TEST_DIR:/usr/bin:/bin"
# shellcheck source=nodemigrate-integration.sh
source "$ROOT/.github/scripts/nodemigrate-integration.sh"

audit_policy="$(sed -n '/^apiVersion: audit.k8s.io\/v1$/,/^EOF$/p' \
    "$ROOT/.github/scripts/nodemigrate-integration.sh")"
grep -Fq 'resources: [nodes, nodes/status]' <<< "$audit_policy" || {
    echo "K3s audit policy does not capture Node lifecycle writes" >&2
    exit 1
}
grep -Fq 'namespaces: [kube-node-lease]' <<< "$audit_policy" || {
    echo "K3s audit policy does not scope Lease auditing to kube-node-lease" >&2
    exit 1
}
grep -Fq 'resources: [leases]' <<< "$audit_policy" || {
    echo "K3s audit policy does not capture Node Lease writes" >&2
    exit 1
}

export NODEMIGRATE_K3S_AUDIT_LOG="$TEST_DIR/nodemigrate-audit.jsonl"
MIGRATION_STARTED_AT=2026-09-28T19:10:00+00:00
cat > "$NODEMIGRATE_K3S_AUDIT_LOG" <<'JSONL'
{"requestReceivedTimestamp":"2026-09-28T19:10:03+00:00","verb":"delete","objectRef":{"resource":"nodes","name":"worker-1"},"user":{"username":"migration-admin"},"responseStatus":{"code":200}}
{"requestReceivedTimestamp":"2026-09-28T19:10:04+00:00","verb":"update","objectRef":{"resource":"leases","namespace":"kube-node-lease","name":"worker-1"},"user":{"username":"system:node:worker-1"},"responseStatus":{"code":200}}
JSONL
output="$(verify_returned_k3s_audit 2>&1)" || {
    echo "valid returned K3s audit events were rejected: $output" >&2
    exit 1
}
grep -Fq 'PASS returned K3s audit captured 1 Node and 1 Lease mutation(s)' <<< "$output" || {
    echo "returned K3s audit event counts were not reported: $output" >&2
    exit 1
}

printf '%s\n' \
    '{"requestReceivedTimestamp":"2026-09-28T19:10:04+00:00","verb":"update","objectRef":{"resource":"leases","namespace":"kube-node-lease","name":"worker-1"}}' \
    > "$NODEMIGRATE_K3S_AUDIT_LOG"
if verify_returned_k3s_audit >"$TEST_DIR/missing-node-audit.out" 2>&1; then
    echo "returned K3s audit verification accepted a missing Node mutation" >&2
    exit 1
fi
grep -Fq 'no Node API mutations since return migration began' "$TEST_DIR/missing-node-audit.out" || {
    echo "missing returned Node audit event did not produce a clear failure" >&2
    cat "$TEST_DIR/missing-node-audit.out" >&2
    exit 1
}

export KUBECTL_FIXTURE=failure
output="$(diagnostic_kubectl_json /tmp/test-kubeconfig '{"name": .items[0].metadata.name}' \
    get service cert-manager-webhook -n cert-manager 2>&1)"
jq -e '.kubectlError | contains("NotFound")' <<< "$output" >/dev/null
if grep -qi 'parse error' <<< "$output"; then
    echo "failed kubectl request leaked a jq parse error" >&2
    exit 1
fi

export KUBECTL_FIXTURE=success
output="$(diagnostic_kubectl_json /tmp/test-kubeconfig '{"name": .items[0].metadata.name}' \
    get pods 2>&1)"
[[ "$output" == '{"name":"test"}' ]] || {
    echo "valid kubectl JSON was not filtered cleanly: $output" >&2
    exit 1
}

output="$(diagnostic_kubectl_json /tmp/test-kubeconfig \
    '{"name": .items[0].metadata.name, "ca": $ca}' --arg ca Y2E= get configmaps -A 2>&1)"
[[ "$output" == '{"ca":"Y2E=","name":"test"}' ]] || {
    echo "jq arguments were not passed through: $output" >&2
    exit 1
}

export KUBECTL_FIXTURE=invalid
output="$(diagnostic_kubectl_json /tmp/test-kubeconfig '{"name": .items[0].metadata.name}' \
    get pods 2>&1)"
jq -e '.diagnosticError | contains("invalid JSON")' <<< "$output" >/dev/null
if grep -qi 'parse error' <<< "$output"; then
    echo "malformed kubectl response leaked a jq parse error" >&2
    exit 1
fi

echo "PASS migration watcher JSON diagnostics"
