# lib/test/manifests.sh — shared building blocks for the YAML manifests
# each lib/test/cases/*.sh file writes inline (via `apply_manifest <<EOF`).
# Kept small on purpose: most manifests are bespoke enough per-test that a
# heredoc next to its assertions is more readable than a wall of generator
# functions with a dozen near-identical parameters.
#
# TEST_IMAGE defaults to Alpine — small, fast to pull, and has a real
# /bin/sh (busybox's doesn't support all the shell features some checks
# below use) plus coreutils, which is what most checks need (`id`, `cat`,
# `stat`). Override with TEST_IMAGE=... if your node can't reach docker.io.

TEST_IMAGE="${TEST_IMAGE:-alpine:3.20}"

# The single biggest unlock for testing without `kubectl exec`/`kubectl
# logs` (neither works yet — nodelet has no streaming server; see
# lib/test/cases/unimplemented.sh): this is a single-node architecture, so
# this test script runs on the exact same host as nodelet. A container's
# self-check output written into an emptyDir volume is readable directly
# off the node's filesystem at the path nodelet itself materializes it to
# (VOLUME_ROOT in crates/nodelet/src/runtime/cri.rs) — no exec needed.
NODELET_VOLUME_ROOT="${NODELET_VOLUME_ROOT:-/var/lib/nodelet/pods}"

pod_volume_host_path() { # pod_volume_host_path <pod> <volume-name>
    local uid
    uid="$(pod_field "$1" '{.metadata.uid}')"
    [[ -n "$uid" ]] || die "pod_volume_host_path: pod $1 has no UID yet (does it exist?)"
    echo "$NODELET_VOLUME_ROOT/$uid/volumes/$2"
}

# wait_for_check_file <pod> <volume> <filename> <timeout-seconds>
# Polls for a file a container wrote into a shared emptyDir to appear on
# the host, then prints its contents. This both waits for the container to
# have actually run its check *and* retrieves the result in one step.
wait_for_check_file() {
    local pod="$1" volume="$2" file="$3" timeout="${4:-60}"
    local path
    local waited=0
    while true; do
        path="$(pod_volume_host_path "$pod" "$volume")/$file"
        [[ -s "$path" ]] && { cat "$path"; return 0; }
        if [[ "$waited" -ge "$timeout" ]]; then
            die "timed out after ${timeout}s waiting for check file $file (volume $volume) from pod $pod"
        fi
        sleep 2
        waited=$((waited + 2))
    done
}

# Same reasoning as k8s.sh's own export -f block: several call sites use
# `wait_for_check_file`/`pod_volume_host_path` from inside a `bash -c
# "..."` (a separate bash process), which needs its own copy of the
# function and every global it touches (NODELET_VOLUME_ROOT here;
# TEST_NAMESPACE transitively, via pod_field -> kctl).
export -f pod_volume_host_path wait_for_check_file
export NODELET_VOLUME_ROOT
