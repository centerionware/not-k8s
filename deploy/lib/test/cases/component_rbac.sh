# lib/test/cases/component_rbac.sh — the replacement scheduler and
# controller-manager authenticate as their own base identities for shared
# informer reads. Keep the exact read surface executable: a missing grant
# otherwise appears much later as a PVC/DRA timeout plus a reflector warning
# storm.

_assert_component_can() {
    local identity="$1" verb="$2" resource="$3"
    local answer
    answer="$(kubectl auth can-i --as="$identity" "$verb" "$resource" --all-namespaces 2>/dev/null || true)"
    assert_eq "$answer" "yes" "$identity should be able to $verb $resource"
}

test_replacement_control_plane_identities_can_read_all_watch_inputs() {
    test_component_running nodescheduler \
        || skip_test "nodescheduler is not running — this RBAC regression applies only when the replacement scheduler is enabled"
    test_component_running nodecontroller \
        || skip_test "nodecontroller is not running — this RBAC regression applies only when the replacement controller is enabled"

    local identity verb resource
    local -a scheduler_resources=(
        persistentvolumes
        persistentvolumeclaims
        storageclasses.storage.k8s.io
        csinodes.storage.k8s.io
        csidrivers.storage.k8s.io
        csistoragecapacities.storage.k8s.io
        volumeattachments.storage.k8s.io
    )
    local -a controller_resources=(
        persistentvolumes
        persistentvolumeclaims
        storageclasses.storage.k8s.io
        volumeattachments.storage.k8s.io
    )

    for identity in system:kube-scheduler system:kube-controller-manager; do
        if [[ "$identity" == system:kube-scheduler ]]; then
            set -- "${scheduler_resources[@]}"
        else
            set -- "${controller_resources[@]}"
        fi
        for resource in "$@"; do
            for verb in get list watch; do
                _assert_component_can "$identity" "$verb" "$resource"
            done
        done
    done

    # DRA is feature-gated. When present, both replacement components must
    # be able to read the exact resources their unconditional DRA paths use;
    # on a cluster without the API group there is nothing to authorize.
    if kubectl api-resources --api-group=resource.k8s.io --no-headers 2>/dev/null | grep -q .; then
        for resource in resourceclaims.resource.k8s.io deviceclasses.resource.k8s.io resourceslices.resource.k8s.io; do
            for verb in get list watch; do
                _assert_component_can system:kube-scheduler "$verb" "$resource"
            done
        done
        if kubectl api-resources --api-group=resource.k8s.io --no-headers 2>/dev/null \
            | grep -q '^resourceclaimtemplates'; then
            for verb in get list watch; do
                _assert_component_can system:kube-controller-manager "$verb" resourceclaimtemplates.resource.k8s.io
            done
        fi
    fi
}

register_test test_replacement_control_plane_identities_can_read_all_watch_inputs
