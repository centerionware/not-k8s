# lib/toolchain-go.sh — Go, only needed to build containerd/runc/CNI
# plugins/flannel from source (no prebuilt release for this arch). Same
# tiering as everything else: package manager -> official prebuilt ->
# from-source bootstrap.
#
# NOTE: this whole file becomes optional per-component once GitHub Actions
# cross-builds containerd/runc/CNI/flannel centrally (see container-
# runtime.sh and cni.sh) — an on-device install only needs ensure_go() at
# all when no prebuilt exists for its arch, same seam as toolchain-rust.sh.

GO_VERSION=1.22.6

go_arch_map() {
    case "$ARCH" in
        x86_64)   echo amd64 ;;
        aarch64)  echo arm64 ;;
        armv7l)   echo armv6l ;;
        armv6l)   echo armv6l ;;
        i686)     echo 386 ;;
        riscv64)  echo riscv64 ;;
        ppc64le)  echo ppc64le ;;
        s390x)    echo s390x ;;
        loongarch64) echo loong64 ;;
        *)        echo "" ;;
    esac
}

go_is_new_enough() {
    command -v go &>/dev/null || return 1
    local minor; minor="$(go version | sed -n 's/.*go1\.\([0-9]\+\).*/\1/p')"
    [[ "$minor" =~ ^[0-9]+$ ]] || return 1
    (( minor >= 21 ))
}

ensure_go() {
    if go_is_new_enough; then
        log "Go present and new enough: $(go version)"
        return 0
    fi
    command -v go &>/dev/null && warn "Found $(go version) but containerd/runc need >=1.21 — looking for a newer one."

    pkg_install "go" "golang-go" "golang" "go" "go" "go" "go" \
        && go_is_new_enough && { log "Go installed via $PKG_MGR: $(go version)"; return 0; }

    local goarch; goarch="$(go_arch_map)"
    if [[ -n "$goarch" ]]; then
        log "Fetching official Go $GO_VERSION release for linux-$goarch..."
        local tarball="go$GO_VERSION.linux-$goarch.tar.gz"
        if fetch "https://go.dev/dl/$tarball" "$SRC_DIR/$tarball"; then
            rm -rf "$TOOLCHAIN_DIR/go"
            tar xzf "$SRC_DIR/$tarball" -C "$TOOLCHAIN_DIR"
            ln -sf "$TOOLCHAIN_DIR/go/bin/go" "$TOOLCHAIN_DIR/bin/go"
            go_is_new_enough && { log "Go ready: $(go version)"; return 0; }
        fi
    fi

    build_go_from_source
}

# Go's own documented from-source bootstrap: Go 1.4 (the last C-implemented
# release) builds with just a C compiler; that becomes GOROOT_BOOTSTRAP for
# an intermediate Go version, because Go >=1.21 refuses to bootstrap from
# anything older than Go 1.20; that intermediate Go then builds the final
# target version. Slow (three full Go builds) but has no binary dependency
# beyond a C compiler.
build_go_from_source() {
    log "No prebuilt Go for $ARCH — bootstrapping Go from source (three stages, slow)."
    command -v cc &>/dev/null || die "Need a C compiler to bootstrap Go and none is available."

    cd "$SRC_DIR"
    if [[ ! -x go-bootstrap-c/bin/go ]]; then
        fetch "https://dl.google.com/go/go1.4-bootstrap-20171003.tar.gz" go1.4-bootstrap.tar.gz
        rm -rf go-bootstrap-c; mkdir go-bootstrap-c
        tar xzf go1.4-bootstrap.tar.gz -C go-bootstrap-c --strip-components=1
        ( cd go-bootstrap-c/src && CGO_ENABLED=0 ./make.bash )
    fi

    local MID_VER=1.20.14
    if [[ ! -x "go-$MID_VER/bin/go" ]]; then
        fetch "https://go.dev/dl/go$MID_VER.src.tar.gz" "go$MID_VER.src.tar.gz"
        rm -rf "go-$MID_VER"; mkdir "go-$MID_VER"
        tar xzf "go$MID_VER.src.tar.gz" -C "go-$MID_VER" --strip-components=1
        ( export GOROOT_BOOTSTRAP="$SRC_DIR/go-bootstrap-c"; cd "go-$MID_VER/src" && ./make.bash )
    fi

    if [[ ! -x "go-$GO_VERSION/bin/go" ]]; then
        fetch "https://go.dev/dl/go$GO_VERSION.src.tar.gz" "go$GO_VERSION.src.tar.gz"
        rm -rf "go-$GO_VERSION"; mkdir "go-$GO_VERSION"
        tar xzf "go$GO_VERSION.src.tar.gz" -C "go-$GO_VERSION" --strip-components=1
        ( export GOROOT_BOOTSTRAP="$SRC_DIR/go-$MID_VER"; cd "go-$GO_VERSION/src" && ./make.bash )
    fi

    ln -sf "$SRC_DIR/go-$GO_VERSION/bin/go" "$TOOLCHAIN_DIR/bin/go"
    go_is_new_enough || die "Go source bootstrap finished but 'go' is still not usable."
    log "Go built from source: $(go version)"
}
