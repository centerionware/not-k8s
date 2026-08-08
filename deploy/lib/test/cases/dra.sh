# lib/test/cases/dra.sh — Dynamic Resource Allocation (spec.resourceClaims,
# round 63): DRA driver plugin discovery, ResourceClaim allocation
# resolution, and NodePrepareResources CDI device wiring into container
# creation (dra.rs / runtime/cri.rs's resolve_pod_claim_devices()).
# Reuses the same plugin-registration directory csi_plugin_registration.sh
# and device_plugins.sh check (plugin_registry.rs handles CSI/device
# plugin/DRA driver registrations through one watcher).
#
# **Round 121: manually verified live** against
# kubernetes-sigs/dra-example-driver (the reference DRA driver real
# Kubernetes e2e/conformance tests use for exactly this, same role
# csi-driver-host-path plays for CSI) — a real Pod claiming a device via
# ResourceClaimTemplate reached Running with genuine CDI-injected device
# env vars from the driver's own NodePrepareResources response. Found and
# fixed 3 real bugs in the process (unbound ServiceAccount tokens,
# ResourceClaim fetched against the removed v1beta1 API, and
# draplugin.proto's reconstructed wire format being genuinely wrong — see
# docs/E2E_FINDINGS.md finding #18 for the full story). draplugin.proto is
# now transcribed directly from upstream, not reconstructed — that
# specific caveat is resolved.
#
# Round 123: that GitHub Actions automation now exists —
# e2e-full-setup.sh installs the reference driver (Helm) and writes
# TEST_DRA_DEVICE_CLASS — so test_dra_claim_is_allocated_and_reserved_for_the_pod
# below is a real `register_test`, not a manual spot-check, when run
# there. Running this file's tests against a bare local nodelet (no
# e2e-full-setup.sh) still skips them cleanly via the same
# TEST_DRA_DEVICE_CLASS gate CSI's own tests use for TEST_CSI_*.

test_plugin_registry_watches_for_dra_drivers_too() {
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local dir="${NODELET_PLUGIN_REGISTRY_PATH:-/var/lib/nodelet/plugins_registry}"
    if ! try_wait_until 30 bash -c "[[ -d '$dir' ]]"; then
        skip_test "no $dir — same directory csi_plugin_registration.sh/device_plugins.sh check; see their notes if this fails"
    fi
    assert_true test -d "$dir"
}

test_resource_api_group_is_enabled() {
    # Round 123: this used to be a "manual spot-check" purely because the
    # skip message hedged on whether the apiserver's own version/build
    # actually has the DynamicResourceAllocation feature gate on — but
    # that's exactly what a real `kubectl api-resources` call answers,
    # automatically, no guessing needed.
    local resourceclaims
    resourceclaims="$(kubectl api-resources 2>/dev/null | grep -c '^resourceclaims ' || true)"
    if [[ "$resourceclaims" -eq 0 ]]; then
        skip_test "resource.k8s.io/resourceclaims isn't registered on this apiserver — DynamicResourceAllocation feature gate is off on this deployment's k3s build"
    fi
    assert_true bash -c "kubectl api-resources 2>/dev/null | grep -q '^resourceclaims '"
}

test_dra_claim_is_allocated_and_reserved_for_the_pod() {
    # Round 123: dra-example-driver is already installed in CI
    # (e2e-full-setup.sh), same as csi-driver-host-path — this used to be
    # manual-only anyway, purely because nothing ever wired
    # TEST_DRA_DEVICE_CLASS through. Real round-trip: a ResourceClaimTemplate
    # + a Pod referencing it via spec.resourceClaims, exactly
    # dra-example-driver's own basic-resourceclaimtemplate demo shape.
    if [[ -z "${TEST_DRA_DEVICE_CLASS:-}" ]]; then
        skip_test "TEST_DRA_DEVICE_CLASS not set — export it to a real DeviceClass name (e.g. 'gpu.example.com' for dra-example-driver) once a DRA driver is registered to exercise this"
    fi
    if ! node_uses_cri_runtime; then skip_test "needs cri runtime"; fi
    local name="dra-claim-check"
    local template="$name-template"
    apply_manifest <<EOF
apiVersion: resource.k8s.io/v1
kind: ResourceClaimTemplate
metadata:
  name: $template
spec:
  spec:
    devices:
      requests:
        - name: gpu
          exactly:
            deviceClassName: $TEST_DRA_DEVICE_CLASS
EOF
    apply_manifest <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
spec:
  containers:
    - name: app
      image: $TEST_IMAGE
      command: ["sh", "-c", "env; sleep 3600"]
      resources:
        claims:
          - name: gpu
  resourceClaims:
    - name: gpu
      resourceClaimTemplateName: $template
EOF
    if ! try_wait_until 90 pod_is_phase "$name" Running; then
        delete_pod_if_exists "$name"
        kctl delete resourceclaimtemplate "$template" --ignore-not-found >/dev/null 2>&1
        die "pod never reached Running with a DRA resourceClaim — check nodelet's logs for 'DRA: NodePrepareResources failed' or 'DRA: claim not yet reserved for this pod'"
    fi

    # The claim the template minted takes the pod's own generated-name
    # shape (<pod>-<claim-name>-<suffix>) — find it by owner reference
    # instead of guessing the exact generated name.
    local claim_name
    claim_name="$(kctl get resourceclaims -o jsonpath="{.items[?(@.metadata.ownerReferences[0].name==\"$name\")].metadata.name}")"
    assert_not_empty "$claim_name" "a ResourceClaim owned by pod $name"

    local allocation reserved_for
    allocation="$(kctl get resourceclaim "$claim_name" -o jsonpath='{.status.allocation}')"
    reserved_for="$(kctl get resourceclaim "$claim_name" -o jsonpath="{.status.reservedFor[?(@.name==\"$name\")].name}")"
    assert_not_empty "$allocation" "status.allocation on the ResourceClaim — the driver's own controller must have allocated a real device"
    assert_eq "$reserved_for" "$name" "status.reservedFor should list this pod — round 64's gate on NodePrepareResources"

    delete_pod_if_exists "$name"
    kctl delete resourceclaimtemplate "$template" --ignore-not-found >/dev/null 2>&1
}

register_test test_plugin_registry_watches_for_dra_drivers_too csi_dra
register_test test_resource_api_group_is_enabled csi_dra
register_test test_dra_claim_is_allocated_and_reserved_for_the_pod csi_dra
