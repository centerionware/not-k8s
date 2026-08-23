# component_discovery.sh — locate and identify the components under test.
#
# nodebootstrap stages applets in its own toolchain directory, while the
# legacy shell e2e suite looked only in the checkout. These helpers keep
# both deployment paths covered.

test_component_running() {
    local name="$1"
    if command -v systemctl >/dev/null 2>&1; then
        systemctl is-active --quiet "$name.service" 2>/dev/null && return 0
    fi
    pgrep -x "$name" >/dev/null 2>&1
}

test_component_binary() {
    local name="$1"
    local upper="${name^^}"
    local override_var="NOTK8S_E2E_${upper}_BIN"
    local override="${!override_var:-}"
    local candidate service_path toolchain_dir

    if [[ -n "$override" && -x "$override" ]]; then
        printf '%s\n' "$override"
        return 0
    fi

    if command -v systemctl >/dev/null 2>&1; then
        # nodebootstrap publishes this stable metadata variable on every
        # component unit. It is preferred because it also works with a
        # non-default NODEBOOTSTRAP_TOOLCHAIN_DIR.
        local service_env
        service_env="$(systemctl show "$name.service" -p Environment --value 2>/dev/null || true)"
        if [[ "$service_env" == *NOTK8S_COMPONENT_BINARY=* ]]; then
            service_path="${service_env#*NOTK8S_COMPONENT_BINARY=}"
            service_path="${service_path%% *}"
            if [[ -x "$service_path" ]]; then
                printf '%s\n' "$service_path"
                return 0
            fi
        fi

        # Preserve a combined-layout component symlink so argv[0] dispatch
        # still works for callers that launch the returned path.
        service_path="$(systemctl cat "$name.service" 2>/dev/null \
            | grep -oE "/[^[:space:]']+/${name}" \
            | head -1 || true)"
        if [[ -n "$service_path" && -x "$service_path" ]]; then
            printf '%s\n' "$service_path"
            return 0
        fi
    fi

    toolchain_dir="${NODEBOOTSTRAP_TOOLCHAIN_DIR:-/var/lib/nodebootstrap/toolchain}"
    local -a candidates=(
        "$toolchain_dir/bin/$name"
        "$REPO_ROOT/bin/$name"
        "$REPO_ROOT/target/release/$name"
        "$REPO_ROOT/target/debug/$name"
        "/usr/local/bin/$name"
    )
    for candidate in "${candidates[@]}"; do
        if [[ -x "$candidate" ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done

    candidate="$(command -v "$name" 2>/dev/null || true)"
    if [[ -n "$candidate" && -x "$candidate" ]]; then
        printf '%s\n' "$candidate"
        return 0
    fi
    return 1
}

test_controller_manager_is_exclusive() {
    local k3s_args upstream="kube-controller-manager"

    if command -v systemctl >/dev/null 2>&1; then
        systemctl is-active --quiet "$upstream.service" 2>/dev/null && return 1
        if systemctl is-active --quiet k3s.service 2>/dev/null; then
            k3s_args="$(systemctl show k3s.service -p ExecStart --value 2>/dev/null || true)"
            [[ "$k3s_args" == *--disable-controller-manager* ]] || return 1
            return 0
        fi
    fi

    pgrep -x "$upstream" >/dev/null 2>&1 && return 1
    k3s_args="$(ps -eo args= 2>/dev/null | grep -E '[k]3s( server)?' || true)"
    if [[ -n "$k3s_args" ]]; then
        [[ "$k3s_args" == *--disable-controller-manager* ]] || return 1
    fi
    return 0
}

test_control_plane_unit() {
    command -v systemctl >/dev/null 2>&1 || return 1
    local unit
    for unit in kube-apiserver.service k3s.service; do
        if systemctl cat "$unit" >/dev/null 2>&1; then
            printf '%s\n' "$unit"
            return 0
        fi
    done
    return 1
}
