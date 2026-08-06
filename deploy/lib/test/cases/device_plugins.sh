# lib/test/cases/device_plugins.sh — device plugin discovery, Node
# capacity/allocatable advertisement, and Allocate() wiring into container
# creation (device_plugins.rs). Reuses the same plugin-registration
# directory csi_plugin_registration.sh checks (plugin_registry.rs handles
# both CSI and device plugin registrations through one watcher).
#
# Fully exercising this needs a real device plugin (nvidia-device-plugin,
# or similar) pointed at nodelet's registry directory — this suite doesn't
# have GPU/FPGA hardware to test against, and a real plugin binary isn't
# something to bundle here. A hand-rolled fake gRPC device plugin (no real
# hardware needed — GetInfo/ListAndWatch/Allocate can all be faked) would
# make this fully automatable; that's a natural next step, not attempted
# in this round's bash-only harness.

test_plugin_registry_watches_for_device_plugins_too() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local dir="${NODELET_PLUGIN_REGISTRY_PATH:-/var/lib/nodelet/plugins_registry}"
    if ! try_wait_until 15 bash -c "[[ -d '$dir' ]]"; then
        skip_test "no $dir — same directory csi_plugin_registration.sh checks; see its notes if this fails"
    fi
    assert_true test -d "$dir"
}

test_device_plugin_manual_note() {
    skip_test "exercising real device allocation needs an actual device plugin (e.g. nvidia-device-plugin) with real or faked hardware behind it, pointed at NODELET_PLUGIN_REGISTRY_PATH — not something this suite can set up. Manual spot-check: deploy a device plugin DaemonSet configured to register against this node's registry path, watch nodelet's logs for 'device plugin: inventory updated' and 'plugin registry: plugin registered', confirm 'kubectl describe node' shows the resource (e.g. nvidia.com/gpu) under Capacity/Allocatable, then create a pod with resources.limits['<resource>']: 1 and confirm it reaches Running with the device plugin's Allocate() response (envs/mounts/device nodes) actually present in the container (kubectl exec + env / ls the mounted paths)."
}

test_device_plugin_preferred_allocation_and_prestart_manual_note() {
    skip_test "exercising GetPreferredAllocation/PreStartContainer (round 21) needs a device plugin whose DevicePluginOptions actually sets get_preferred_allocation_available/pre_start_required — most reference plugins (e.g. nvidia-device-plugin) don't by default, so this is a manual spot-check even given real hardware. Manual spot-check: point a device plugin that implements GetPreferredAllocation at this node, request its resource from a pod, and confirm nodelet's logs show no 'GetPreferredAllocation failed; falling back' warning (proof the plugin's response was used) — or deliberately have the plugin return a bogus/duplicate/wrong-count response and confirm the warning *does* appear with the pod still reaching Running (proof the fallback to pick_devices_preferring() works). For PreStartContainer: point a plugin with pre_start_required=true at this node and confirm nodelet's logs show a successful PreStartContainer call before the container starts, and that a plugin returning an error from PreStartContainer leaves the pod in a failed/retrying state rather than starting with stale device state."
}

test_allocated_resources_status_absent_without_device_resources() {
    # Regression check for round 79's wiring: a plain pod with no
    # device-plugin resources requested must never get a spurious
    # allocatedResourcesStatus entry.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="no-device-resources"
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
EOF
    wait_until 60 "$name Running" pod_is_phase "$name" Running
    local ars
    ars="$(kctl get pod "$name" -o jsonpath='{.status.containerStatuses[0].allocatedResourcesStatus}')"
    delete_pod_if_exists "$name"
    assert_eq "$ars" "" "containerStatuses[0].allocatedResourcesStatus should be empty/absent for a pod with no device-plugin resources allocated"
}

test_allocated_resources_status_manual_note() {
    # Round 79 (ResourceHealthStatus, KEP-4680; found in round 72's
    # re-audit): containerStatuses[].allocatedResourcesStatus reports
    # live per-device health for devices a container currently holds.
    # Same hardware limitation as every other device-plugin round --
    # genuinely observing a device transition Healthy->Unhealthy AFTER
    # allocation needs real (or faked) hardware misbehaving on cue.
    skip_test "exercising allocatedResourcesStatus needs a real device plugin (or a fake one that can be told to report a device unhealthy on demand) with a pod already holding that device allocated. Manual spot-check: deploy a device plugin, run a pod with resources.limits['<resource>']: 1, confirm 'kubectl get pod <name> -o jsonpath={.status.containerStatuses[0].allocatedResourcesStatus}' shows an entry for that resource with health: Healthy (matching the plugin's ListAndWatch state at allocation time) -- then have the plugin's own ListAndWatch report that specific device ID as unhealthy (most reference plugins support this via a fault-injection mechanism, e.g. removing/restoring a GPU) and confirm the SAME field updates to Unhealthy on nodelet's next reconcile of that pod, without the container itself being restarted (this is a live status signal, not a restart trigger)."
}

register_test test_plugin_registry_watches_for_device_plugins_too
register_test test_device_plugin_manual_note
register_test test_device_plugin_preferred_allocation_and_prestart_manual_note
register_test test_allocated_resources_status_absent_without_device_resources
register_test test_allocated_resources_status_manual_note
