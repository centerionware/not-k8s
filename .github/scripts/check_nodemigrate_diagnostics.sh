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
    api-route)
        case "$*" in
            *"get service kubernetes"*) printf '10.43.0.1\n' ;;
            *"get pod nodemigrate-api-route-"*) printf 'Succeeded\n' ;;
            *"apply -f -"*) cat > "${NODEMIGRATE_TEST_APPLY_CAPTURE:?}" ;;
        esac
        ;;
    cilium-restart)
        case "$*" in
            *"get pods -n kube-system -l k8s-app=cilium -o json"*)
                if [[ -f "${NODEMIGRATE_CILIUM_DELETE_MARKER:?}" ]]; then
                    printf '%s\n' '{"items":[{"metadata":{"name":"cilium-new","uid":"new"},"status":{"phase":"Running","conditions":[{"type":"Ready","status":"True"}]}}]}'
                else
                    printf '%s\n' '{"items":[{"metadata":{"name":"cilium-old","uid":"old"},"status":{"phase":"Running","conditions":[{"type":"Ready","status":"True"}]}}]}'
                fi
                ;;
            *"delete pod cilium-old"*) touch "${NODEMIGRATE_CILIUM_DELETE_MARKER:?}" ;;
        esac
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

cat > "$TEST_DIR/cmp" <<'STUB'
#!/usr/bin/env bash
if [[ "${1:-}" == -s ]]; then shift; fi
python3 - "$1" "$2" <<'PY'
import pathlib
import sys
sys.exit(0 if pathlib.Path(sys.argv[1]).read_bytes() == pathlib.Path(sys.argv[2]).read_bytes() else 1)
PY
STUB
chmod +x "$TEST_DIR/cmp"

cat > "$TEST_DIR/crictl" <<'STUB'
#!/usr/bin/env bash
case "$*" in
    *"pods -o json"*)
        if [[ -f "${NODEMIGRATE_CRI_REMOVED_MARKER:?}" ]]; then
            printf '%s\n' '{"items":[]}'
        else
            printf '%s\n' '{"items":[{"id":"cilium","state":"SANDBOX_READY","metadata":{"name":"cilium-agent"}},{"id":"ordinary","state":"SANDBOX_READY","metadata":{"name":"application"}},{"id":"cilium-not-ready","state":"SANDBOX_NOTREADY","labels":{"k8s-app":"cilium-envoy"}}]}'
        fi
        ;;
    *"ps -a -o json"*)
        printf '%s\n' '{"containers":[]}'
        ;;
    *"stopp ordinary"*)
        echo stopp-ordinary >> "${NODEMIGRATE_CRI_CALLS:?}"
        ;;
    *"stopp cilium"*)
        echo stopp-cilium >> "${NODEMIGRATE_CRI_CALLS:?}"
        ;;
    *"stopp cilium-not-ready"*)
        echo stopp-not-ready >> "${NODEMIGRATE_CRI_CALLS:?}"
        ;;
    *"rmp ordinary"*)
        echo rmp-ordinary >> "${NODEMIGRATE_CRI_CALLS:?}"
        ;;
    *"rmp cilium-not-ready"*)
        echo rmp-cilium-not-ready >> "${NODEMIGRATE_CRI_CALLS:?}"
        touch "${NODEMIGRATE_CRI_REMOVED_MARKER:?}"
        ;;
    *"rmp cilium"*)
        echo rmp-cilium >> "${NODEMIGRATE_CRI_CALLS:?}"
        ;;
    *)
        echo "unexpected crictl invocation: $*" >&2
        exit 2
        ;;
esac
STUB
chmod +x "$TEST_DIR/crictl"

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

CHECKPOINT_DIR="$TEST_DIR/checkpoints"
for stage in source replaced; do
    mkdir -p "$CHECKPOINT_DIR/$stage"
    printf 'secrets\n' > "$CHECKPOINT_DIR/$stage/api-resources.txt"
    : > "$CHECKPOINT_DIR/$stage/required-crds.json"
    : > "$CHECKPOINT_DIR/$stage/application.json"
    : > "$CHECKPOINT_DIR/$stage/certificate.json"
    : > "$CHECKPOINT_DIR/$stage/issuer.json"
    : > "$CHECKPOINT_DIR/$stage/storageclass.json"
    : > "$CHECKPOINT_DIR/$stage/certificate-secret.sha256"
    : > "$CHECKPOINT_DIR/$stage/user-configmap-data.sha256"
    : > "$CHECKPOINT_DIR/$stage/user-binary-configmap-data.sha256"
    : > "$CHECKPOINT_DIR/$stage/user-immutable-configmap.sha256"
    : > "$CHECKPOINT_DIR/$stage/user-secret-data.sha256"
    : > "$CHECKPOINT_DIR/$stage/helm-releases.jsonl"
done
jq -cn --arg hash before '{identity:{apiGroup:"",kind:"Secret",name:"node1.node-password.k3s",namespace:"kube-system"},sha256:$hash,fields:{"/data/hash":$hash}}' > "$CHECKPOINT_DIR/source/migratable-objects.jsonl"
jq -cn '{identity:{apiGroup:"",kind:"ConfigMap",name:"user-config",namespace:"default"},sha256:"same",fields:{"/data/value":"same"}}' >> "$CHECKPOINT_DIR/source/migratable-objects.jsonl"
jq -cn --arg hash after '{identity:{apiGroup:"",kind:"Secret",name:"node1.node-password.k3s",namespace:"kube-system"},sha256:$hash,fields:{"/data/hash":$hash}}' > "$CHECKPOINT_DIR/replaced/migratable-objects.jsonl"
jq -cn '{identity:{apiGroup:"",kind:"ConfigMap",name:"user-config",namespace:"default"},sha256:"same",fields:{"/data/value":"same"}}' >> "$CHECKPOINT_DIR/replaced/migratable-objects.jsonl"
output="$(assert_migratable_api_objects_retained source replaced node1 2>&1)" || {
    echo "K3s Node replacement credential rotation was rejected: $output" >&2
    exit 1
}
grep -Fq 'Expected K3s node-password hash rotation' <<< "$output" || {
    echo "expected generated credential rotation was not reported: $output" >&2
    exit 1
}

jq -cn '{identity:{apiGroup:"",kind:"Secret",name:"node1.node-password.k3s",namespace:"kube-system"},sha256:"after",fields:{"/data/hash":"after","/unexpected":"changed"}}' > "$CHECKPOINT_DIR/replaced/migratable-objects.jsonl"
jq -cn '{identity:{apiGroup:"",kind:"ConfigMap",name:"user-config",namespace:"default"},sha256:"same",fields:{"/data/value":"same"}}' >> "$CHECKPOINT_DIR/replaced/migratable-objects.jsonl"
if assert_migratable_api_objects_retained source replaced node1 >"$TEST_DIR/unexpected-secret-change.out" 2>&1; then
    echo "replacement parity accepted an unexpected node-password Secret field change: $(cat "$TEST_DIR/unexpected-secret-change.out")" >&2
    exit 1
fi
grep -Fq 'Unexpected changed paths in K3s node-password Secret' "$TEST_DIR/unexpected-secret-change.out" || {
    echo "unexpected generated credential mutation did not fail clearly" >&2
    cat "$TEST_DIR/unexpected-secret-change.out" >&2
    exit 1
}

jq -cn '{identity:{apiGroup:"",kind:"Secret",name:"node1.node-password.k3s",namespace:"kube-system"},sha256:"after",fields:{"/data/hash":"after"}}' > "$CHECKPOINT_DIR/replaced/migratable-objects.jsonl"
jq -cn '{identity:{apiGroup:"",kind:"ConfigMap",name:"user-config",namespace:"default"},sha256:"changed",fields:{"/data/value":"changed"}}' >> "$CHECKPOINT_DIR/replaced/migratable-objects.jsonl"
if assert_migratable_api_objects_retained source replaced node1 >"$TEST_DIR/user-object-change.out" 2>&1; then
    echo "replacement parity accepted changed user fixture data" >&2
    exit 1
fi
grep -Fq 'Normalized source API object data changed' "$TEST_DIR/user-object-change.out" || {
    echo "changed user fixture data did not fail parity clearly" >&2
    cat "$TEST_DIR/user-object-change.out" >&2
    exit 1
}

export KUBECTL_FIXTURE=api-route
export NODEMIGRATE_TEST_APPLY_CAPTURE="$TEST_DIR/api-route-pod.yaml"
output="$(probe_api_clusterip_from_pod /tmp/test-kubeconfig source 2>&1)" || {
    echo "successful Pod-to-API-Service probe fixture failed: $output" >&2
    exit 1
}
grep -Fq 'PASS Pod-to-Service API TCP probe at stage=source clusterIP=10.43.0.1' <<< "$output" || {
    echo "successful Pod-to-API-Service probe was not reported: $output" >&2
    exit 1
}
grep -Fq 'nc -z -w 5 10.43.0.1 443' "$NODEMIGRATE_TEST_APPLY_CAPTURE" || {
    echo "Pod probe does not test TCP access to the Kubernetes API ClusterIP" >&2
    cat "$NODEMIGRATE_TEST_APPLY_CAPTURE" >&2
    exit 1
}

export KUBECTL_FIXTURE=cilium-restart
export NODEMIGRATE_CILIUM_DELETE_MARKER="$TEST_DIR/cilium-agent-deleted"
output="$(restart_cilium_agent_pod /tmp/test-kubeconfig 2>&1)" || {
    echo "Cilium agent Pod restart fixture failed: $output" >&2
    exit 1
}
grep -Fq 'Replacement Cilium agent Pod cilium-new UID=new is Ready' <<< "$output" || {
    echo "Cilium agent replacement was not reported: $output" >&2
    exit 1
}

plan="$(source_sandbox_plan <<'JSON'
{"items":[
  {"id":"cilium-agent","state":"SANDBOX_READY","metadata":{"name":"cilium-agent"}},
  {"id":"workload","state":"SANDBOX_READY","metadata":{"name":"app"}},
  {"id":"cilium-not-ready","state":"SANDBOX_NOTREADY","labels":{"k8s-app":"cilium-envoy"}},
  {"state":"SANDBOX_READY","metadata":{"name":"ignored"}}
]}
JSON
)" || {
    echo "source CRI sandbox planner rejected its valid fixture" >&2
    exit 1
}
expected_plan=$'workload\ttrue\tfalse\ncilium-agent\ttrue\ttrue\ncilium-not-ready\tfalse\ttrue'
[[ "$plan" == "$expected_plan" ]] || {
    printf 'source CRI sandbox planner returned an unexpected order:\n%s\n' "$plan" >&2
    exit 1
}
if source_sandbox_plan <<'JSON' >"$TEST_DIR/empty-sandbox-plan.out" 2>&1
{"items":[]}
JSON
then
    echo "source CRI sandbox planner accepted an empty sandbox inventory" >&2
    exit 1
fi
grep -Fq 'CRI returned no pod sandboxes' "$TEST_DIR/empty-sandbox-plan.out" || {
    echo "empty source CRI sandbox inventory did not fail with a clear message" >&2
    cat "$TEST_DIR/empty-sandbox-plan.out" >&2
    exit 1
}

export NODEMIGRATE_CRI_CALLS="$TEST_DIR/crictl-calls.log"
export NODEMIGRATE_CRI_REMOVED_MARKER="$TEST_DIR/crictl-sandboxes-removed"
output="$(stop_source_sandboxes_for_probe 2>&1)" || {
    echo "full source sandbox handoff fixture failed: $output" >&2
    exit 1
}
grep -Fq 'PASS removed all 3 source CRI pod sandboxes' <<< "$output" || {
    echo "full source sandbox removal was not reported: $output" >&2
    exit 1
}
expected_calls=$'stopp-ordinary\nstopp-cilium\nrmp-ordinary\nrmp-cilium\nrmp-cilium-not-ready'
actual_calls="$(cat "$NODEMIGRATE_CRI_CALLS")"
[[ "$actual_calls" == "$expected_calls" ]] || {
    printf 'source sandbox handoff used an unexpected CRI order:\n%s\n' "$actual_calls" >&2
    exit 1
}

echo "PASS migration watcher and replacement parity diagnostics"
