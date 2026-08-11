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

# ensure_cgroups_mounted — make sure something is mounted at /sys/fs/cgroup.
#
# systemd mounts the cgroup hierarchy itself, very early, so this is a no-op
# on any systemd distro. Alpine (and OpenRC generally) does not: mounting it
# is the job of the `cgroups` service, which is not in a default runlevel on
# a minimal image. Nothing before this point needs cgroups, so a node comes
# all the way up looking healthy -- containerd starts, nodelet registers, the
# node goes Ready, the scheduler assigns pods -- and only then does every
# sandbox fail inside runc:
#
#   failed to create shim task: OCI runtime create failed:
#   runc create failed: no cgroup mount found in mountinfo
#
# which reads as a container-runtime bug rather than a missing mount. Worse,
# nodelet's watch-driven reconciliation gives up after its one retry and waits
# for the next watch event (by design -- see the PodController comment in
# CLAUDE.md), so the pods then sit Pending indefinitely with no further
# attempts even once cgroups do appear.
ensure_cgroups_mounted() {
    grep -q ' /sys/fs/cgroup ' /proc/mounts 2>/dev/null && return 0

    if command -v rc-update &>/dev/null && command -v rc-service &>/dev/null; then
        log "No cgroup hierarchy mounted — enabling OpenRC's 'cgroups' service (containers can't start without it)..."
        rc-update add cgroups boot 2>/dev/null || true
        rc-service cgroups start 2>/dev/null || true
    fi

    grep -q ' /sys/fs/cgroup ' /proc/mounts 2>/dev/null \
        || warn "Nothing is mounted at /sys/fs/cgroup. Container creation will fail in runc with 'no cgroup mount found in mountinfo'. Mount it before starting workloads."
}

ensure_container_runtime() {
    [[ "$WITH_CRI" -eq 1 ]] || return 0

    ensure_cgroups_mounted

    if command -v containerd &>/dev/null && command -v runc &>/dev/null; then
        log "containerd + runc already present."
    else
        pkg_install "containerd/runc" "containerd runc" "containerd runc" "containerd runc" "containerd runc" "containerd runc" "containerd runc" || true
        { command -v containerd &>/dev/null && command -v runc &>/dev/null; } \
            || fetch_containerd_runc_prebuilt \
            || build_containerd_runc_from_source
    fi

    mkdir -p /etc/containerd
    local wrote_fresh_config=0
    [[ -f /etc/containerd/config.toml ]] || { containerd config default > /etc/containerd/config.toml; wrote_fresh_config=1; }
    if grep -qE '(docker|containerd|kubepods)' /proc/1/cgroup 2>/dev/null; then
        log "Nested container environment detected — using the native snapshotter (overlayfs can't mount here)."
        sed -i 's/snapshotter = "overlayfs"/snapshotter = "native"/' /etc/containerd/config.toml
    fi
    # CDI (Container Device Interface) device injection — off by default in
    # containerd's own generated config. nodelet's own DRA support
    # (dra.rs/runtime/cri's ContainerConfig.cdi_devices, round 63) hands
    # containerd fully-qualified CDI device names expecting it to actually
    # inject them at OCI spec generation time; without this, those IDs are
    # silently accepted and do nothing — confirmed live standing up a real
    # DRA driver for e2e testing (round 121), where a claimed device never
    # showed up in the container despite NodePrepareResources succeeding
    # and nodelet passing its CDI IDs through correctly.
    grep -q '^\s*enable_cdi = true' /etc/containerd/config.toml \
        || sed -i 's/enable_cdi = false/enable_cdi = true/' /etc/containerd/config.toml

    # A pre-existing config.toml (the `[[ -f ... ]] ||` above skipped
    # generating one) commonly means containerd came from somewhere other
    # than this script — Docker's own package install being the case that
    # actually bit this live (found running e2e in GitHub Actions, round
    # 123: ubuntu-latest ships Docker with its own bundled containerd,
    # already running, whose shipped config.toml explicitly disables the
    # CRI plugin — Docker itself talks to containerd via its own internal
    # API, not CRI, and the two would otherwise fight over pod/container
    # lifecycle). nodelet is a CRI client; a containerd with the CRI
    # plugin disabled answers every RPC with "unknown service
    # runtime.v1.RuntimeService" and nodelet can never do anything at
    # all — confirmed for real, not a hypothetical. Strip "cri" out of
    # disabled_plugins unconditionally, not just on a freshly-generated
    # config, so an already-configured-elsewhere containerd still ends up
    # CRI-capable.
    local removed_disabled_cri=0
    if grep -qE '^\s*disabled_plugins\s*=.*"cri"' /etc/containerd/config.toml; then
        log "containerd's existing config.toml disables the CRI plugin (likely a Docker-managed install) — enabling it."
        sed -i -E 's/^(\s*disabled_plugins\s*=.*)"cri",?\s*/\1/; s/,\s*\]/]/' /etc/containerd/config.toml
        removed_disabled_cri=1
    fi

    if pgrep -x containerd &>/dev/null && { [[ "$wrote_fresh_config" -eq 1 ]] || [[ "$removed_disabled_cri" -eq 1 ]]; }; then
        # containerd was already running before we touched its config
        # (the branch below that starts it fresh never runs in that case)
        # — restart it so the CRI-enabling change actually takes effect,
        # rather than leaving the old (CRI-disabled) process running
        # forever with a config file on disk that says otherwise.
        log "containerd was already running with a stale config — restarting it to pick up the CRI-enabling change..."
        if command -v systemctl &>/dev/null && systemctl is-active --quiet containerd.service 2>/dev/null; then
            systemctl restart containerd.service
            sleep 2
            systemctl is-active --quiet containerd.service \
                || die "containerd.service failed to restart after enabling the CRI plugin — check: journalctl -u containerd -n 50"
        else
            pkill -x containerd
            for _ in $(seq 1 15); do pgrep -x containerd &>/dev/null || break; sleep 1; done
            install_supervised_service containerd "containerd container runtime (installed by not-k8s)" \
                "$(command -v containerd)" ""
        fi
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
