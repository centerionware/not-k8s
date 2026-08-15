# lib/components.sh — the one list of not-k8s node components the shell side
# knows about, and the two build layouts they can be produced in.
#
# There are two places a component has to be declared: `crates/notk8s`'s
# Cargo.toml + APPLETS table (the Rust side), and the table below (the deploy
# side). Adding `nodeapiserver` / `nodescheduler` later is one row here, not
# an edit to every function that loops over components — which is exactly
# what nodelet-build.sh's per-component `if want_nodeproxy` special-casing
# was turning into.
#
# Row format (pipe-separated, no spaces around the pipes):
#
#   <name>|<cargo package>|<cargo features>|<enabled-predicate>|<prebuilt env var>|<needs-protoc>
#
#   name             installed binary name, and the applet name the combined
#                    binary dispatches on (argv[0]) — they must match, that's
#                    what makes the combined layout a drop-in.
#   cargo package    -p argument for a split build.
#   cargo features   feature flags for a split build of that package, or
#                    empty. `@cri` expands to `--features cri` only when this
#                    run wants the real container runtime (WITH_CRI=1) and to
#                    nothing otherwise.
#   predicate        shell function returning 0 when this run wants the
#                    component at all. Defined below.
#   prebuilt env var lets a caller supply an already-built binary instead of
#                    compiling (the seam CI's e2e shards and the
#                    build-in-CI/test-on-device loop use — see CLAUDE.md).
#   needs protoc     "protoc" when this component compiles .proto files at
#                    build time, empty otherwise. Used to decide whether to
#                    install protoc at all — it used to be inferable from
#                    --with-cri alone, until nodestore (whose etcd protos are
#                    not optional) made that no longer true.
NOTK8S_COMPONENTS=(
    # nodelet only needs protoc under --with-cri, which is what @cri
    # resolves to; the protoc column is therefore also conditional for it,
    # handled by component_needs_protoc below.
    "nodelet|nodelet|@cri|want_nodelet|NOTK8S_NODELET_PREBUILT|@cri"
    "nodeproxy|nodeproxy||want_nodeproxy|NOTK8S_NODEPROXY_PREBUILT|"
    "nodestore|nodestore||want_nodestore|NOTK8S_NODESTORE_PREBUILT|protoc"
    "nodescheduler|nodescheduler||want_nodescheduler|NOTK8S_NODESCHEDULER_PREBUILT|"
    "nodecontroller|nodecontroller||want_nodecontroller|NOTK8S_NODECONTROLLER_PREBUILT|"
)

# Whether this run wants the node agent. --skip-nodelet is handled by the
# entry point before build_nodelet() is called at all, so this is
# unconditional today; it exists so every component is asked the same
# question rather than one of them being implicitly always-on.
want_nodelet() { return 0; }

# Whether this run wants the Service proxy at all. PROXY is set by the
# bootstrap entry points (--proxy=nodeproxy|none); default to building it,
# since that matches the behaviour of every release before the split, when
# nodelet did this job in-process.
want_nodeproxy() { [[ "${PROXY:-nodeproxy}" != "none" ]]; }

# Whether this run wants the datastore. Unlike nodelet/nodeproxy, this
# defaults OFF: nodelet and nodeproxy are node components every node runs by
# default, but the datastore is a control-plane component, and today that
# job is still done by k3s's own bundled kine — a node has to opt into ours
# explicitly via DATASTORE=nodestore. Note this predicate only decides what
# a given device's own build includes; a release build takes the crate's
# default features regardless, so it ships a combined binary containing
# every component (nodestore included) no matter what any device sets here.
want_nodestore() { [[ "${DATASTORE:-none}" == "nodestore" ]]; }

# Whether this run wants our scheduler. Defaults OFF for the same reason
# nodestore does: it is a control-plane component replacing something the
# stripped k3s control plane still bundles and runs in-process (its own
# kube-scheduler), so turning ours on is only half the job — the other half
# is k3s's --disable-scheduler, which setup-control-plane.sh adds when this
# is set. Two schedulers both binding pods is not a degraded mode, it is a
# race, so this has to be opted into deliberately rather than defaulted on.
# Same release-build caveat as nodestore: a release takes the crate's
# default features and ships a combined binary containing every component
# regardless of what any device sets here.
want_nodescheduler() { [[ "${SCHEDULER:-none}" == "nodescheduler" ]]; }

# Whether this run wants our controller manager. Same defaulting reasoning
# as nodescheduler/nodestore: it replaces a job the stripped k3s control
# plane still bundles (its own kube-controller-manager), so turning ours on
# is only half the job — setup-control-plane.sh adds k3s's own
# --disable-controller-manager when this is set, the same two-halves-of-one-
# switch pairing --scheduler=nodescheduler already established. Two active
# controller managers is a race for every object each of them writes (Node
# taints, podCIDR, owner-ref GC, ...), not a redundant pair.
want_nodecontroller() { [[ "${CONTROLLER_MANAGER:-none}" == "nodecontroller" ]]; }

# component_field <row> <1-based index> — pipe-separated field accessor.
component_field() { echo "$1" | cut -d'|' -f"$2"; }

# enabled_components — echoes the names of the components this run wants,
# one per line, in table order.
enabled_components() {
    local row name pred
    for row in "${NOTK8S_COMPONENTS[@]}"; do
        name="$(component_field "$row" 1)"
        pred="$(component_field "$row" 4)"
        if "$pred"; then echo "$name"; fi
    done
}

# component_row <name> — echoes the table row for a component, or nothing.
component_row() {
    local row
    for row in "${NOTK8S_COMPONENTS[@]}"; do
        [[ "$(component_field "$row" 1)" == "$1" ]] && { echo "$row"; return 0; }
    done
    return 1
}

# component_cargo_features <name> — echoes the cargo feature flags for a
# split build of this component ("" or "--features cri"), resolving @cri
# against this run's WITH_CRI. Word-split by the caller on purpose.
component_cargo_features() {
    local row spec
    # An unknown name must be loud: component_row returns nothing, and an
    # empty spec would silently mean "no features" — i.e. a nodelet built
    # without --features cri, which fails much later and looks like a
    # runtime bug rather than a typo in the table.
    row="$(component_row "$1")" || die "Unknown component '$1' — every component needs a row in NOTK8S_COMPONENTS (lib/components.sh)."
    spec="$(component_field "$row" 3)"
    case "$spec" in
        "") echo "" ;;
        @cri) [[ "${WITH_CRI:-0}" -eq 1 ]] && echo "--features cri" || echo "" ;;
        *) echo "--features $spec" ;;
    esac
}

# any_component_needs_protoc — whether this run has to install protoc before
# building. Asked of the table so a new component that compiles protos gets
# it without anyone remembering to widen a --with-cri check.
any_component_needs_protoc() {
    local name spec
    while read -r name; do
        spec="$(component_field "$(component_row "$name")" 6)"
        case "$spec" in
            protoc) return 0 ;;
            @cri) [[ "${WITH_CRI:-0}" -eq 1 ]] && return 0 ;;
        esac
    done < <(enabled_components)
    return 1
}

# ─────────────────────────────────────────────────────────────────────────
# Build layout
# ─────────────────────────────────────────────────────────────────────────
#
# NOTK8S_BUILD_LAYOUT (or --layout= on the bootstrap entry points):
#
#   split     (default) one binary per component: bin/nodelet, bin/nodeproxy.
#             Full modularity — install only what this node runs, replace one
#             component without touching the other, mix with upstream
#             components. Costs one full copy of the shared dependency tree
#             (tokio/kube/k8s-openapi/rustls) per binary.
#   combined  one multi-call binary, bin/notk8s, with bin/<component>
#             symlinks pointing at it. It dispatches on argv[0], so
#             everything downstream (service units, run-*.sh, the e2e suite)
#             is unchanged. One copy of the shared dependencies for all
#             components — on aarch64 that was ~12MB total vs. ~17MB split.
#   both      build both layouts. The split binaries are the ones *installed
#             and run* (least surprise); the combined binary is produced
#             alongside at bin/notk8s for packaging/comparison. This is what
#             a release build wants: ship both, let the operator choose.
#
# The choice is purely about packaging. Both layouts run the same code, read
# the same environment variables, and are started by the same per-component
# services — a node running the combined binary still runs two processes and
# can still stop one of them.
resolve_build_layout() {
    local layout="${NOTK8S_BUILD_LAYOUT:-split}"
    case "$layout" in
        split|combined|both) echo "$layout" ;;
        *) die "Unknown build layout '$layout' (want 'split', 'combined', or 'both'). Set NOTK8S_BUILD_LAYOUT or pass --layout=." ;;
    esac
}

layout_builds_split() { [[ "$1" == "split" || "$1" == "both" ]]; }
layout_builds_combined() { [[ "$1" == "combined" || "$1" == "both" ]]; }
# Which layout's binaries actually get wired up as bin/<component>.
layout_installs_combined() { [[ "$1" == "combined" ]]; }

# combined_cargo_features — echoes the cargo flags selecting exactly this
# run's components in the combined binary. Nothing when every component is
# wanted (notk8s's own defaults already cover that); an explicit
# --no-default-features set otherwise, so a --proxy=none node doesn't carry
# a Service proxy it will never run.
combined_cargo_features() {
    local -a flags=() names=()
    mapfile -t names < <(enabled_components)
    local total="${#NOTK8S_COMPONENTS[@]}"
    if [[ "${#names[@]}" -lt "$total" ]]; then
        flags+=(--no-default-features --features "$(IFS=,; echo "${names[*]}")")
    fi
    [[ "${WITH_CRI:-0}" -eq 1 ]] && flags+=(--features cri)
    echo "${flags[@]:-}"
}
