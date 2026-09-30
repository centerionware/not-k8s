#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="${GITHUB_WORKSPACE:-/workspace/not-k8s}"
export GITHUB_WORKSPACE="$ROOT"
export NODEMIGRATE_INTEGRATION_LIBRARY=true
export NODEMIGRATE_HOSTPATH_SETUP="${NODEMIGRATE_HOSTPATH_SETUP:-/tmp/nodemigrate-hostpath-setup.sh}"
export SOURCE_DIST=kubernetes

cd "$ROOT"
source .github/scripts/nodemigrate-integration.sh

SOURCE_KUBECONFIG=/etc/kubernetes/admin.conf
CURRENT_KUBECONFIG="$SOURCE_KUBECONFIG"
export KUBECONFIG="$SOURCE_KUBECONFIG"

case "${1:?use source, nodestore, or returned}" in
    source)
        install_hostpath_driver /var/lib/kubelet
        # Keep the node-local hostpath catalog on the same node that will own
        # the generated PV topology before provisioning any fixture claims.
        ensure_hostpath_topology_label worker-1
        pin_hostpath_driver_to_node worker-1
        install_workloads
        pin_hostpath_driver_to_fixture_volumes
        verify_stage source "$SOURCE_KUBECONFIG"
        capture_source_csi_device_volume
        ;;
    nodestore)
        local_target_kubeconfig=/etc/nodebootstrap/admin.kubeconfig
        CURRENT_KUBECONFIG="$local_target_kubeconfig"
        export KUBECONFIG="$local_target_kubeconfig"
        assert_csi_device_volume_matches_source "$local_target_kubeconfig" after-forward-migration
        install_hostpath_driver /var/lib/nodelet true
        pin_hostpath_driver_to_fixture_volumes
        restore_csi_device_volume_after_fixture_reinstall "$local_target_kubeconfig" nodestore
        verify_stage nodestore "$local_target_kubeconfig"
        assert_migratable_api_objects_retained source nodestore
        ;;
    returned)
        CURRENT_KUBECONFIG="$SOURCE_KUBECONFIG"
        export KUBECONFIG="$SOURCE_KUBECONFIG"
        assert_csi_device_volume_matches_source "$SOURCE_KUBECONFIG" after-return-migration
        install_hostpath_driver /var/lib/kubelet true
        pin_hostpath_driver_to_fixture_volumes
        restore_csi_device_volume_after_fixture_reinstall "$SOURCE_KUBECONFIG" returned
        verify_stage returned "$SOURCE_KUBECONFIG"
        assert_round_trip_unchanged
        ;;
    *)
        echo "unknown five-node fixture checkpoint: $1" >&2
        exit 2
        ;;
esac
