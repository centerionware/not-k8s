# lib/cni.sh — CNI networking: real pod IPs instead of hostNetwork-only.
# containerd's own CRI plugin invokes CNI on RunPodSandbox for any pod that
# isn't hostNetwork (nodelet already only forces hostNetwork when the Pod
# spec asks for it — see crates/nodelet/src/runtime/cri.rs). What's missing
# without this file: the CNI plugin binaries, and a network config file
# telling containerd which plugin to invoke and how. Node PodCIDR allocation
# itself needs no changes here — k3s's controller-manager runs
# --allocate-node-cidrs unconditionally, precisely so `--flannel-backend=none`
# (which setup-control-plane.sh already passes) can be paired with a
# self-managed CNI. That's exactly this setup.

CNI_BIN_DIR=/opt/cni/bin
CNI_CONF_DIR=/etc/cni/net.d

cni_go_arch_map() {   # arch names used by containernetworking/plugins and flannel-io/cni-plugin releases
    case "$ARCH" in
        x86_64)  echo amd64 ;;
        aarch64) echo arm64 ;;
        armv7l)  echo arm ;;
        ppc64le) echo ppc64le ;;
        s390x)   echo s390x ;;
        riscv64) echo riscv64 ;;
        *)       echo "" ;;
    esac
}

# Standard plugins (bridge, host-local, loopback, portmap, ...) that flannel
# (and most other CNIs) delegate to for the actual veth/bridge/IPAM work.
ensure_cni_base_plugins() {
    [[ -x "$CNI_BIN_DIR/bridge" && -x "$CNI_BIN_DIR/host-local" ]] && return 0
    mkdir -p "$CNI_BIN_DIR"

    if pkg_install "CNI plugins" "containernetworking-plugins" "containernetworking-plugins" \
            "cni-plugins" "cni-plugins" "containernetworking-plugins" "containernetworking-plugins"; then
        # Distro packages install to their own dir; point ours at it. Using
        # the exact path just verified to actually have the plugin binary —
        # NOT `command -v bridge`, which found the wrong thing in testing:
        # `bridge` is also the name of an unrelated standard Linux
        # networking tool (part of iproute2, for managing bridge devices),
        # commonly present at /usr/sbin/bridge independent of whether CNI
        # plugins are installed at all. That silently pointed CNI_BIN_DIR at
        # the wrong directory, and nothing downstream (containerd's config)
        # ever got told, so RunPodSandbox couldn't find the real plugins —
        # pods stuck forever instead of erroring somewhere visible.
        local distro_cni_dir=""
        [[ -x /usr/lib/cni/bridge ]] && distro_cni_dir=/usr/lib/cni
        [[ -x /usr/libexec/cni/bridge ]] && distro_cni_dir=/usr/libexec/cni
        if [[ -n "$distro_cni_dir" ]]; then
            CNI_BIN_DIR="$distro_cni_dir"
            log "Using distro CNI plugins in $CNI_BIN_DIR"
            return 0
        fi
    fi

    local goarch; goarch="$(cni_go_arch_map)"
    if [[ -n "$goarch" ]]; then
        local ver=1.5.1
        log "Fetching official containernetworking/plugins release for linux-$goarch..."
        if fetch "https://github.com/containernetworking/plugins/releases/download/v$ver/cni-plugins-linux-$goarch-v$ver.tgz" "$SRC_DIR/cni-plugins.tgz"; then
            tar xzf "$SRC_DIR/cni-plugins.tgz" -C "$CNI_BIN_DIR"
            [[ -x "$CNI_BIN_DIR/bridge" ]] && { log "CNI base plugins ready in $CNI_BIN_DIR"; return 0; }
        fi
    fi

    log "No prebuilt CNI plugins for $ARCH — building from source (needs Go)."
    ensure_go
    command -v git &>/dev/null || pkg_install git git git git git git git || true
    cd "$SRC_DIR"
    [[ -d plugins ]] || git clone --depth 1 --branch v1.5.1 https://github.com/containernetworking/plugins.git
    ( cd plugins && ./build_linux.sh )
    cp plugins/bin/* "$CNI_BIN_DIR/"
    [[ -x "$CNI_BIN_DIR/bridge" ]] || die "CNI base plugin source build did not produce usable binaries."
}

ensure_flannel_binaries() {
    command -v flanneld &>/dev/null || pkg_install "flannel" "flannel" "flannel" "flannel" "flannel" "flannel" "flannel" || true

    local goarch; goarch="$(cni_go_arch_map)"
    if ! command -v flanneld &>/dev/null && [[ -n "$goarch" ]]; then
        local ver=0.25.6
        log "Fetching official flannel release for linux-$goarch..."
        fetch "https://github.com/flannel-io/flannel/releases/download/v$ver/flanneld-$goarch" "$TOOLCHAIN_DIR/bin/flanneld" \
            && chmod +x "$TOOLCHAIN_DIR/bin/flanneld" || true
    fi
    if ! command -v flanneld &>/dev/null; then
        log "No prebuilt flanneld for $ARCH — building from source (needs Go)."
        ensure_go
        command -v git &>/dev/null || pkg_install git git git git git git git || true
        cd "$SRC_DIR"
        [[ -d flannel ]] || git clone --depth 1 --branch v0.25.6 https://github.com/flannel-io/flannel.git
        ( cd flannel && make dist/flanneld )
        install -m 0755 flannel/dist/flanneld "$TOOLCHAIN_DIR/bin/flanneld"
    fi
    command -v flanneld &>/dev/null || die "Could not obtain a flanneld binary for $ARCH."

    # The CNI-side flannel plugin (reads /run/flannel/subnet.env, delegates to bridge).
    if [[ ! -x "$CNI_BIN_DIR/flannel" ]]; then
        if [[ -n "$goarch" ]]; then
            fetch "https://github.com/flannel-io/cni-plugin/releases/download/v1.6.0-flannel1/flannel-$goarch" "$CNI_BIN_DIR/flannel" \
                && chmod +x "$CNI_BIN_DIR/flannel" || true
        fi
        if [[ ! -x "$CNI_BIN_DIR/flannel" ]]; then
            ensure_go
            cd "$SRC_DIR"
            [[ -d cni-plugin ]] || git clone --depth 1 --branch v1.6.0-flannel1 https://github.com/flannel-io/cni-plugin.git
            ( cd cni-plugin && ./build.sh )
            install -m 0755 "cni-plugin/dist/flannel-$goarch" "$CNI_BIN_DIR/flannel"
        fi
    fi
    [[ -x "$CNI_BIN_DIR/flannel" ]] || die "Could not obtain the flannel CNI plugin binary for $ARCH."
}

write_flannel_cni_conf() {
    mkdir -p "$CNI_CONF_DIR"
    [[ -f "$CNI_CONF_DIR/10-flannel.conflist" ]] && return 0
    cat > "$CNI_CONF_DIR/10-flannel.conflist" <<EOF
{
  "name": "not-k8s-flannel",
  "cniVersion": "1.0.0",
  "plugins": [
    {
      "type": "flannel",
      "delegate": { "hairpinMode": true, "isDefaultGateway": true }
    },
    {
      "type": "portmap",
      "capabilities": { "portMappings": true }
    }
  ]
}
EOF
}

# flanneld's kube-subnet-mgr mode still needs an explicit net-conf.json for
# every IP family, not just dual/v6 — there's no ConfigMap in this
# standalone (non-DaemonSet) setup for it to fall back to, and it has no
# built-in default Network/Backend. Writing the file now lives entirely in
# deploy/run-flanneld.sh, which regenerates it on every single service
# start (not just once, here, at install time) — see that file's header
# for why: a systemd unit's Restart=always only re-runs ExecStart, never
# this installer, so a one-time write here can't recover from the config
# going missing after a reboot. Confirmed for real: exactly that happened
# on a live test machine — /etc/kube-flannel/net-conf.json wasn't there
# after a reboot, and flanneld crash-looped forever with no way to recover
# short of manually re-running this whole script.
start_flanneld() {
    pgrep -x flanneld &>/dev/null && return 0
    # set -u makes a bare $KUBECONFIG reference itself fatal when the
    # variable was never exported (e.g. --skip-control-plane, or --with-cri
    # running before setup_control_plane gets to export it) — default it the
    # same way run_and_verify() does before testing/using it.
    local KUBECONFIG="${KUBECONFIG:-/etc/rancher/k3s/k3s.yaml}"
    [[ -f "$KUBECONFIG" ]] || die "flanneld needs a KUBECONFIG (control plane must be up first)."
    log "Starting flanneld (kube subnet manager, backend=vxlan, ip-family=$IP_FAMILY)..."
    # flanneld does NOT read the KUBECONFIG env var — its own flag-parsing
    # code (not generic client-go behavior) only looks at an explicit
    # kubeconfig flag or --master; with neither set it skips straight to
    # in-cluster config, which only works when actually running as a pod.
    # The flag is -kubeconfig-file, NOT -kubeconfig: confirmed against this
    # flannel build's actual -h usage output ("-kubeconfig-file string
    # kubeconfig file location") after --kubeconfig itself failed with
    # "flag provided but not defined: -kubeconfig" — the earlier fix was
    # built off a client-go warning message's wording ("Neither --kubeconfig
    # nor --master was specified"), which turned out not to match this
    # binary's actual registered flag name. The usage dump is the authority
    # here, not a log message's phrasing.
    # Absolute path, not the bare command: systemd/OpenRC services get a
    # fresh, minimal PATH that doesn't include wherever this script's own
    # PATH additions put a fetched/built binary ($TOOLCHAIN_DIR/bin) — a
    # bare name here resolves fine in this script's own shell and then
    # fails with "not found" (exit 127) the moment the service manager
    # actually runs it. Confirmed for real: exactly this failure, in
    # journalctl -u flanneld, on the first version of this fix.
    local flanneld_bin; flanneld_bin="$(command -v flanneld)"
    local exec_cmd="$SCRIPT_DIR/run-flanneld.sh"
    local node_name="${NODELET_NODE_NAME:-$(hostname)}"
    install_supervised_service flanneld "flanneld — CNI overlay network daemon for not-k8s" \
        "$exec_cmd" "k3s.service" \
        "NODE_NAME=$node_name" "FLANNELD_BIN=$flanneld_bin" "KUBECONFIG=$KUBECONFIG" \
        "IP_FAMILY=$IP_FAMILY" "IPV4_CLUSTER_CIDR=$IPV4_CLUSTER_CIDR" "IPV6_CLUSTER_CIDR=$IPV6_CLUSTER_CIDR"
    # Deliberately not waiting for /run/flannel/subnet.env here: flanneld's
    # kube subnet manager can't get one until a Node object exists *with an
    # allocated PodCIDR* — which needs nodelet to have registered the node
    # first, and nodelet doesn't start until run_and_verify(), later in
    # main(). Checking this early would be structurally premature on every
    # single run, not just occasionally — see wait_for_flannel_subnet(),
    # called after nodelet's own node-registration wait succeeds instead.
}

# Called from run_and_verify() only after the node has actually registered
# (so a PodCIDR has had a chance to be allocated) — this is the point where
# flanneld having no subnet.env yet is an actual problem worth a warning,
# not just normal startup ordering.
wait_for_flannel_subnet() {
    [[ "$WITH_CRI" -eq 1 && "$CNI_PLUGIN" == "flannel" ]] || return 0
    [[ -f /run/flannel/subnet.env ]] && return 0
    log "Waiting for flanneld to pick up this node's PodCIDR..."
    for _ in $(seq 1 30); do
        [[ -f /run/flannel/subnet.env ]] && { log "flannel subnet ready: $(grep FLANNEL_SUBNET /run/flannel/subnet.env 2>/dev/null)"; return 0; }
        sleep 1
    done
    warn "flanneld still hasn't written /run/flannel/subnet.env after the node registered — \
that's an actual problem now, not just startup ordering. Check $LOG_DIR/flanneld.log and \
'kubectl get node -o jsonpath={.items[0].spec.podCIDR}' (empty means the controller-manager \
hasn't allocated one — check its --allocate-node-cidrs/--cluster-cidr flags)."
}

ensure_cni() {
    [[ "$WITH_CRI" -eq 1 ]] || return 0
    case "$CNI_PLUGIN" in
        none)
            log "CNI disabled (--cni=none) — pods will need hostNetwork: true to be reachable."
            return 0
            ;;
        flannel)
            ensure_cni_base_plugins
            ensure_flannel_binaries
            write_flannel_cni_conf
            start_flanneld
            ;;
        *)
            die "Unknown --cni='$CNI_PLUGIN'. Only 'flannel' and 'none' are implemented today. \
Adding another CNI (calico, cilium, ...) means: drop its plugin binaries in \
$CNI_BIN_DIR, write its config into $CNI_CONF_DIR, and start whatever daemon \
it needs — containerd and nodelet don't need to change, since containerd's \
CRI plugin drives CNI generically and nodelet only decides host-vs-pod \
networking per Pod spec."
            ;;
    esac
}
