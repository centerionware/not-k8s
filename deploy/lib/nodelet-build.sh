# lib/nodelet-build.sh — populate $REPO_ROOT/bin/ with the components this
# run wants, in the layout it asked for.
#
# Which components: lib/components.sh's table (nodelet, plus nodeproxy
# unless PROXY=none — the Service-routing binary, split out of nodelet the
# way kube-proxy is split from the kubelet upstream). Which layout:
# NOTK8S_BUILD_LAYOUT / --layout= — `split` (a binary per component),
# `combined` (one multi-call `notk8s` binary plus a symlink per component),
# or `both`. Everything here loops over that table rather than naming
# components, so a new one is a row there and nothing in this file.
#
# One shared cargo target dir throughout: each build after the first reuses
# nearly all of its dependency compilation, which is also why the combined
# binary costs so little extra to produce alongside the split ones.
#
# Prebuilt drop-ins short-circuit the whole cargo path (no toolchain gets
# installed at all, see bootstrap-source.sh's own check for this):
# $NOTK8S_NODELET_PREBUILT / $NOTK8S_NODEPROXY_PREBUILT for the split
# layout, $NOTK8S_COMBINED_PREBUILT for a combined binary.
# bootstrap-release.sh is the entry point that populates them from a GitHub
# release, and CI's e2e shards use the same seam to skip rebuilding a binary
# an earlier stage already built.

# install_built_binary <source-path> <name> — copies a built/prebuilt binary to
# $REPO_ROOT/bin/<name>, overwriting whatever's already there (a stale
# binary from a previous run, possibly owned by a different user if that
# run's privileges differed from this one's). Split out from build_nodelet()
# because a plain `install -m 0755 src dst` silently no-ops on a permission
# error against an existing dst on some systems' coreutils rather than
# failing loudly — confirmed for real: an earlier root-owned bin/nodelet
# left an unprivileged rebuild's `install` failing with "Permission denied"
# while the surrounding script logged success anyway and moved on, leaving
# the OLD binary running with nothing about the output saying so. rm -f
# first so the second install has nothing to fail to overwrite; if THAT
# still fails (e.g. bin/ itself isn't writable by this user), die instead of
# silently leaving a stale binary in place.
install_built_binary() {
    local src="$1" name="$2"
    mkdir -p "$REPO_ROOT/bin"
    rm -f "$REPO_ROOT/bin/$name" 2>/dev/null
    install -m 0755 "$src" "$REPO_ROOT/bin/$name" \
        || die "Couldn't install $src to $REPO_ROOT/bin/$name (permission denied? check ownership of $REPO_ROOT/bin — a previous run under different privileges, e.g. sudo vs. not, can leave it owned by another user). The build itself succeeded; only this final copy step failed, so re-running with correct permissions on $REPO_ROOT/bin should be all that's needed."
    [[ -x "$REPO_ROOT/bin/$name" ]] || die "install reported success but $REPO_ROOT/bin/$name still isn't there/executable — filesystem full? check df -h."
}

# release_lto_settings_for_this_device — echoes env-var assignments
# (CARGO_PROFILE_RELEASE_LTO=... CARGO_PROFILE_RELEASE_CODEGEN_UNITS=...) to
# eval before cargo build. Cargo.toml's committed [profile.release] uses
# opt-level=s, full LTO, and codegen-units=1 for the smallest/fastest edge
# binary — right for
# a well-resourced build machine/CI, but it means nearly all of the actual
# compiling (every dependency crate: tokio, kube, and with --features cri
# also tonic/prost/rustls) happens fine, and then the *entire* dependency
# graph gets merged into one codegen unit and LTO'd in a single rustc/LLVM
# process at the very end. That one process's memory use scales with the
# whole dependency graph, not any single crate.
#
# On a well-resourced machine that's just slow. On a genuinely
# memory-constrained device (confirmed for real on a ~2.8GB-RAM box) it can
# OOM-kill hard enough to take the whole host down with it, not just the
# rustc process — which means the ordinary "build fails, retry with lighter
# settings" fallback below never gets a chance to run at all, and whoever's
# running this script sees the box reboot mid-build with no diagnostic,
# left with whatever stale bin/nodelet happened to already be there
# (install_built_binary above at least makes that visible on the *next*
# successful run, but the crashed run itself gives no signal). Cheaper to
# just not attempt the risky profile at all below a conservative memory
# floor than to rely on a retry that a hard crash preempts.
release_lto_settings_for_this_device() {
    local total_kb
    total_kb="$(awk '/^MemTotal:/{print $2}' /proc/meminfo 2>/dev/null || echo 0)"
    # ~4GB floor: comfortably above the ~2.8GB device this was confirmed
    # on (where even the *lighter* thin-LTO/16-codegen-unit build peaked
    # under 1GB free), comfortably below any real build server/CI runner.
    if [[ "$total_kb" -gt 0 && "$total_kb" -lt 4194304 ]]; then
        echo "CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16"
    fi
}

# ─────────────────────────────────────────────────────────────────────────
# Build layouts
# ─────────────────────────────────────────────────────────────────────────
#
# Two of them (lib/components.sh documents the choice itself): `split`
# builds one binary per component, `combined` builds the single multi-call
# `notk8s` binary and symlinks each component name at it, `both` builds
# both. Everything below is written per-component off components.sh's table
# rather than per-binary, so a future component is a row there, not another
# copy of these blocks.

# link_component_binary <name> — point $REPO_ROOT/bin/<name> at the combined
# binary next to it. Relative target on purpose: bin/ gets moved/copied
# around (deploy.tar.gz, a device's install path), and an absolute symlink
# into this checkout would break the moment it does.
link_component_binary() {
    local name="$1"
    [[ -x "$REPO_ROOT/bin/notk8s" ]] || die "Combined binary $REPO_ROOT/bin/notk8s is missing — nothing to point bin/$name at."
    rm -f "$REPO_ROOT/bin/$name" 2>/dev/null
    ln -s notk8s "$REPO_ROOT/bin/$name" \
        || die "Couldn't create $REPO_ROOT/bin/$name -> notk8s symlink (permission denied? check ownership of $REPO_ROOT/bin)."
    log "bin/$name -> notk8s (combined layout; the binary dispatches on argv[0])"
}

# install_combined_layout — bin/notk8s plus a symlink per enabled component,
# so every consumer downstream (the service units, run-nodelet.sh,
# run-nodeproxy.sh, the e2e suite) keeps exec'ing bin/<component> and neither
# knows nor cares which layout is installed.
install_combined_layout() {
    local src="$1" name
    install_built_binary "$src" notk8s
    while read -r name; do
        link_component_binary "$name"
    done < <(enabled_components)
}

# use_prebuilt_binaries — install already-built binaries instead of
# compiling, if the caller supplied any. Echoes nothing; returns 0 if it
# handled the whole build, 1 if there's still compiling to do.
#
# NOTK8S_COMBINED_PREBUILT takes precedence: it's a complete answer on its
# own (one file containing every component), so a caller that sets it isn't
# asked for the per-component ones too.
use_prebuilt_binaries() {
    if [[ -n "${NOTK8S_COMBINED_PREBUILT:-}" ]]; then
        log "Using prebuilt combined binary: $NOTK8S_COMBINED_PREBUILT"
        [[ -x "$NOTK8S_COMBINED_PREBUILT" ]] || die "NOTK8S_COMBINED_PREBUILT is set but not an executable file: $NOTK8S_COMBINED_PREBUILT"
        install_combined_layout "$NOTK8S_COMBINED_PREBUILT"
        return 0
    fi

    # "Did the caller supply ANY per-component prebuilt?" — asked of the
    # component table, not of nodelet specifically. A future component
    # supplied prebuilt on its own has to reach the mixing error below
    # rather than silently falling through to a from-source build.
    local name row var path any=0
    while read -r name; do
        var="$(component_field "$(component_row "$name")" 5)"
        [[ -n "${!var:-}" ]] && any=1
    done < <(enabled_components)
    [[ "$any" -eq 1 ]] || return 1

    while read -r name; do
        row="$(component_row "$name")"
        var="$(component_field "$row" 5)"
        path="${!var:-}"
        [[ -n "$path" ]] \
            || die "$var isn't set, but this run wants the '$name' component and another component was supplied prebuilt (mixing prebuilt and from-source builds isn't supported — a partial set is far more likely to be an oversight than a request). Set $var too, or drop the component (e.g. --proxy=none for nodeproxy), or set NOTK8S_COMBINED_PREBUILT for a single binary containing everything."
        log "Using prebuilt $name binary: $path"
        [[ -x "$path" ]] || die "$var is set but not an executable file: $path"
        install_built_binary "$path" "$name"
    done < <(enabled_components)
    return 0
}

# LTO_OVERRIDE — env-var assignments prepended to every cargo build in this
# run (empty on a well-resourced machine). Seeded from this device's memory
# before the first build and, if a full-profile build fails anyway, latched
# to the lighter settings for every build afterwards rather than
# re-discovering the same failure once per component/layout.
LTO_OVERRIDE=""
LTO_FALLBACK="CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16"

# cargo_build_component <profile-flag> <cargo args...> — one `cargo build`,
# with the memory-constrained-device LTO override applied and the one
# documented retry on top. Every build in this file goes through here so the
# split and combined paths can't drift on how they invoke cargo or on how
# they recover.
cargo_build_component() {
    local profile_flag="$1"; shift
    # shellcheck disable=SC2086  # both are deliberately word-split
    env $LTO_OVERRIDE cargo build $profile_flag "$@" && return 0

    # Only the committed release profile has the failure mode below to
    # retry, and only if it hasn't already been backed off.
    [[ "$profile_flag" == "--release" && -z "$LTO_OVERRIDE" ]] || return 1

    # Confirmed for real: this is the actual failure being retried here —
    # see release_lto_settings_for_this_device()'s comment for why a
    # big-enough device still tries the expensive profile first.
    warn "cargo build --release failed — if this device is memory-constrained, the likely cause is the final whole-program LTO step (Cargo.toml's [profile.release] uses opt-level=s, full LTO, and codegen-units=1 for the smallest edge binary, which needs the most memory right at the end). Retrying once with lighter LTO settings (thin LTO, 16 codegen units) that trade a slightly larger binary for much lower peak memory..."
    # Whatever made the full profile fail applies to every later build in
    # this run too, so latch it rather than re-discovering it per component.
    LTO_OVERRIDE="$LTO_FALLBACK"
    # shellcheck disable=SC2086
    env $LTO_OVERRIDE cargo build $profile_flag "$@"
}

build_split_layout() {
    local profile_flag="$1" out_dir="$2" target="$3"
    local name features
    while read -r name; do
        features="$(component_cargo_features "$name")"
        log "Building $name (cargo build ${profile_flag:-} -p $name ${features:-})..."
        # shellcheck disable=SC2086  # features is a flag string to split
        cargo_build_component "$profile_flag" -p "$name" $features --target "$target" \
            || die "cargo build -p $name failed — check $LOG_DIR.$([[ -n "$LTO_OVERRIDE" ]] && echo " This was already the lighter-LTO settings ($LTO_OVERRIDE); if this is memory exhaustion (dmesg will show an oom-kill of rustc/cc1plus/ld), adding swap or building on a bigger box are the remaining options.")"
        [[ -x "$out_dir/$name" ]] || die "Build finished but $out_dir/$name isn't there."
    done < <(enabled_components)
}

build_combined_layout() {
    local profile_flag="$1" out_dir="$2" target="$3"
    local features
    features="$(combined_cargo_features)"
    log "Building combined binary (cargo build ${profile_flag:-} -p notk8s ${features:-})..."
    # shellcheck disable=SC2086  # features is a flag string to split
    cargo_build_component "$profile_flag" -p notk8s $features --target "$target" \
        || die "cargo build -p notk8s failed — check $LOG_DIR. This is the combined single-binary layout; NOTK8S_BUILD_LAYOUT=split builds one binary per component instead."
    [[ -x "$out_dir/notk8s" ]] || die "Build finished but $out_dir/notk8s isn't there."
}

# install_layout_output — copy the freshly built binaries out of target/ to
# bin/ before the end-of-run cleanup wipes the whole build cache. `both`
# builds everything but installs the split binaries as the live ones (least
# surprise vs. every previous release), keeping the combined binary
# alongside at bin/notk8s for packaging.
install_layout_output() {
    local layout="$1" out_dir="$2" name
    if layout_installs_combined "$layout"; then
        install_combined_layout "$out_dir/notk8s"
        log "Combined binary installed: $REPO_ROOT/bin/notk8s ($(du -h "$REPO_ROOT/bin/notk8s" 2>/dev/null | cut -f1))"
        return 0
    fi
    while read -r name; do
        install_built_binary "$out_dir/$name" "$name"
        log "$name built: $REPO_ROOT/bin/$name"
    done < <(enabled_components)
    if layout_builds_combined "$layout"; then
        install_built_binary "$out_dir/notk8s" notk8s
        log "Combined binary also built (not installed as the running one — NOTK8S_BUILD_LAYOUT=combined does that): $REPO_ROOT/bin/notk8s"
    elif [[ -e "$REPO_ROOT/bin/notk8s" ]]; then
        # Left by an earlier combined run. Not a dispatch hazard (the
        # run-*.sh fallback only looks at it when bin/<component> is
        # missing, and this run just wrote those), but leaving a stale
        # binary of unknown vintage on the device is how the footprint
        # report ends up counting something this run never built.
        rm -f "$REPO_ROOT/bin/notk8s"
        log "Removed a stale $REPO_ROOT/bin/notk8s left by an earlier combined-layout run."
    fi
}

# build_nodelet — despite the name (kept: every entry point and doc calls
# it), this builds *every* component this run wants, in whichever layout(s)
# NOTK8S_BUILD_LAYOUT asks for.
build_nodelet() {
    local layout target
    layout="$(resolve_build_layout)"
    target="$(RUSTUP_TARGET_MAP)"
    [[ -n "$target" ]] || die "No supported static musl Rust target for arch '$ARCH' — refusing to build a glibc-linked binary."

    # A prebuilt drop-in decides the layout by itself — you can't assemble a
    # combined binary out of per-component ones, or split a combined one
    # back apart. Say so rather than quietly installing the other layout:
    # the whole point of asking for one is the resulting footprint.
    if layout_installs_combined "$layout" && [[ -z "${NOTK8S_COMBINED_PREBUILT:-}" && -n "${NOTK8S_NODELET_PREBUILT:-}" ]]; then
        die "--layout=combined was requested, but the prebuilt binaries supplied are the per-component ones (NOTK8S_NODELET_PREBUILT/...). A combined binary has to be built as one — set NOTK8S_COMBINED_PREBUILT to a prebuilt 'notk8s' instead (bootstrap-release.sh --layout=combined fetches one), or drop --layout=combined to install the per-component binaries you already have."
    fi

    # ...and the mirror image: a combined binary can't be taken apart into
    # per-component ones either.
    if ! layout_builds_combined "$layout" && [[ -n "${NOTK8S_COMBINED_PREBUILT:-}" ]]; then
        die "NOTK8S_COMBINED_PREBUILT is set (a single binary containing every component), but this run's layout is '$layout', which installs one binary per component. Pass --layout=combined to install it as intended, or supply the per-component prebuilts (NOTK8S_NODELET_PREBUILT/...) instead."
    fi

    if use_prebuilt_binaries; then
        [[ "$layout" == "both" ]] \
            && warn "--layout=both asked for both layouts, but this run is installing prebuilt binaries and only builds what it was given — the ones supplied are what's installed."
        return 0
    fi

    cd "$REPO_ROOT"
    # Not `if [[ $WITH_CRI -eq 1 ]]` any more: protoc is needed by whichever
    # enabled components compile .proto files, which since nodestore is no
    # longer the same question as "is the real container runtime wanted".
    if any_component_needs_protoc; then
        ensure_protoc
    fi

    log "Build layout: $layout ($(enabled_components | tr '\n' ' ')|combined=$(layout_builds_combined "$layout" && echo yes || echo no))"

    # NOTK8S_BUILD_PROFILE=debug skips the optimized release profile
    # entirely (Cargo.toml's opt-level=s/full-LTO/codegen-units=1 is what makes a
    # release build take ~5 minutes vs. cargo test's ~1 — worthwhile for
    # an actual deployed binary, pure waste for e2e testing, which only
    # needs correctness, not runtime performance). Not the default: a real
    # device install should still get the real, optimized binary.
    local out_dir profile_flag=--release
    if [[ "${NOTK8S_BUILD_PROFILE:-release}" == "debug" ]]; then
        profile_flag=""
        out_dir="$REPO_ROOT/target/$target/debug"
    else
        out_dir="$REPO_ROOT/target/$target/release"
        LTO_OVERRIDE="$(release_lto_settings_for_this_device)"
        [[ -n "$LTO_OVERRIDE" ]] \
            && log "This device has under 4GB RAM — building with lighter LTO settings ($LTO_OVERRIDE) from the start instead of risking the full opt-level=s/full-LTO/codegen-units=1 profile's memory spike."
    fi

    layout_builds_split "$layout" && build_split_layout "$profile_flag" "$out_dir" "$target"
    layout_builds_combined "$layout" && build_combined_layout "$profile_flag" "$out_dir" "$target"
    install_layout_output "$layout" "$out_dir"
}
