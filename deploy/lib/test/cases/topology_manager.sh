# lib/test/cases/topology_manager.sh — Topology Manager (topology.rs):
# coordinates CPU Manager and device plugins so a container's exclusive
# cores and allocated devices land on the same NUMA node. Real multi-NUMA
# cross-provider conflicts need either genuine multi-socket hardware or a
# device plugin reporting NUMA affinity that differs from where CPU
# Manager would otherwise pin — neither of which this suite can set up.
# What *is* automatable: the common single-NUMA-node case (most edge
# devices) must never spuriously reject a pod under any policy, including
# the strictest `single-numa-node`.

test_topology_manager_does_not_reject_pods_on_a_single_numa_node_host() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if [[ "${TEST_TOPOLOGY_MANAGER_POLICY:-}" != "single-numa-node" ]]; then
        skip_test "TEST_TOPOLOGY_MANAGER_POLICY not set to 'single-numa-node' — export it once nodelet is running with NODELET_TOPOLOGY_MANAGER_POLICY=single-numa-node (and NODELET_CPU_MANAGER_POLICY=static) to exercise this"
    fi
    if [[ ! -d /sys/devices/system/node ]] || [[ "$(find /sys/devices/system/node -maxdepth 1 -iname 'node*' -type d 2>/dev/null | wc -l)" -gt 1 ]]; then
        skip_test "this host either has no NUMA info at all or has more than one NUMA node — this test specifically covers the single-node case; a multi-node host needs the manual cross-provider check below instead"
    fi

    local name="topology-manager-check"
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
      resources:
        requests: { cpu: "1", memory: "64Mi" }
        limits: { cpu: "1", memory: "64Mi" }
EOF
    if ! try_wait_until 30 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        die "a Guaranteed 1-CPU pod was rejected on a single-NUMA-node host under single-numa-node policy — every request should trivially align to the one node that exists; check topology.rs::align()/cpu_hint() and their wiring in runtime/cri.rs::create_and_start_container()"
    fi
    delete_pod_if_exists "$name"
}

test_topology_manager_cross_provider_alignment_manual_note() {
    skip_test "proving CPU-and-device alignment (not just non-rejection) needs either real multi-socket/multi-NUMA hardware or a device plugin reporting TopologyInfo pointing at a specific NUMA node — not something this suite can set up. Manual spot-check on real multi-NUMA hardware: deploy a device plugin whose devices report differing NUMA affinity, request both an exclusive CPU and one of those devices in the same Guaranteed pod, then compare the container's cpuset.cpus (see cpu_manager.sh for how to find it) against which NUMA node the allocated device actually sits on — they should be the same node. Also try NODELET_TOPOLOGY_MANAGER_POLICY=restricted or single-numa-node with a deliberately unsatisfiable combination (e.g. requesting more exclusive CPUs than any single node has) and confirm the pod stays Pending with a 'Topology Manager: no single NUMA node can satisfy' error logged, rather than starting misaligned."
}

register_test test_topology_manager_does_not_reject_pods_on_a_single_numa_node_host
register_test test_topology_manager_cross_provider_alignment_manual_note
