# lib/common.sh — logging, platform detection, and the fetch/pkg_install
# primitives everything else in deploy/lib is built on. Sourced first, by
# bootstrap-source.sh, before any other lib/*.sh file.
#
# Expects these globals to already be set by the caller before use:
#   WORK_DIR, LOG_DIR, SUDO, PKG_MGR, FORCE_SOURCE_BUILD

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*" >&2; }
warn() { printf '\033[1;33m==> WARNING:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m==> FATAL:\033[0m %s\n' "$*" >&2; exit 1; }
# Exported so callers that need to run one of these inside a `bash -c "..."`
# (a genuinely separate bash process, not just a subshell) still have them
# — see lib/test/k8s.sh's own export -f block for the fuller story of why
# this matters.
export -f log warn die

# Sets ARCH_RAW, ARCH, PKG_MGR, IS_ROOT, SUDO. Call once, early.
detect_platform() {
    OS="$(uname -s)"
    [[ "$OS" == "Linux" ]] || die "This script targets Linux only (found: $OS)."

    ARCH_RAW="$(uname -m)"
    case "$ARCH_RAW" in
        x86_64|amd64)           ARCH=x86_64 ;;
        aarch64|arm64)          ARCH=aarch64 ;;
        armv7l|armv7)           ARCH=armv7l ;;
        armv6l)                 ARCH=armv6l ;;
        i686|i386)              ARCH=i686 ;;
        riscv64)                ARCH=riscv64 ;;
        ppc64le)                ARCH=ppc64le ;;
        s390x)                  ARCH=s390x ;;
        loongarch64)            ARCH=loongarch64 ;;
        *)                      ARCH="$ARCH_RAW" ;;
    esac

    PKG_MGR=unknown
    if   command -v apt-get &>/dev/null; then PKG_MGR=apt
    elif command -v dnf     &>/dev/null; then PKG_MGR=dnf
    elif command -v yum     &>/dev/null; then PKG_MGR=yum
    elif command -v pacman  &>/dev/null; then PKG_MGR=pacman
    elif command -v apk     &>/dev/null; then PKG_MGR=apk
    elif command -v zypper  &>/dev/null; then PKG_MGR=zypper
    elif command -v xbps-install &>/dev/null; then PKG_MGR=xbps
    elif command -v emerge  &>/dev/null; then PKG_MGR=emerge
    elif command -v pkg     &>/dev/null; then PKG_MGR=pkg
    fi

    IS_ROOT=0
    [[ "$EUID" -eq 0 ]] && IS_ROOT=1
    SUDO=""
    if [[ "$IS_ROOT" -eq 0 ]]; then
        command -v sudo &>/dev/null && SUDO="sudo"
    fi
}

# ─────────────────────────────────────────────────────────────────────────
# IP family resolution — decided once, and handed to every consumer
# (k3s's --cluster-cidr/--service-cidr, flannel's net-conf, and nodelet's
# NODELET_IP_FAMILY) so they can't disagree with each other.
#
# Checks for an actual default route in each family — deliberately the same
# thing flannel itself checks when picking its interface ("failed to get
# default v6 interface: Unable to find default v6 route"). An earlier
# version of this used a UDP socket *bind* test instead, which only proves
# the address family's socket API works — nearly every modern kernel has
# IPv6 compiled in and enabled regardless of whether there's any real IPv6
# connectivity, so that test passed on a machine with no default v6 route
# at all. Result, confirmed for real: auto picked "dual", flannel got a
# net-conf.json telling it to also handle IPv6, and it crash-looped forever
# since it could never find a v6 interface to use. A route-table check is
# the actual thing that matters, and it's exactly what was missing.
# ─────────────────────────────────────────────────────────────────────────

detect_ipv4() {
    if command -v ip &>/dev/null; then
        ip -4 route show default 2>/dev/null | grep -q .
    else
        # No iproute2 to check a real route table. IPv4 is near-universal,
        # so a bind test (proves the socket API works, not real
        # connectivity — weaker, but the false-positive direction here is
        # low-risk) is a reasonable fallback.
        command -v python3 &>/dev/null \
            && python3 -c 'import socket; socket.socket(socket.AF_INET, socket.SOCK_DGRAM).bind(("0.0.0.0", 0))' 2>/dev/null
    fi
}
detect_ipv6() {
    if command -v ip &>/dev/null; then
        ip -6 route show default 2>/dev/null | grep -q .
    else
        # No route table to check, and unlike IPv4, a false positive here
        # is exactly what caused flannel to crash-loop — default to "no
        # v6" rather than trust a weaker test. Worst case is ipv4-only when
        # dual-stack would actually have worked; that's a far better
        # failure mode than a service that can never start.
        false
    fi
}

# Sets IP_FAMILY (resolving "auto"), CLUSTER_CIDR, SERVICE_CIDR, and exports
# NOTK8S_CLUSTER_CIDR/NOTK8S_SERVICE_CIDR. Reads/writes the global IP_FAMILY.
resolve_ip_family() {
    case "$IP_FAMILY" in
        auto)
            local v4=0 v6=0
            detect_ipv4 && v4=1
            detect_ipv6 && v6=1
            if [[ "$v4" -eq 1 && "$v6" -eq 1 ]]; then IP_FAMILY=dual
            elif [[ "$v6" -eq 1 ]]; then IP_FAMILY=ipv6
            else IP_FAMILY=ipv4
            fi
            ;;
        ipv4|ipv6|dual) ;;
        *) die "Unknown --ip-family='$IP_FAMILY' (want 'auto', 'ipv4', 'ipv6', or 'dual')." ;;
    esac
    log "IP family: $IP_FAMILY"

    # CIDRs match Rancher's own documented k3s dual-stack example, so anyone
    # comparing against upstream k3s docs sees familiar numbers.
    IPV4_CLUSTER_CIDR="10.42.0.0/16"
    IPV4_SERVICE_CIDR="10.43.0.0/16"
    IPV6_CLUSTER_CIDR="fd00:42::/48"
    IPV6_SERVICE_CIDR="fd00:43::/112"
    case "$IP_FAMILY" in
        ipv4) CLUSTER_CIDR="$IPV4_CLUSTER_CIDR"; SERVICE_CIDR="$IPV4_SERVICE_CIDR" ;;
        ipv6) CLUSTER_CIDR="$IPV6_CLUSTER_CIDR"; SERVICE_CIDR="$IPV6_SERVICE_CIDR" ;;
        dual) CLUSTER_CIDR="$IPV4_CLUSTER_CIDR,$IPV6_CLUSTER_CIDR"; SERVICE_CIDR="$IPV4_SERVICE_CIDR,$IPV6_SERVICE_CIDR" ;;
    esac
    export NOTK8S_CLUSTER_CIDR="$CLUSTER_CIDR" NOTK8S_SERVICE_CIDR="$SERVICE_CIDR"
}

# apt-get has its own per-connection timeouts, but those do not cover every
# way an apt invocation can wedge (for example, a stalled child process or a
# package-manager lock). Keep the whole operation bounded as well. The value
# is intentionally configurable for slow edge links, while the default is
# short enough that a bootstrap can get to its fallback mirror promptly.
_apt_timeout_seconds() {
    local timeout_seconds="${NOTK8S_APT_TIMEOUT_SECONDS:-120}"
    if [[ ! "$timeout_seconds" =~ ^[1-9][0-9]*$ ]]; then
        warn "Ignoring invalid NOTK8S_APT_TIMEOUT_SECONDS='$timeout_seconds'; using 120 seconds."
        timeout_seconds=120
    fi
    printf '%s\n' "$timeout_seconds"
}

# Rewrite only the distribution mirrors we know how to fail over. The
# replacement is deliberately done on a temporary copy of the source files;
# a bootstrap must not modify the host's apt configuration just because one
# mirror was unavailable.
_apt_rewrite_sources() {
    local source_file="$1"
    sed \
        -e 's#azure\.archive\.ubuntu\.com#notk8s-alt-ubuntu#g' \
        -e 's#archive\.ubuntu\.com#azure.archive.ubuntu.com#g' \
        -e 's#notk8s-alt-ubuntu#archive.ubuntu.com#g' \
        -e 's#security\.ubuntu\.com#archive.ubuntu.com#g' \
        -e 's#deb\.debian\.org#notk8s-alt-debian#g' \
        -e 's#security\.debian\.org#notk8s-alt-debian#g' \
        -e 's#notk8s-alt-debian#ftp.de.debian.org#g' \
        "$source_file"
}

# Echo a temporary apt source-parts directory when a known mirror was
# rewritten, or return 1 when this host uses a source format/mirror for which
# we do not have a safe alternate. Handles both classic .list files and the
# deb822 .sources format used by current Ubuntu runners.
_apt_alternate_sources() {
    local base_dir="${WORK_DIR:-}"
    [[ -n "$base_dir" && -d "$base_dir" ]] || return 1

    local alternate_dir source_file alternate_file changed=0
    alternate_dir="$(mktemp -d "$base_dir/apt-sources.XXXXXX")" || return 1
    : > "$alternate_dir/sources.list"

    if [[ -f /etc/apt/sources.list ]]; then
        _apt_rewrite_sources /etc/apt/sources.list > "$alternate_dir/sources.list" \
            || { rm -rf -- "$alternate_dir"; return 1; }
        cmp -s /etc/apt/sources.list "$alternate_dir/sources.list" || changed=1
    fi

    for source_file in /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; do
        [[ -f "$source_file" ]] || continue
        alternate_file="$alternate_dir/$(basename "$source_file")"
        _apt_rewrite_sources "$source_file" > "$alternate_file" \
            || { rm -rf -- "$alternate_dir"; return 1; }
        cmp -s "$source_file" "$alternate_file" || changed=1
    done

    if [[ "$changed" -eq 0 ]]; then
        rm -rf -- "$alternate_dir"
        return 1
    fi
    printf '%s\n' "$alternate_dir"
}

# _apt_run <source-dir-or-empty> <update|install> <package-spec> <timeout>
# Keep this in one helper so update and install get exactly the same network
# and process-level bounds. The package spec is the space-separated form used
# by all existing pkg_install call sites.
_apt_run() {
    local source_dir="$1" action="$2" package_spec="$3" timeout_seconds="$4"
    local -a timeout_cmd=() sudo_cmd=() apt_options=() package_args=()

    if command -v timeout &>/dev/null; then
        timeout_cmd=(timeout --signal=TERM --kill-after=10s "${timeout_seconds}s")
    else
        # The apt Acquire timeouts below still bound network connections, but
        # without coreutils' timeout command a non-network apt hang cannot be
        # interrupted. All supported apt environments normally have it.
        warn "'timeout' is unavailable; apt network operations will use per-connection bounds only."
    fi
    [[ -n "$SUDO" ]] && sudo_cmd=("$SUDO")
    apt_options=(
        -o Acquire::Retries=1
        -o Acquire::http::Timeout=20
        -o Acquire::https::Timeout=20
        -o DPkg::Lock::Timeout=20
    )
    if [[ -n "$source_dir" ]]; then
        apt_options+=(
            -o "Dir::Etc::sourcelist=$source_dir/sources.list"
            -o "Dir::Etc::sourceparts=$source_dir"
        )
    fi

    case "$action" in
        update)
            "${timeout_cmd[@]}" "${sudo_cmd[@]}" apt-get \
                "${apt_options[@]}" update -qq -y >>"$LOG_DIR/pkg.log" 2>&1
            ;;
        install)
            read -r -a package_args <<< "$package_spec"
            "${timeout_cmd[@]}" "${sudo_cmd[@]}" apt-get \
                "${apt_options[@]}" install -qq -y "${package_args[@]}" \
                >>"$LOG_DIR/pkg.log" 2>&1
            ;;
        *)
            return 2
            ;;
    esac
}

# pkg_install <logical-name> <apt-pkg> <dnf-pkg> <pacman-pkg> <apk-pkg> <zypper-pkg> <xbps-pkg>
# Best-effort: returns 0 on apparent success, 1 if no package manager could be used.
# Every successful install is appended to $WORK_DIR/pkg_installs.log as
# "<pkgmgr>|<logical-name>|<packages>" — this is what makes the end-of-run
# footprint cleanup possible: only packages *this script* actually installed
# get recorded, so cleanup can uninstall exactly those and leave anything
# that was already on the system untouched. Never logs failures.
pkg_install() {
    local name="$1" apt="$2" dnf="$3" pacman="$4" apk="$5" zypper="$6" xbps="$7"
    [[ "$FORCE_SOURCE_BUILD" -eq 1 ]] && return 1
    log "Trying to install '$name' via $PKG_MGR..."
    local pkgs="" ok=1
    case "$PKG_MGR" in
        apt)
            pkgs="$apt"
            local apt_timeout first_status=0 alt_sources=""
            apt_timeout="$(_apt_timeout_seconds)"
            if _apt_run "" update "" "$apt_timeout" \
                && _apt_run "" install "$apt" "$apt_timeout"; then
                ok=0
            else
                first_status=$?
                if [[ "$first_status" -eq 124 ]]; then
                    warn "apt '$name' timed out after ${apt_timeout}s; retrying with an alternate mirror."
                else
                    warn "apt '$name' failed (exit $first_status); retrying with an alternate mirror."
                fi
                if alt_sources="$(_apt_alternate_sources)"; then
                    log "Retrying apt '$name' with an alternate distribution mirror."
                    if _apt_run "$alt_sources" update "" "$apt_timeout" \
                        && _apt_run "$alt_sources" install "$apt" "$apt_timeout"; then
                        ok=0
                    fi
                    rm -rf -- "$alt_sources"
                else
                    warn "No supported alternate apt mirror was found; continuing with the normal fallback path."
                fi
            fi
            ;;
        dnf)    pkgs="$dnf";    $SUDO dnf install -y -q $dnf >>"$LOG_DIR/pkg.log" 2>&1 && ok=0 || ok=1 ;;
        yum)    pkgs="$dnf";    $SUDO yum install -y -q $dnf >>"$LOG_DIR/pkg.log" 2>&1 && ok=0 || ok=1 ;;
        pacman) pkgs="$pacman"; $SUDO pacman -Sy --noconfirm --needed $pacman >>"$LOG_DIR/pkg.log" 2>&1 && ok=0 || ok=1 ;;
        apk)    pkgs="$apk";    $SUDO apk add --no-cache $apk >>"$LOG_DIR/pkg.log" 2>&1 && ok=0 || ok=1 ;;
        zypper) pkgs="$zypper"; $SUDO zypper --non-interactive install $zypper >>"$LOG_DIR/pkg.log" 2>&1 && ok=0 || ok=1 ;;
        xbps)   pkgs="$xbps";   $SUDO xbps-install -Sy $xbps >>"$LOG_DIR/pkg.log" 2>&1 && ok=0 || ok=1 ;;
        *)      return 1 ;;
    esac
    [[ "$ok" -eq 0 ]] && echo "$PKG_MGR|$name|$pkgs" >> "$WORK_DIR/pkg_installs.log"
    return "$ok"
}

remove_pkgs_via_mgr() {
    local mgr="$1" pkgs="$2"
    case "$mgr" in
        apt)    $SUDO apt-get remove -y -qq $pkgs >>"$LOG_DIR/pkg.log" 2>&1 ;;
        dnf)    $SUDO dnf remove -y -q $pkgs >>"$LOG_DIR/pkg.log" 2>&1 ;;
        yum)    $SUDO yum remove -y -q $pkgs >>"$LOG_DIR/pkg.log" 2>&1 ;;
        pacman) $SUDO pacman -Rns --noconfirm $pkgs >>"$LOG_DIR/pkg.log" 2>&1 ;;
        apk)    $SUDO apk del $pkgs >>"$LOG_DIR/pkg.log" 2>&1 ;;
        zypper) $SUDO zypper --non-interactive remove $pkgs >>"$LOG_DIR/pkg.log" 2>&1 ;;
        xbps)   $SUDO xbps-remove -Ry $pkgs >>"$LOG_DIR/pkg.log" 2>&1 ;;
    esac
}

fetch() { # fetch <url> <output-path>
    if command -v curl &>/dev/null; then
        curl -fsSL --retry 3 -o "$2" "$1"
    elif command -v wget &>/dev/null; then
        wget -q -O "$2" "$1"
    else
        die "Neither curl nor wget is available, and none could be installed."
    fi
}

ensure_fetch_tool() {
    # curl/wget themselves have to come from *somewhere* — every distro
    # package manager ships one of them, so this is the one dependency we
    # require the package manager (or the base image) to already provide.
    if ! command -v curl &>/dev/null && ! command -v wget &>/dev/null; then
        pkg_install curl curl curl curl curl curl curl || true
    fi
    command -v curl &>/dev/null || command -v wget &>/dev/null \
        || die "No curl/wget and no usable package manager — cannot fetch anything. Install curl manually first."
}
