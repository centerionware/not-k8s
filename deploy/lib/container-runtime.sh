# lib/container-runtime.sh — containerd + runc, only for --with-cri (the
# mock runtime needs neither).

fetch_containerd_runc_prebuilt() {
    local c_arch="" r_arch=""
    case "$ARCH" in
        x86_64)  c_arch=amd64;  r_arch=amd64  ;;
        aarch64) c_arch=arm64;  r_arch=arm64  ;;
        armv7l)                 r_arch=armhf  ;;
        ppc64le) c_arch=ppc64le; r_arch=ppc64le ;;
        s390x)   c_arch=s390x;  r_arch=s390x  ;;
        riscv64)                r_arch=riscv64 ;;
    esac

    if [[ -n "$c_arch" ]] && ! command -v containerd &>/dev/null; then
        local cver=1.7.23
        log "Fetching official containerd release for linux-$c_arch..."
        fetch "https://github.com/containerd/containerd/releases/download/v$cver/containerd-$cver-linux-$c_arch.tar.gz" "$SRC_DIR/containerd.tar.gz" \
            && tar xzf "$SRC_DIR/containerd.tar.gz" -C "$TOOLCHAIN_DIR" || true
    fi
    if [[ -n "$r_arch" ]] && ! command -v runc &>/dev/null; then
        local rver=1.1.14
        log "Fetching official runc release for linux-$r_arch..."
        fetch "https://github.com/opencontainers/runc/releases/download/v$rver/runc.$r_arch" "$TOOLCHAIN_DIR/bin/runc" \
            && chmod +x "$TOOLCHAIN_DIR/bin/runc" || true
    fi
    command -v containerd &>/dev/null && command -v runc &>/dev/null
}

build_containerd_runc_from_source() {
    log "No prebuilt containerd/runc for $ARCH — building both from source (needs Go)."
    ensure_go
    command -v git &>/dev/null || pkg_install git git git git git git git || true
    command -v git &>/dev/null || die "Need git to fetch containerd/runc source and couldn't get it."

    if ! command -v runc &>/dev/null; then
        cd "$SRC_DIR"
        [[ -d runc ]] || git clone --depth 1 --branch v1.1.14 https://github.com/opencontainers/runc.git
        ( cd runc && make )
        install -m 0755 runc/runc "$TOOLCHAIN_DIR/bin/runc"
    fi
    if ! command -v containerd &>/dev/null; then
        cd "$SRC_DIR"
        [[ -d containerd ]] || git clone --depth 1 --branch v1.7.23 https://github.com/containerd/containerd.git
        ( cd containerd && make )
        install -m 0755 containerd/bin/containerd "$TOOLCHAIN_DIR/bin/containerd"
        [[ -f containerd/bin/containerd-shim-runc-v2 ]] && install -m 0755 containerd/bin/containerd-shim-runc-v2 "$TOOLCHAIN_DIR/bin/"
    fi
    command -v containerd &>/dev/null && command -v runc &>/dev/null \
        || die "containerd/runc source build did not produce usable binaries."
}

ensure_container_runtime() {
    [[ "$WITH_CRI" -eq 1 ]] || return 0

    if command -v containerd &>/dev/null && command -v runc &>/dev/null; then
        log "containerd + runc already present."
    else
        pkg_install "containerd/runc" "containerd runc" "containerd runc" "containerd runc" "containerd runc" "containerd runc" "containerd runc" || true
        { command -v containerd &>/dev/null && command -v runc &>/dev/null; } \
            || fetch_containerd_runc_prebuilt \
            || build_containerd_runc_from_source
    fi

    mkdir -p /etc/containerd
    [[ -f /etc/containerd/config.toml ]] || containerd config default > /etc/containerd/config.toml
    if grep -qE '(docker|containerd|kubepods)' /proc/1/cgroup 2>/dev/null; then
        log "Nested container environment detected — using the native snapshotter (overlayfs can't mount here)."
        sed -i 's/snapshotter = "overlayfs"/snapshotter = "native"/' /etc/containerd/config.toml
    fi

    if ! pgrep -x containerd &>/dev/null; then
        # If containerd came from a distro package, it almost certainly
        # already shipped its own containerd.service — use that (just not
        # enabled/started yet) instead of writing a generic one over it,
        # since the packaged unit is likely better-tuned (cgroup delegation,
        # OOM score, etc.) than anything worth generating here.
        if command -v systemctl &>/dev/null && systemctl list-unit-files containerd.service &>/dev/null; then
            log "containerd has an existing systemd unit — enabling and starting it..."
            systemctl enable --now containerd.service
            sleep 2
            systemctl is-active --quiet containerd.service \
                || die "containerd.service didn't come up — check: journalctl -u containerd -n 50"
        else
            # Absolute path, not the bare command: systemd/OpenRC services
            # get a fresh, minimal PATH that doesn't include wherever this
            # script's own PATH additions put a fetched/built binary
            # ($TOOLCHAIN_DIR/bin) — a bare name here resolves fine in this
            # script's own shell and then fails with "not found" (exit 127)
            # the moment the service manager actually runs it.
            install_supervised_service containerd "containerd container runtime (installed by not-k8s)" \
                "$(command -v containerd)" ""
        fi
        for _ in $(seq 1 15); do
            [[ -S /run/containerd/containerd.sock ]] && break
            sleep 1
        done
        [[ -S /run/containerd/containerd.sock ]] || die "containerd did not create its socket — check $LOG_DIR/containerd.log (or journalctl -u containerd)"
    fi
    export NODELET_CRI_ENDPOINT="unix:///run/containerd/containerd.sock"
}
