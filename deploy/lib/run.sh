# lib/run.sh — start nodelet as a service and verify the deployment.

run_and_verify() {
    export KUBECONFIG="${KUBECONFIG:-/etc/rancher/k3s/k3s.yaml}"
    if [[ ! -f "$KUBECONFIG" ]]; then
        warn "No KUBECONFIG at $KUBECONFIG — control plane wasn't set up (see above)."
        warn "nodelet needs an apiserver to register against; stopping before running it."
        return 0
    fi

    export NODELET_RUNTIME="mock"
    [[ "$WITH_CRI" -eq 1 ]] && NODELET_RUNTIME="cri"

    log "Starting nodelet (runtime=$NODELET_RUNTIME)..."
    install_nodelet_service

    # nodeproxy is a separate service with no ordering relationship to
    # nodelet — it only needs the apiserver. --proxy=none skips it entirely
    # (something else owns ClusterIP/NodePort routing on this node), and
    # under the mock runtime there are no real pod IPs to route to at all,
    # which is what nodelet's old service_proxy default already encoded.
    #
    # "Not wanted" has to mean *removed*, not merely "not installed": these
    # scripts are re-run against machines they already deployed to, so a host
    # that gets --proxy=none after a previous default run would otherwise keep
    # the old nodeproxy service running and its nftables table live, silently
    # DNAT'ing traffic out from under whatever was supposed to take over.
    # remove_nodeproxy_service + stop_service_proxy_nft specifically, NOT
    # uninstall.sh's stop_running_components — that one also tears down
    # flanneld and containerd, which this run still needs.
    if want_nodeproxy && [[ "$WITH_CRI" -eq 1 ]]; then
        log "Starting nodeproxy (Service routing: ip_family=$IP_FAMILY lb_method=$LB_METHOD)..."
        install_nodeproxy_service
    else
        if want_nodeproxy; then
            log "Skipping nodeproxy — the mock runtime has no real pod IPs to route Services to."
        else
            log "Skipping nodeproxy (--proxy=none) — ClusterIP/NodePort routing is this node's own business."
        fi
        remove_nodeproxy_service
        stop_service_proxy_nft
    fi

    # nodescheduler, same shape and the same "not wanted means removed"
    # reasoning as nodeproxy above — a host that gets --scheduler=none after a
    # previous --scheduler=nodescheduler run would otherwise keep our
    # scheduler binding pods alongside the k3s one this run just re-enabled,
    # which is the exact double-binding race the flag exists to prevent.
    #
    # Unlike nodeproxy there is no --with-cri condition: placement is a
    # control-plane decision about which node a pod belongs on, and it is just
    # as real under the mock runtime as under containerd.
    if want_nodescheduler; then
        log "Starting nodescheduler (pod placement; k3s's own kube-scheduler is disabled)..."
        install_nodescheduler_service
    else
        remove_nodescheduler_service
    fi

    # nodecontroller, same shape and the same "not wanted means removed"
    # reasoning as nodescheduler above — a host that gets
    # --controller-manager=none after a previous
    # --controller-manager=nodecontroller run would otherwise keep ours
    # tainting Nodes/allocating podCIDRs alongside the k3s one this run just
    # re-enabled, the same double-writer race the flag exists to prevent.
    #
    # No --with-cri condition either, for the same reason nodescheduler has
    # none: node lifecycle/podCIDR/GC are control-plane decisions, not
    # runtime-dependent ones.
    if want_nodecontroller; then
        log "Starting nodecontroller (node lifecycle/podCIDR/GC; k3s's own kube-controller-manager is disabled)..."
        install_nodecontroller_service
    else
        remove_nodecontroller_service
    fi

    log "Waiting for the node to register..."
    for i in $(seq 1 20); do
        if kubectl get nodes --no-headers 2>/dev/null | grep -q .; then
            break
        fi
        sleep 2
    done
    kubectl get nodes -o wide || warn "kubectl get nodes failed — check: journalctl -u nodelet -n 50 (or $LOG_DIR/nodelet.log if systemd isn't available)"

    wait_for_flannel_subnet

    log "Applying demo pod..."
    kubectl apply -f "$REPO_ROOT/deploy/demo-pod.yaml"
    sleep 3
    kubectl get pods -o wide

    log "Done. Logs: journalctl -u nodelet -f"
    log "Tear everything down with: $0 --cleanup"
}
