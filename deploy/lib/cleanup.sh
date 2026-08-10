# lib/cleanup.sh — footprint cleanup: disk, not functionality. Only
# *build-only* tools are in scope here (see bootstrap-source.sh's header):
# rustc/cargo, the C/C++ toolchain, protoc, Go, and git if this script
# installed it fresh purely to `git clone` one of the above. Runtime pieces
# the cluster keeps needing (containerd, runc, flanneld, CNI plugins,
# nftables, k3s) are never touched here — see uninstall.sh for those.

# Must match the `name` argument nodelet's build-only pkg_install call sites
# use, so cleanup only ever removes packages installed for compiling things,
# never packages a *running* cluster still depends on.
BUILD_ONLY_PKG_NAMES=" C toolchain C++ compiler rust protoc go git "

# Removes only build-only packages (see BUILD_ONLY_PKG_NAMES) — used after
# every normal build, since the cluster still needs runtime packages
# (containerd/runc/nftables/CNI plugins/flannel) running.
uninstall_tracked_build_packages() {
    uninstall_tracked_packages_matching "$BUILD_ONLY_PKG_NAMES"
}

# Removes every package this script installed, build or runtime — used by
# --uninstall, which tears down the whole deployment, not just the build.
uninstall_all_tracked_packages() {
    uninstall_tracked_packages_matching ""
}

# $1: space-padded whitelist of logical names to remove (e.g.
# BUILD_ONLY_PKG_NAMES), or "" to remove everything logged regardless of name.
uninstall_tracked_packages_matching() {
    local whitelist="$1"
    [[ -f "$WORK_DIR/pkg_installs.log" ]] || return 0
    local removed_any=0
    while IFS='|' read -r mgr pkg_name pkgs; do
        if [[ -n "$whitelist" ]]; then
            case "$whitelist" in
                *" $pkg_name "*) ;;
                *) continue ;; # not in the whitelist — leave it installed
            esac
        fi
        [[ -z "$pkgs" ]] && continue
        log "Removing package(s) this script installed: $pkgs (via $mgr)"
        remove_pkgs_via_mgr "$mgr" "$pkgs" \
            || warn "Failed to remove '$pkgs' via $mgr — leaving it installed (see $LOG_DIR/pkg.log)."
        removed_any=1
    done < "$WORK_DIR/pkg_installs.log"
    if [[ "$removed_any" -eq 1 && "$PKG_MGR" == "apt" ]]; then
        $SUDO apt-get autoremove -y -qq >>"$LOG_DIR/pkg.log" 2>&1 || true
    fi
    rm -f "$WORK_DIR/pkg_installs.log"
}

cleanup_build_footprint() {
    [[ "$KEEP_BUILD_TOOLS" -eq 1 ]] && { log "Skipping build-toolchain cleanup (--keep-build-tools)."; return 0; }
    log "Cleaning up build toolchain (keeping the built binary + whatever the cluster needs running)..."

    # All download/build scratch — tarballs, extracted sources, git clones
    # of gcc/binutils/go/protobuf/containerd/runc/plugins/flannel/cni-plugin.
    # None of it is needed once the binaries it produced exist elsewhere.
    rm -rf "$SRC_DIR"

    # Self-contained build toolchains under .bootstrap/ — always ours,
    # always safe, regardless of whether they came from rustup, a musl.cc
    # static download, or a from-source build.
    rm -rf "$TOOLCHAIN_DIR/rustup" "$TOOLCHAIN_DIR/cargo" "$TOOLCHAIN_DIR/go" \
           "$TOOLCHAIN_DIR/gcc-src-build" "$TOOLCHAIN_DIR/protoc-dist" "$TOOLCHAIN_DIR/protoc-src-build"
    rm -rf "$TOOLCHAIN_DIR"/*-cross 2>/dev/null
    # $TOOLCHAIN_DIR/bin is mixed: build-only (cc/gcc/g++/protoc/go) sits
    # next to runtime binaries (runc/containerd/flanneld) — remove only the
    # named build-only entries, never glob the whole directory.
    rm -f "$TOOLCHAIN_DIR/bin"/{cc,gcc,g++,protoc,go}

    # cargo's build cache (deps, incremental, fingerprints) — the only
    # thing worth keeping was already copied to bin/ (build_nodelet's
    # install step, whichever layout it built).
    rm -rf "$REPO_ROOT/target"

    # Build-only *system* packages this run installed fresh (never anything
    # that pre-existed, and never containerd/runc/CNI/flannel/nftables).
    uninstall_tracked_build_packages

    # -L so a combined-layout bin/nodelet (a symlink to bin/notk8s) reports
    # the binary's real size rather than the symlink's, and -c so the total
    # is the whole installed set, not just the node agent.
    log "Footprint cleanup done. $(du -shcL "$REPO_ROOT/bin"/* 2>/dev/null | tail -1 | cut -f1) of binaries in $REPO_ROOT/bin is what's left of the build."
}
