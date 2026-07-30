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
    export NODELET_IP_FAMILY="$IP_FAMILY"
    export NODELET_LB_METHOD="$LB_METHOD"

    log "Starting nodelet (runtime=$NODELET_RUNTIME)..."
    install_nodelet_service

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
