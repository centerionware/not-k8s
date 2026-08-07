# lib/test/cases/resources.sh — resource requests/limits actually reaching
# cgroups, not just being advertised in the Pod spec. Reads the container's
# own cgroup v2 files (cat'd into a shared emptyDir — see dns.sh for why
# this needs the container's own cooperation rather than a host-readable
# path). Assumes cgroup v2 (the modern containerd default); skips cleanly
# if that's not what this node uses rather than guessing at the v1 paths.
#
# oom_score_adj (round 28) doesn't need the cgroup-v2 caveat above — a
# container can always read its own /proc/self/oom_score_adj regardless
# of cgroup version, since it's a per-process kernel value, not a cgroup
# controller file.

test_memory_limit_is_enforced_via_cgroup() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="mem-limit"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      resources:
        limits:
          memory: "67108864"
      command: ["sh", "-c", "cat /sys/fs/cgroup/memory.max > /shared/memmax.txt 2>/shared/memmax.err; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local value
    if ! value="$(try_wait_until 30 bash -c "[[ -s \"\$(pod_volume_host_path '$name' shared)/memmax.txt\" ]]" && wait_for_check_file "$name" shared memmax.txt 5)"; then
        delete_pod_if_exists "$name"
        skip_test "no /sys/fs/cgroup/memory.max in the container — this node likely uses cgroup v1, not v2"
    fi
    assert_eq "$value" "67108864" "cgroup memory.max should match the Pod's memory limit exactly"
    delete_pod_if_exists "$name"
}

test_no_swap_default_disables_swap_via_cgroup() {
    # Round 68: memorySwap.swapBehavior was never wired up at all before
    # this. NODELET_MEMORY_SWAP_BEHAVIOR's default (NoSwap, matching
    # upstream) pins a container's combined memory+swap ceiling to
    # exactly its own memory limit — the OCI runtime derives cgroup v2's
    # memory.swap.max as (combined ceiling - memory.max), so a limited
    # container should read swap.max as exactly 0. This proves the
    # *default* behavior end to end; LimitedSwap itself needs restarting
    # nodelet with a different env var — see the manual-note test below.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="no-swap-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      resources:
        limits:
          memory: "67108864"
      command: ["sh", "-c", "cat /sys/fs/cgroup/memory.swap.max > /shared/swapmax.txt 2>/shared/swapmax.err; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local value
    if ! value="$(try_wait_until 30 bash -c "[[ -s \"\$(pod_volume_host_path '$name' shared)/swapmax.txt\" ]]" && wait_for_check_file "$name" shared swapmax.txt 5)"; then
        delete_pod_if_exists "$name"
        skip_test "no /sys/fs/cgroup/memory.swap.max in the container — this node either uses cgroup v1, or has the memory controller's swap accounting disabled at the kernel level (CONFIG_MEMCG_SWAP / swapaccount=1)"
    fi
    assert_eq "$value" "0" "with the default NoSwap behavior, a memory-limited container's cgroup memory.swap.max should read exactly 0 (no additional swap beyond its memory limit)"
    delete_pod_if_exists "$name"
}

test_limited_swap_gives_burstable_pods_proportional_swap() {
    # Round 123: previously manual-only for two reasons — nodelet needing
    # a real restart with NODELET_MEMORY_SWAP_BEHAVIOR=LimitedSwap
    # (nodelet_restart_with_env, nodelet_env.sh, fixes that), and needing
    # a node with swap actually enabled (LimitedSwap grants nothing on a
    # swapless node regardless of config) — this now creates a real
    # temporary swapfile via sudo if the node doesn't already have one,
    # rather than skip.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if ! nodelet_restart_supported; then skip_test "needs systemd to restart nodelet with NODELET_MEMORY_SWAP_BEHAVIOR=LimitedSwap"; fi

    local swap_total
    swap_total="$(grep -m1 '^SwapTotal:' /proc/meminfo | awk '{print $2}')"
    local created_swapfile=""
    if [[ -z "$swap_total" || "$swap_total" -eq 0 ]]; then
        created_swapfile="$(mktemp /tmp/nodelet-e2e-swapfile.XXXXXX)"
        sudo fallocate -l 512M "$created_swapfile" 2>/dev/null || sudo dd if=/dev/zero of="$created_swapfile" bs=1M count=512 status=none
        sudo chmod 600 "$created_swapfile"
        if ! sudo mkswap "$created_swapfile" >/dev/null 2>&1 || ! sudo swapon "$created_swapfile" 2>/dev/null; then
            sudo rm -f "$created_swapfile"
            skip_test "couldn't create/enable a real swapfile on this node (mkswap/swapon failed) — LimitedSwap needs real swap to exercise"
        fi
    fi

    limited_swap_test_cleanup() {
        nodelet_restore_env
        if [[ -n "${created_swapfile:-}" ]]; then
            sudo swapoff "$created_swapfile" 2>/dev/null || true
            sudo rm -f "$created_swapfile"
        fi
    }
    trap limited_swap_test_cleanup EXIT
    nodelet_restart_with_env "NODELET_MEMORY_SWAP_BEHAVIOR=LimitedSwap"

    # Round 123 (found live in CI): a request with no limit at all is
    # ALSO "Burstable-shaped" by the classification in
    # container_swap_limit_bytes()'s own doc comment, but its actual code
    # deliberately returns 0 (unconstrained/"max") for that specific
    # shape — there's no memory limit to combine a swap share with,
    # matching upstream's own KEP-2400 behavior for a limit-less pod.
    # This test needs a genuinely bounded Burstable container (a request
    # AND a higher limit) to exercise the real proportional-share path.
    local burstable="limited-swap-burstable"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $burstable
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      resources:
        requests: { memory: "64Mi" }
        limits: { memory: "256Mi" }
EOF
    wait_until 60 "$burstable Running" pod_is_phase "$burstable" Running
    local burstable_swap_max
    # Round 123 (found live in CI): kubectl's own "kuberc: ... permission
    # denied" warning lands on the same stream as the real value with no
    # separating newline, so a plain `2>&1` capture polluted a genuine
    # numeric result into e.g. "...deniedNNNNN" — tr -dc keeps only
    # digits, the same fix csi_pvc.sh's fsGroup test already needed.
    burstable_swap_max="$(kctl exec "$burstable" -- cat /sys/fs/cgroup/memory.swap.max 2>&1 | tr -dc '0-9')"
    delete_pod_if_exists "$burstable"
    assert_true bash -c "[[ '$burstable_swap_max' =~ ^[0-9]+$ && '$burstable_swap_max' -gt 0 ]]" \
        "a Burstable pod under LimitedSwap should get a nonzero memory.swap.max (KEP-2400's proportional formula) — got '$burstable_swap_max'"

    local guaranteed="limited-swap-guaranteed"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $guaranteed
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sleep", "3600"]
      resources:
        requests: { memory: "64Mi", cpu: "100m" }
        limits: { memory: "64Mi", cpu: "100m" }
EOF
    wait_until 60 "$guaranteed Running" pod_is_phase "$guaranteed" Running
    local guaranteed_swap_max
    guaranteed_swap_max="$(kctl exec "$guaranteed" -- cat /sys/fs/cgroup/memory.swap.max 2>&1 | tr -dc '0-9')"
    delete_pod_if_exists "$guaranteed"
    assert_eq "$guaranteed_swap_max" "0" "a Guaranteed pod should get memory.swap.max = 0 even under LimitedSwap — Guaranteed-shaped containers get no swap at all, same as BestEffort"
}

test_hugepages_limit_is_enforced_via_cgroup() {
    # Round 59: resources.limits["hugepages-2Mi"] was never translated to
    # CRI's LinuxContainerResources.hugepage_limits at all before this.
    # Real k8s validation requires a memory limit alongside any hugepages
    # limit, hence both here. Skips cleanly (not a hard fail) if this
    # node/kernel has no 2Mi hugepages reserved or the hugetlb cgroup
    # controller isn't enabled — genuinely outside nodelet's control.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="hugepages-limit"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      resources:
        limits:
          hugepages-2Mi: "4Mi"
          memory: "67108864"
      command: ["sh", "-c", "cat /sys/fs/cgroup/hugetlb.2MB.limit_in_bytes > /shared/hugetlb.txt 2>/shared/hugetlb.err; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        skip_test "pod never reached Running with a hugepages-2Mi limit set — this node/kernel likely has no 2Mi hugepages reserved (check /proc/sys/vm/nr_hugepages) or the runtime doesn't support the hugetlb cgroup controller"
    fi

    local path="$(pod_volume_host_path "$name" shared)/hugetlb.txt"
    local waited=0
    while [[ ! -s "$path" && "$waited" -lt 20 ]]; do
        sleep 2
        waited=$((waited + 2))
    done
    if [[ ! -s "$path" ]]; then
        delete_pod_if_exists "$name"
        skip_test "no /sys/fs/cgroup/hugetlb.2MB.limit_in_bytes in the container — this node's cgroup v2 hierarchy may not have the hugetlb controller enabled"
    fi
    local value
    value="$(cat "$path")"
    assert_eq "$value" "4194304" "cgroup hugetlb.2MB.limit_in_bytes should match the Pod's hugepages-2Mi limit exactly (4Mi)"
    delete_pod_if_exists "$name"
}

test_cpu_limit_is_enforced_via_cgroup() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="cpu-limit"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      resources:
        limits:
          cpu: "250m"
      command: ["sh", "-c", "cat /sys/fs/cgroup/cpu.max > /shared/cpumax.txt 2>/shared/cpumax.err; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local value
    if ! value="$(try_wait_until 30 bash -c "[[ -s \"\$(pod_volume_host_path '$name' shared)/cpumax.txt\" ]]" && wait_for_check_file "$name" shared cpumax.txt 5)"; then
        delete_pod_if_exists "$name"
        skip_test "no /sys/fs/cgroup/cpu.max in the container — this node likely uses cgroup v1, not v2"
    fi
    # 250m -> cpu_period_us default 100000, quota = 100000 * 250 / 1000 = 25000.
    assert_eq "$value" "25000 100000" "cgroup cpu.max should be '<quota> <period>' matching linux_resources()'s formula"
    delete_pod_if_exists "$name"
}

test_besteffort_pod_gets_no_cgroup_limit() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="besteffort"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "cat /sys/fs/cgroup/memory.max > /shared/memmax.txt 2>/shared/memmax.err; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local value
    if ! value="$(try_wait_until 30 bash -c "[[ -s \"\$(pod_volume_host_path '$name' shared)/memmax.txt\" ]]" && wait_for_check_file "$name" shared memmax.txt 5)"; then
        delete_pod_if_exists "$name"
        skip_test "no /sys/fs/cgroup/memory.max in the container — this node likely uses cgroup v1, not v2"
    fi
    assert_eq "$value" "max" "a BestEffort pod (no memory limit set) must not get an accidental cgroup ceiling"
    delete_pod_if_exists "$name"
}

test_besteffort_pod_gets_the_certain_death_oom_score() {
    # Round 28: oom_score_adj wasn't set at all before this — every
    # container got the kernel's own default, no QoS signal at all.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="oom-score-besteffort"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "cat /proc/self/oom_score_adj > /shared/oom.txt; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local value
    value="$(wait_for_check_file "$name" shared oom.txt 30)"
    assert_eq "$value" "1000" "BestEffort containers should get oom_score_adj=1000 (the kernel's most-likely-to-kill value), matching eviction::oom_score_adj()"
    delete_pod_if_exists "$name"
}

test_guaranteed_pod_gets_the_protected_oom_score() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="oom-score-guaranteed"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  volumes:
    - name: shared
      emptyDir: {}
  containers:
    - name: app
      image: $TEST_IMAGE
      resources:
        requests: { cpu: "100m", memory: "64Mi" }
        limits: { cpu: "100m", memory: "64Mi" }
      command: ["sh", "-c", "cat /proc/self/oom_score_adj > /shared/oom.txt; sleep 3600"]
      volumeMounts:
        - {name: shared, mountPath: /shared}
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local value
    value="$(wait_for_check_file "$name" shared oom.txt 30)"
    assert_eq "$value" "-998" "Guaranteed containers should get oom_score_adj=-998 (the kernel's least-likely-to-kill value), matching eviction::oom_score_adj()"
    delete_pod_if_exists "$name"
}

test_in_place_resize_updates_memory_limit_without_restarting() {
    # Round 42: in-place pod vertical scaling. Before this round, editing a
    # running pod's resources did nothing at all — not even a container
    # restart. Uses kubectl exec (streaming server, see streaming.sh) to
    # read the container's own live cgroup file before and after patching
    # the resize subresource, and confirms the container's own restart
    # count stayed at 0 (resizePolicy defaults to NotRequired).
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="resize-check"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      resources:
        limits:
          memory: "134217728"
      command: ["sleep", "3600"]
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    wait_until 30 "$name container ready" pod_container_ready "$name" app

    local before
    before="$(kctl exec "$name" -- cat /sys/fs/cgroup/memory.max 2>/dev/null || true)"
    if [[ "$before" != "134217728" ]]; then
        delete_pod_if_exists "$name"
        skip_test "couldn't read the container's own /sys/fs/cgroup/memory.max via kubectl exec — either a cgroup v1 node or an exec-path issue (see streaming.sh notes)"
    fi

    if ! kubectl --namespace "$TEST_NAMESPACE" patch pod "$name" --subresource resize --type merge \
        -p '{"spec":{"containers":[{"name":"app","resources":{"limits":{"memory":"268435456"}}}]}}' >/dev/null 2>&1; then
        delete_pod_if_exists "$name"
        skip_test "this kubectl/apiserver version doesn't support the pod 'resize' subresource (needs InPlacePodVerticalScaling, GA in Kubernetes 1.33)"
    fi

    local after waited=0
    while true; do
        after="$(kctl exec "$name" -- cat /sys/fs/cgroup/memory.max 2>/dev/null || true)"
        [[ "$after" == "268435456" ]] && break
        if [[ "$waited" -ge 30 ]]; then
            die "timed out after 30s waiting for the container's own memory.max to reflect the resized limit — check resize_decision()/UpdateContainerResources wiring in runtime/cri.rs"
        fi
        sleep 2
        waited=$((waited + 2))
    done
    assert_eq "$after" "268435456" "memory.max should reflect the resized limit"
    assert_eq "$(pod_container_restart_count "$name" app)" "0" "container restart count (resizePolicy defaults to NotRequired — must not restart)"

    # Round 43: containerStatuses[].resources should catch up to the
    # resized limit once the in-place update lands (allocatedResources
    # already reflects it as soon as the patch is accepted).
    local reported_limit
    wait_until 30 "containerStatuses[0].resources.limits.memory to catch up" bash -c \
        "[[ \"\$(kctl get pod '$name' -o jsonpath='{.status.containerStatuses[0].resources.limits.memory}')\" == '268435456' ]]"
    reported_limit="$(kctl get pod "$name" -o jsonpath='{.status.containerStatuses[0].resources.limits.memory}')"
    assert_eq "$reported_limit" "268435456" "containerStatuses[0].resources.limits.memory should reflect the actually-applied resize"

    local resize_condition
    resize_condition="$(kctl get pod "$name" -o jsonpath='{.status.conditions[?(@.type=="PodResizeInProgress")].status}')"
    assert_eq "$resize_condition" "False" "PodResizeInProgress should be False once the container's actual resources have caught up"

    delete_pod_if_exists "$name"
}

test_env_resource_field_ref_reports_the_containers_own_limits() {
    # Round 44: env valueFrom.resourceFieldRef previously bail!ed
    # "not supported yet" unconditionally. limits.cpu with no divisor
    # rounds UP to whole cores (real kubelet's well-known default-divisor
    # quirk); limits.memory with a 1Mi divisor is the classic JVM
    # heap-sizing use case.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="resource-field-ref"
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      resources:
        limits:
          cpu: "1500m"
          memory: "536870912"
      command: ["sh", "-c", "sleep 3600"]
      env:
        - name: CPU_LIMIT_CORES
          valueFrom:
            resourceFieldRef:
              resource: limits.cpu
        - name: MEM_LIMIT_MI
          valueFrom:
            resourceFieldRef:
              resource: limits.memory
              divisor: 1Mi
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local cpu_env mem_env
    cpu_env="$(kctl exec "$name" -- sh -c 'echo $CPU_LIMIT_CORES' 2>/dev/null || true)"
    mem_env="$(kctl exec "$name" -- sh -c 'echo $MEM_LIMIT_MI' 2>/dev/null || true)"
    assert_eq "$cpu_env" "2" "CPU_LIMIT_CORES should round 1500m UP to 2 whole cores (no divisor given)"
    assert_eq "$mem_env" "512" "MEM_LIMIT_MI should report 512 (536870912 bytes / 1Mi)"
    delete_pod_if_exists "$name"
}

register_test test_memory_limit_is_enforced_via_cgroup
register_test test_hugepages_limit_is_enforced_via_cgroup
register_test test_no_swap_default_disables_swap_via_cgroup
register_test test_limited_swap_gives_burstable_pods_proportional_swap
register_test test_in_place_resize_updates_memory_limit_without_restarting
register_test test_env_resource_field_ref_reports_the_containers_own_limits
register_test test_cpu_limit_is_enforced_via_cgroup
register_test test_besteffort_pod_gets_no_cgroup_limit
register_test test_besteffort_pod_gets_the_certain_death_oom_score
register_test test_guaranteed_pod_gets_the_protected_oom_score
