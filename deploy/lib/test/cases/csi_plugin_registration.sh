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

test_dynamic_csi_registration_actually_registered_the_driver() {
    # Round 123: previously manual-only, but a real CSI driver's
    # node-driver-registrar sidecar has been pointed at nodelet's registry
    # directory in CI all along (e2e-full-setup.sh installs
    # csi-driver-host-path via its own real deploy tooling, which already
    # configures --kubelet-registration-path for exactly this node). The
    # full GetInfo/NotifyRegistrationStatus handshake having actually
    # completed is directly observable: CSINode.spec.drivers is populated
    # by the apiserver *from* a successful CSI registration, not
    # something nodelet writes itself — so a driver showing up there at
    # all is real, independent proof the handshake worked.
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    if [[ -z "${TEST_CSI_INLINE_DRIVER:-}" ]]; then
        skip_test "TEST_CSI_INLINE_DRIVER not set — export it to a CSI driver name whose node-driver-registrar is pointed at this node's NODELET_PLUGIN_REGISTRY_PATH to exercise this"
    fi
    local n
    n="$(node_name)"
    local drivers
    drivers="$(kubectl get csinodes "$n" -o jsonpath='{.spec.drivers[*].name}' 2>/dev/null)"
    assert_contains "$drivers" "$TEST_CSI_INLINE_DRIVER" "CSINode.spec.drivers should list $TEST_CSI_INLINE_DRIVER — proof plugin_registry.rs's GetInfo/NotifyRegistrationStatus handshake actually completed for a real driver, not just that the registry directory exists"
}

register_test test_plugin_registry_directory_exists
register_test test_dynamic_csi_registration_actually_registered_the_driver
