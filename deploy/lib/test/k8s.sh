# lib/test/k8s.sh — kubectl wrappers and poll/wait helpers for test-e2e.sh.
# Expects TEST_NAMESPACE set. Every helper here operates in that namespace
# unless told otherwise, so test cases never have to repeat --namespace.

# Shadows the real `kubectl` binary for every caller in this suite
# (test cases routinely call bare `kubectl` directly for node-level
# queries, not just kctl's namespace-scoped wrapper). A bare `kubectl`
# call has NO client-side deadline by default — if the apiserver
# connection ever wedges (stalled TLS handshake, half-open TCP), a single
# call can hang indefinitely, completely outside every test's own bounded
# wait_until/try_wait_until loops (confirmed for real: round 123's batch-3
# run sat well past what even a full-timeout worst case across all 12
# tests could explain). --request-timeout closes that gap for the
# ordinary request/response verbs. Streaming subcommands
# (exec/attach/port-forward, and `logs -f`) are deliberately excluded —
# they're long-lived by design and this timeout would truncate them
# mid-stream. `command kubectl` reaches the real binary, not this
# function, avoiding infinite recursion.
kubectl() {
    # "$1" is reliably the real verb here.
    case "$1" in
        exec|attach|port-forward)
            command kubectl "$@" ;;
        logs)
            if [[ " $* " == *" -f "* ]]; then
                command kubectl "$@"
            else
                command kubectl --request-timeout=30s "$@"
            fi
            ;;
        *)
            command kubectl --request-timeout=30s "$@" ;;
    esac
}

kctl() { # kctl <kubectl args...> — namespace-scoped kubectl
    # Round 123 (found live in CI): appending "--namespace $TEST_NAMESPACE"
    # AFTER "$@" broke every caller using kubectl's own "--" remote-command
    # separator (kctl exec <pod> -- <cmd>, the exact shape streaming.sh and
    # csi_pvc.sh's fsGroup test both use) — anything after "--" is opaque
    # remote-command argv to kubectl exec, so --namespace silently landed
    # inside the CONTAINER's own command instead of being parsed as a
    # kubectl flag, and kubectl quietly fell back to the default namespace.
    # That's exactly what made a real, Running pod look like "not found" —
    # it was being looked for in the wrong namespace. Inserting it
    # immediately after the verb keeps it safely before any "--" the
    # caller's own remaining args might contain.
    local verb="$1"
    shift
    kubectl "$verb" --namespace "$TEST_NAMESPACE" "$@"
}

apply_manifest() { # apply_manifest <<< "$yaml"
    # Round 124 (found live in CI): this used to discard kubectl apply's
    # output and exit code entirely, so a real apiserver rejection (a
    # manifest failing API validation — confirmed live: procMount:
    # Unmasked without hostUsers: false gets a hard 422) was completely
    # invisible. The pod was never even created, and every caller's own
    # subsequent try_wait_until/wait_until for it reaching Running just
    # burned its whole budget on something that could never appear,
    # surfacing as a generic, misleading "pod never reached Running"
    # instead of the real, immediate, and much more useful rejection
    # reason. Still doesn't hard-fail the test itself here — some
    # callers legitimately want to decide for themselves how to react —
    # but the real error is now always visible in the log right where it
    # happened, not buried under an unrelated timeout message minutes
    # later.
    local output
    if ! output="$(kctl apply -f - 2>&1)"; then
        warn "apply_manifest: kubectl apply failed — likely to surface downstream as a misleading 'pod never reached Running' instead of this real error: $output"
        return 1
    fi
}

delete_pod_if_exists() { # delete_pod_if_exists <name>
    kctl delete pod "$1" --ignore-not-found --wait=false >/dev/null 2>&1 || true
}

# delete_pod_and_pvc <pod-name> <pvc-name> [pod-gone-timeout=90] — the CSI
# test cleanup pattern, done right. Round 124 (found live in CI, full-suite
# runs only): deleting the pod with --wait=false and then IMMEDIATELY
# deleting its PVC (the pattern nearly every CSI test used) races nodelet's
# own async teardown — unmount_csi_volumes() (volumes_resolve.rs) needs to
# re-fetch the PVC to resolve which driver/volume_handle to call
# NodeUnpublishVolume with, and that fetch is a documented, deliberate "real
# but narrow gap": if the PVC is already gone by the time it runs, nodelet
# logs "CSI teardown: failed to resolve PersistentVolumeClaim; volume left
# mounted" and gives up on that volume for good — no retry. Under real
# full-suite CI load (reconciliation queued behind other work), the
# --wait=false pod delete returning to bash and the immediately-following
# PVC delete both routinely beat nodelet to it, so this raced on nearly
# every single CSI test run and left a permanently-phantom "still mounted"
# entry in Node.status.volumesInUse — which is why a LATER, unrelated test
# (test_node_reports_volumes_in_use_for_a_csi_volume, which only asserts
# volumesInUse has no kubernetes.io/csi/ entries at all) kept timing out no
# matter how generous its own budget got: it was waiting on a stale entry
# from a completely different pod's botched teardown, not its own. Waiting
# for the pod to actually be gone (which is when unmount_csi_volumes() runs)
# before deleting the PVC closes the window this suite itself was creating.
delete_pod_and_pvc() {
    local pod="$1" pvc="$2" timeout="${3:-90}"
    delete_pod_if_exists "$pod"
    try_wait_until "$timeout" pod_gone "$pod" \
        || warn "delete_pod_and_pvc: $pod still not gone after ${timeout}s — deleting its PVC anyway, but nodelet's CSI teardown may lose the PVC->volume mapping race (see this function's own comment)"
    kctl delete pvc "$pvc" --ignore-not-found >/dev/null 2>&1 || true
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
    # Real elapsed wall-clock time, not "iterations * 2s" — see
    # try_wait_until's own comment (harness.sh) for the real CI failure
    # this fixes: the old accounting ignored how long "$@" itself took to
    # return, so a slow-but-eventually-returning command could blow a
    # nominal timeout budget many times over in real wall-clock time.
    local start_s=$SECONDS
    while ! "$@" >/dev/null 2>&1; do
        if [[ "$((SECONDS - start_s))" -ge "$timeout" ]]; then
            die "timed out after ${timeout}s waiting for: $description"
        fi
        sleep 2
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
    # Round 124: the `|| echo 0` fallback only fires if kubectl itself
    # fails -- a successful call with no matching containerStatuses entry
    # yet (container not created, or status not written yet) returns exit
    # 0 with EMPTY output, not an error, silently defeating the fallback
    # and handing callers an empty string instead of "0". Explicit check.
    local out
    out="$(kctl get pod "$1" -o jsonpath="{.status.containerStatuses[?(@.name==\"$2\")].restartCount}" 2>/dev/null)"
    [[ -n "$out" ]] && echo "$out" || echo 0
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

# A good few callers below build their own poll loop as `wait_until N desc
# bash -c "... kctl ..."` instead of using wait_until's own re-invoke — a
# real, separate `bash -c` command needs its own bash *process*, and shell
# functions are per-process; nothing here reaches that new process unless
# it's exported. Confirmed for real: without this, every one of those
# call sites' `kctl`/`pod_*` reference silently resolved to nothing
# ("command not found", swallowed by the caller's own `2>/dev/null`), so
# the condition being polled for could never become true and the test
# just ran out its full timeout and failed — regardless of whether the
# thing it was actually checking was fine. `export -f` fixes it for good:
# every helper here becomes callable from a `bash -c` (or any other child
# process) the same way it is from this file itself. TEST_NAMESPACE also
# has to be `export`ed by whatever sets it (test-e2e.sh does) — these
# functions are worthless in a child process without it.
export -f kubectl kctl apply_manifest delete_pod_if_exists delete_pod_and_pvc pod_json pod_field \
    pod_is_phase pod_condition_status pod_container_ready pod_container_restart_count \
    pod_exists pod_gone node_name node_condition_status node_uses_cri_runtime

# run_in_container <pod> <container> <shell-command...> — the only way this
# suite can run a command inside a live container: through the CRI-backed
# probe/hook machinery isn't reachable from outside, and `kubectl exec`
# needs the streaming server nodelet doesn't implement yet (see
# lib/test/cases/unimplemented.sh). Callers that need real in-container
# execution should instead assert through the same channels kubelet itself
# would — an httpGet probe hitting a small server in the pod, or a shared
# emptyDir file both the test and an init/main container can read/write —
# not `kubectl exec`.
