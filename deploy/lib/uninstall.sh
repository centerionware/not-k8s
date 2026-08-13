# lib/uninstall.sh — --cleanup and --uninstall.
#
# --cleanup stops what a run started (nodelet, flanneld, containerd, the
# nft Service table) and removes k3s + this script's own scratch — enough
# to start clean, but keeps runtime packages installed so the next run is
# fast. --uninstall goes further: k3s's data/config, containerd/runc's
# state and binaries, and all CNI/flannel config/binaries too — but, same
# rule as everywhere else, only for what it actually installed. --force
# (only meaningful with --uninstall) skips ownership tracking entirely and
# removes everything by name — see force_remove_known_packages() below and
# the --force block comment in bootstrap-source.sh's usage header.
#
# IMPORTANT set -e note, two distinct failure modes — both hit real runs:
#
# 1. A bare failing command (e.g. `pkill` finding nothing to kill,
#    `systemctl stop` on a unit that's already stopped or never installed)
#    kills the whole script immediately. Confirmed for real: an earlier
#    version of stop_running_components() had
#    `pkill -f "$SCRIPT_DIR/run-nodelet.sh" 2>/dev/null` with no `|| true`
#    right after `systemctl stop nodelet.service` — once systemctl had
#    already stopped the process, pkill found nothing, exited 1, and
#    --uninstall died right there. Fixed structurally, not by patching that
#    one spot: service stop+remove is now handled once each, by
#    remove_nodelet_service() and remove_supervised_service() (both fully
#    `|| true`-guarded — see nodelet-service.sh / service-mgr.sh), instead
#    of being duplicated here with its own separate, easy-to-forget guard.
#
# 2. Subtler: `[[ -f "$WORK_DIR/x.pid" ]] && { kill ...; }` as the LAST
#    statement of a function does NOT crash *that* function when the test
#    is false (bash exempts everything but the command after the final
#    &&/|| in a list from -e) — but the function's own exit status then
#    *is* that failing test's status, and calling the function as a bare
#    statement in the CALLER crashes there instead. Confirmed for real:
#    stop_running_components() ended in exactly this shape and --uninstall
#    died right after logging "Stopping containerd..." with nothing after
#    it, on a machine with no leftover containerd.pid file (the normal
#    case). Every risky test that's the last thing in a function here is
#    wrapped in an explicit `if`, never a bare `&&`, because of this.
# with its own separate, easy-to-forget guarding.

stop_service_proxy_nft() {
    log "Removing the Service-proxy nftables table (if present)..."
    if command -v nft &>/dev/null; then
        nft delete table inet not_k8s_svc 2>/dev/null || true
    fi
    # nodeproxy also owns an iptables chain on kernels that can't select
    # among backends natively (no nft_numgen) — see build_statistic_ruleset()
    # in crates/nodeproxy/src/svc.rs. Absent everywhere else, so all of this
    # is a no-op on a normal host.
    # -w bounds the xtables lock wait: without it these block indefinitely
    # if anything else (a CNI, a container runtime) holds the lock, and an
    # uninstall that hangs forever is worse than one that reports failure.
    local ipt
    for ipt in iptables ip6tables; do
        command -v "$ipt" &>/dev/null || continue
        # Delete jumps in a loop, not once. -D removes a single matching
        # rule, so a chain that somehow accumulated duplicates would keep
        # one alive and the -X below would then fail with "chain is not
        # empty" — leaving the chain, and its DNAT rules, in place after an
        # uninstall claimed success. Bounded so a -D that always succeeds
        # can't spin.
        local hook i
        for hook in PREROUTING OUTPUT; do
            for i in 1 2 3 4 5 6 7 8 9 10; do
                "$ipt" -w 5 -t nat -C "$hook" -j NOTK8S-SVC &>/dev/null || break
                "$ipt" -w 5 -t nat -D "$hook" -j NOTK8S-SVC &>/dev/null || break
            done
        done
        "$ipt" -w 5 -t nat -F NOTK8S-SVC &>/dev/null || true
        "$ipt" -w 5 -t nat -X NOTK8S-SVC &>/dev/null || true
        # Say so if it survived. A leftover chain still DNATs to pods that
        # no longer exist, which is a much more confusing state to debug
        # later than a warning here.
        if "$ipt" -w 5 -t nat -L NOTK8S-SVC -n &>/dev/null; then
            warn "The $ipt chain NOTK8S-SVC could not be removed and is still present. \
Remove it by hand ($ipt -t nat -F NOTK8S-SVC && $ipt -t nat -X NOTK8S-SVC) — while it exists \
it keeps DNAT'ing to pods that are gone."
        fi
    done
}

# Stops+removes everything a run started: nodelet, nodeproxy and its nft
# table, nodescheduler, nodestore, flanneld, and containerd (only the last if
# this script started it itself rather than using an existing distro-packaged
# containerd.service).
stop_running_components() {
    remove_nodelet_service
    remove_nodeproxy_service
    stop_service_proxy_nft
    # Safe to remove unconditionally on a teardown path even though it leaves
    # a cluster with no scheduler at all: everything that would notice is
    # being torn down too, and full_cleanup uninstalls k3s shortly after.
    remove_nodescheduler_service
    # After the two node components. The apiserver is a client of the
    # datastore and k3s is not torn down until later (full_cleanup's
    # uninstall_k3s), so k3s will log storage errors between here and there.
    # That is expected and harmless on a teardown path: this function's job is
    # to stop what this script started, and the alternative — leaving the
    # store up until after k3s is gone — means a --cleanup that stops short of
    # full_cleanup leaves the datastore running with nothing using it.
    #
    # Note this stops the service but deliberately leaves $NODESTORE_DATA_DIR
    # alone — that's the cluster's entire state, and destroying it silently is
    # unrecoverable (see remove_nodestore_service).
    remove_nodestore_service
    log "Stopping flanneld..."
    remove_supervised_service flanneld
    log "Stopping containerd (if this script started it)..."
    if [[ ! -f /etc/systemd/system/containerd.service ]]; then
        remove_supervised_service containerd
    fi
    if [[ -f "$WORK_DIR/containerd.pid" ]]; then
        kill "$(cat "$WORK_DIR/containerd.pid")" 2>/dev/null || true
    fi
}

# Full teardown for --cleanup: stop everything this script started running,
# uninstall k3s and every build-only package it installed (a normal run
# already did this at the end automatically unless --keep-build-tools was
# used — this covers that case too), and remove every trace under
# $WORK_DIR/bin/target. Runtime packages (containerd/runc/nftables/CNI
# plugins/flannel) and k3s's own config/data are left in place, so the next
# run doesn't have to reinstall them — use --uninstall to remove those too.
full_cleanup() {
    stop_running_components
    uninstall_k3s
    uninstall_tracked_build_packages
    log "Removing $WORK_DIR, $REPO_ROOT/bin, and $REPO_ROOT/target..."
    rm -rf "$WORK_DIR" "$REPO_ROOT/bin" "$REPO_ROOT/target"
    log "Cleanup done. Runtime packages (containerd/runc/nftables/CNI plugins/flannel) were left installed for next time — pass --uninstall for a full teardown."
}

# k3s ships k3s-uninstall.sh to /usr/local/bin by its own installer. Check
# both PATH and the literal well-known path directly: PATH lookup can, in
# principle, miss it under a restrictive sudoers secure_path even though
# the file is right there (this is the one step of --uninstall with no
# fallback if it's silently skipped — worth being redundant about finding
# it rather than trusting PATH alone).
uninstall_k3s() {
    local uninstaller=""
    if command -v k3s-uninstall.sh &>/dev/null; then
        uninstaller="$(command -v k3s-uninstall.sh)"
    elif [[ -x /usr/local/bin/k3s-uninstall.sh ]]; then
        uninstaller=/usr/local/bin/k3s-uninstall.sh
    fi
    if [[ -n "$uninstaller" ]]; then
        log "Uninstalling k3s via $uninstaller..."
        $SUDO "$uninstaller" || true
        return 0
    fi
    if [[ "$FORCE_UNINSTALL" -eq 1 ]]; then
        log "--force: no k3s-uninstall.sh found — removing k3s manually (service, binary, symlinks)..."
        $SUDO systemctl stop k3s 2>/dev/null || true
        $SUDO systemctl disable k3s 2>/dev/null || true
        $SUDO pkill -x k3s 2>/dev/null || true
        $SUDO rm -f /etc/systemd/system/k3s.service \
            /usr/local/bin/k3s /usr/local/bin/kubectl /usr/local/bin/crictl /usr/local/bin/ctr
    else
        log "No k3s-uninstall.sh found — k3s doesn't appear to be installed (or predates this script)."
    fi
}

# --force's package sweep: the same logical-name -> per-manager-package-name
# mapping every pkg_install call site above already uses, duplicated here
# (not derived from pkg_installs.log) specifically *because* --force exists
# for when that log doesn't have the answer — e.g. a run from before this
# tracking existed, or from a machine this session never touched. Matched
# by name and removed unconditionally; a package that isn't installed is a
# harmless no-op for every package manager here.
#
# Deliberately excludes git, even though pkg_install is called for it (see
# the various "command -v git || pkg_install git ..." call sites above):
# git is a fundamental, near-universal system tool almost certainly already
# on any real machine for reasons that have nothing to do with this
# project, and this project only ever needs it transiently to `git clone`
# a build dependency's source. Force-removing it by name unconditionally is
# real, disproportionate collateral damage compared to everything else in
# this list, which is genuinely specific to this project. The tracked path
# (uninstall_all_tracked_packages, via pkg_installs.log) already removes it
# correctly and safely when this script actually did install it fresh —
# pkg_install is only ever called for git when `command -v git` already
# failed, so a pre-existing git is never logged as ours in the first place.
force_remove_known_packages() {
    log "--force: removing every package this project could ever install, by name — not just what a tracking log says, since that log may not exist for whatever installed things here."
    local entries=(
        "C toolchain|build-essential|gcc make|base-devel|build-base|gcc make|base-devel"
        "C++ compiler|g++|gcc-c++|base-devel|g++|gcc-c++|base-devel"
        "rust|cargo rustc|cargo rustc|rust|cargo|cargo rustc|rust"
        "protoc|protobuf-compiler|protobuf-compiler|protobuf|protobuf|protobuf-devel|protobuf"
        "go|golang-go|golang|go|go|go|go"
        "containerd/runc|containerd runc|containerd runc|containerd runc|containerd runc|containerd runc|containerd runc"
        "CNI plugins|containernetworking-plugins|containernetworking-plugins|cni-plugins|cni-plugins|containernetworking-plugins|containernetworking-plugins"
        "flannel|flannel|flannel|flannel|flannel|flannel|flannel"
        "nftables|nftables|nftables|nftables|nftables|nftables|nftables"
    )
    local col
    case "$PKG_MGR" in
        apt) col=2 ;; dnf) col=3 ;; pacman) col=4 ;; apk) col=5 ;; zypper) col=6 ;; xbps) col=7 ;;
        *) warn "Unrecognized package manager — can't force-remove system packages by name, only files/dirs/processes."; return 0 ;;
    esac
    local entry pkgs
    for entry in "${entries[@]}"; do
        pkgs="$(cut -d'|' -f"$col" <<<"$entry")"
        [[ -z "$pkgs" ]] && continue
        log "Force-removing (if present): $pkgs"
        remove_pkgs_via_mgr "$PKG_MGR" "$pkgs" || true
    done
    [[ "$PKG_MGR" == "apt" ]] && { $SUDO apt-get autoremove -y -qq >>"$LOG_DIR/pkg.log" 2>&1 || true; }
    rm -f "$WORK_DIR/pkg_installs.log"
}

# --uninstall: the nuclear option. Everything --cleanup does, plus k3s's own
# data/config, containerd/runc's state and binaries, and all CNI/flannel
# config and binaries — but only for pieces this script actually installed.
# containerd/runc get the same "was it already here" check pkg tracking gives
# packages: if $TOOLCHAIN_DIR/bin/containerd exists or "containerd/runc" was
# logged as installed, this script put it there and its state is fair game;
# otherwise it predates this script (e.g. Docker's containerd) and is left
# completely alone, config/data included. CNI/flannel use the same idea, keyed
# on the "flannel" CNI plugin binary specifically — nothing else installs a
# file with that name into /opt/cni/bin, so its presence reliably means this
# script's ensure_cni() wrote the whole CNI/flannel setup being removed.
full_uninstall() {
    log "Full uninstall: k3s, containerd/runc, CNI plugins, flannel, nftables — everything this script ever installed."

    local we_own_containerd=0 we_own_cni=0
    if [[ "$FORCE_UNINSTALL" -eq 1 ]]; then
        log "--force: treating containerd/runc and any CNI/flannel setup found as ours to remove, regardless of who installed them."
        we_own_containerd=1
        we_own_cni=1
    else
        { [[ -e "$TOOLCHAIN_DIR/bin/containerd" ]] || { [[ -f "$WORK_DIR/pkg_installs.log" ]] && grep -q '|containerd/runc|' "$WORK_DIR/pkg_installs.log"; }; } \
            && we_own_containerd=1
        [[ -e "$CNI_BIN_DIR/flannel" ]] && we_own_cni=1
    fi

    stop_running_components
    # stop_running_components already stopped/removed whatever *this
    # invocation's* service tier knows about. Ownership here is decided by
    # package/binary provenance instead (we_own_containerd/we_own_cni above,
    # or unconditionally under --force), which also covers a containerd
    # left running from an earlier, unrelated (or untracked) run of this
    # script that stop_running_components has no record of.
    [[ "$we_own_containerd" -eq 1 ]] && { $SUDO pkill -x containerd 2>/dev/null || true; }
    [[ "$we_own_cni" -eq 1 ]] && { $SUDO pkill -x flanneld 2>/dev/null || true; }
    sleep 1

    uninstall_k3s
    log "Removing leftover k3s config/data (if any)..."
    $SUDO rm -rf /etc/rancher /var/lib/rancher

    if [[ "$we_own_containerd" -eq 1 ]]; then
        log "Removing containerd/runc state and binaries..."
        $SUDO rm -rf /etc/containerd /run/containerd /var/lib/containerd
        rm -f "$TOOLCHAIN_DIR/bin"/{containerd,containerd-shim-runc-v2,ctr,runc}
    else
        log "containerd/runc predate this script — leaving them and their state untouched."
    fi

    if [[ "$we_own_cni" -eq 1 ]]; then
        log "Removing CNI plugins, flannel, and their config..."
        $SUDO rm -rf "$CNI_BIN_DIR" "$CNI_CONF_DIR" /etc/kube-flannel /run/flannel
        rm -f "$TOOLCHAIN_DIR/bin/flanneld"
    else
        log "No CNI/flannel setup found — nothing to remove there."
    fi

    if [[ "$FORCE_UNINSTALL" -eq 1 ]]; then
        force_remove_known_packages
    else
        uninstall_all_tracked_packages
    fi
    log "Removing $WORK_DIR, $REPO_ROOT/bin, and $REPO_ROOT/target..."
    rm -rf "$WORK_DIR" "$REPO_ROOT/bin" "$REPO_ROOT/target"
    log "Full uninstall done — the system should be back to (close to) how it was before this script ran."
}
