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
