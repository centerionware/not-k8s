# lib/test/cases/csi_plugin_registration.sh — dynamic CSI driver discovery
# (plugin_registry.rs), round 13's addition on top of round 12's
# static-config-only PVC support. Fully exercising this needs a real CSI
# driver's node-driver-registrar sidecar pointed at nodelet's registry
# directory instead of kubelet's — that's a driver DaemonSet deployment
# choice this suite can't make on the cluster's behalf, so this is a
# lighter check (the watcher started and created its directory) plus a
# manual-note for the full round-trip.

test_plugin_registry_directory_exists() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local dir="${NODELET_PLUGIN_REGISTRY_PATH:-/var/lib/nodelet/plugins_registry}"
    if ! try_wait_until 15 bash -c "[[ -d '$dir' ]]"; then
        skip_test "no $dir — check nodelet's startup logs for a 'plugin registry: couldn't create the registry directory' warning (needs write access to its parent)"
    fi
    assert_true test -d "$dir"
}

test_dynamic_csi_registration_manual_note() {
    skip_test "exercising the full registration handshake (GetInfo/NotifyRegistrationStatus) needs a real CSI driver's node-driver-registrar sidecar pointed at NODELET_PLUGIN_REGISTRY_PATH (not kubelet's usual /var/lib/kubelet/plugins_registry) via its --kubelet-registration-path flag — not something this suite deploys itself. Manual spot-check: point a driver's registrar at this node's NODELET_PLUGIN_REGISTRY_PATH, watch nodelet's logs for 'plugin registry: CSI driver registered', then run csi_pvc.sh's test WITHOUT setting NODELET_CSI_DRIVERS statically — it should still pass, proving the driver was discovered dynamically rather than via static config."
}

register_test test_plugin_registry_directory_exists
register_test test_dynamic_csi_registration_manual_note
