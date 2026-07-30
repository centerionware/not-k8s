# lib/test/k8s.sh — kubectl wrappers and poll/wait helpers for test-e2e.sh.
# Expects TEST_NAMESPACE set. Every helper here operates in that namespace
# unless told otherwise, so test cases never have to repeat --namespace.

kctl() { # kctl <kubectl args...> — namespace-scoped kubectl
    kubectl --namespace "$TEST_NAMESPACE" "$@"
}

apply_manifest() { # apply_manifest <<< "$yaml"
    kctl apply -f - >/dev/null
}

delete_pod_if_exists() { # delete_pod_if_exists <name>
    kctl delete pod "$1" --ignore-not-found --wait=false >/dev/null 2>&1 || true
}

pod_json() { # pod_json <name> — prints the Pod's full JSON, or "" if gone
    kctl get pod "$1" -o json 2>/dev/null || true
}

pod_field() { # pod_field <name> <jsonpath, e.g. '{.status.phase}'>
    kctl get pod "$1" -o jsonpath="$2" 2>/dev/null || true
}

# wait_until <timeout-seconds> <description> <command...> — polls every 2s
# until <command...> exits 0, or dies with <description> after the timeout.
# The command is re-evaluated fresh each poll (it's exec'd, not cached), so
# pass e.g. `wait_until 60 "pod Running" pod_is_phase myapp Running`.
wait_until() {
    local timeout="$1" description="$2"
    shift 2
    local waited=0
    while ! "$@" >/dev/null 2>&1; do
        if [[ "$waited" -ge "$timeout" ]]; then
            die "timed out after ${timeout}s waiting for: $description"
        fi
        sleep 2
        waited=$((waited + 2))
    done
}

pod_is_phase() { # pod_is_phase <name> <phase>
    [[ "$(pod_field "$1" '{.status.phase}')" == "$2" ]]
}

pod_condition_status() { # pod_condition_status <name> <type> -> True/False/""
    kctl get pod "$1" -o jsonpath="{.status.conditions[?(@.type==\"$2\")].status}" 2>/dev/null || true
}

pod_container_ready() { # pod_container_ready <pod> <container>
    [[ "$(kctl get pod "$1" -o jsonpath="{.status.containerStatuses[?(@.name==\"$2\")].ready}" 2>/dev/null)" == "true" ]]
}

pod_container_restart_count() { # pod_container_restart_count <pod> <container>
    kctl get pod "$1" -o jsonpath="{.status.containerStatuses[?(@.name==\"$2\")].restartCount}" 2>/dev/null || echo 0
}

pod_exists() { # pod_exists <name>
    kctl get pod "$1" >/dev/null 2>&1
}

pod_gone() { # pod_gone <name> — true once the pod no longer exists
    ! pod_exists "$1"
}

node_name() {
    kubectl get nodes -o jsonpath='{.items[0].metadata.name}'
}

node_condition_status() { # node_condition_status <type>
    kubectl get node "$(node_name)" -o jsonpath="{.status.conditions[?(@.type==\"$1\")].status}"
}

node_uses_cri_runtime() {
    local version
    version="$(kubectl get node "$(node_name)" -o jsonpath='{.status.nodeInfo.containerRuntimeVersion}' 2>/dev/null || true)"
    [[ "$version" == cri://* ]]
}

# run_in_container <pod> <container> <shell-command...> — the only way this
# suite can run a command inside a live container: through the CRI-backed
# probe/hook machinery isn't reachable from outside, and `kubectl exec`
# needs the streaming server nodelet doesn't implement yet (see
# lib/test/cases/unimplemented.sh). Callers that need real in-container
# execution should instead assert through the same channels kubelet itself
# would — an httpGet probe hitting a small server in the pod, or a shared
# emptyDir file both the test and an init/main container can read/write —
# not `kubectl exec`.
