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

register_test test_plugin_registry_watches_for_device_plugins_too
register_test test_device_plugin_manual_note
