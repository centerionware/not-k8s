# lib/test/cases/build_layout.sh — the combined single-binary layout.
#
# The components ship two ways (deploy/lib/components.sh): `split` — one
# binary per component — and `combined` — one multi-call `notk8s` binary
# that every component name symlinks to and that dispatches on argv[0].
# The claim the combined layout rests on is that it is byte-for-byte the
# same behaviour, so nothing downstream (the service units, run-*.sh, the
# other ~150 tests in this suite) has to know which one is installed.
#
# These tests check exactly that claim on whatever layout this deployment
# actually installed, and skip cleanly on the other one — they're not a
# reason to run the suite twice, they're the thing that would catch the
# combined binary silently losing a component or dispatching wrong. Every
# other test in the suite is already an end-to-end check of the installed
# layout by construction: if `bin/nodelet` is a symlink to `notk8s` and
# pods still run, the dispatch worked.

# The component list comes from the same table the build system uses, so a
# component added there is covered here without touching this file — the
# whole point of that table. Sourced rather than restated: a hand-copied
# list of component names in a test asserting that component names live in
# one place would be its own refutation.
# shellcheck source=../../components.sh
source "$REPO_ROOT/deploy/lib/components.sh"

# every_installed_component — the table's names, in table order, filtered to
# the ones this deployment actually installed.
#
# Not the whole table: the combined binary's contents are chosen at build
# time. combined_cargo_features() passes `--no-default-features --features
# <enabled>` whenever this run wants fewer than every component, so a
# `--proxy=none --layout=combined` node gets a binary that correctly does not
# contain nodeproxy — and a test demanding every table row be present would
# fail on it for doing the right thing.
#
# Not enabled_components() either: that re-evaluates the predicates *now*,
# and PROXY/DATASTORE are set at deploy time, not in this context. The
# bin/<name> entries are the durable record of what the build decided —
# install_combined_layout() writes exactly one per component it built in.
#
# A dangling symlink counts as installed (-L, not just -e). bin/nodeproxy
# pointing at a missing bin/notk8s is precisely the breakage these tests
# exist to catch; skipping it as "not installed" would hide it.
every_installed_component() {
    local row name
    for row in "${NOTK8S_COMPONENTS[@]}"; do
        name="$(component_field "$row" 1)"
        [[ -e "$REPO_ROOT/bin/$name" || -L "$REPO_ROOT/bin/$name" ]] && printf '%s\n' "$name"
    done
    return 0
}

# The combined binary, wherever this deployment put it, or "" if this is a
# split install.
_combined_binary() {
    local candidate
    for candidate in "$REPO_ROOT/bin/notk8s" "$REPO_ROOT/target/release/notk8s" "$REPO_ROOT/target/debug/notk8s"; do
        [[ -x "$candidate" ]] && { echo "$candidate"; return 0; }
    done
    echo ""
}

test_combined_binary_contains_every_component() {
    local bin
    bin="$(_combined_binary)"
    [[ -n "$bin" ]] || skip_test "no combined binary here (split layout — build with --layout=combined or --layout=both)"

    local components name
    components="$("$bin" components)"
    while read -r name; do
        assert_contains "$components" "$name" "'notk8s components' should list the '$name' component"
    done < <(every_installed_component)

    # A component in the dispatch table but not in the help output (or vice
    # versa) means the two have drifted, which is how a component gets
    # silently dropped from a release.
    local help
    help="$("$bin" --help)"
    local name
    while read -r name; do
        [[ -n "$name" ]] || continue
        assert_contains "$help" "$name" "'notk8s --help' should describe the '$name' component it can run"
    done <<< "$components"
}

test_combined_binary_rejects_an_unknown_component() {
    local bin
    bin="$(_combined_binary)"
    [[ -n "$bin" ]] || skip_test "no combined binary here (split layout)"

    # Non-zero exit, not a silent no-op: a typo'd component name in a
    # service unit must fail the unit, not start a process that does
    # nothing and looks healthy.
    local rc=0
    "$bin" nodeproxyy >/dev/null 2>&1 || rc=$?
    assert_not_eq "$rc" "0" "an unknown component name should exit non-zero"
}

test_installed_component_binaries_are_runnable_whatever_the_layout() {
    # The layout-agnostic half: bin/<component> must be executable and be
    # the right component, whether it's a real binary or a symlink into the
    # combined one. `nodeproxy --help` isn't a thing (it takes no arguments
    # — everything is env-driven), so this checks the file, not the output.
    local name
    while read -r name; do
        local path="$REPO_ROOT/bin/$name"
        assert_true test -x "$path"

        if [[ -L "$path" ]]; then
            # Combined layout: the symlink must resolve to the combined
            # binary sitting next to it, and it must be relative — bin/ gets
            # copied around (deploy.tar.gz, a device's install path) and an
            # absolute link into a build checkout breaks the moment it is.
            local target
            target="$(readlink "$path")"
            assert_eq "$target" "notk8s" "bin/$name should be a relative symlink to the combined binary"
            assert_true test -x "$REPO_ROOT/bin/notk8s"
            # argv[0] dispatch is what makes the symlink mean anything.
            assert_contains "$("$REPO_ROOT/bin/notk8s" components)" "$name" \
                "the combined binary bin/$name points at should actually contain '$name'"
        fi
    done < <(every_installed_component)
}

test_a_failing_component_says_why_before_it_exits() {
    # A component that can't start must print the reason, not just exit
    # non-zero. Found live while verifying the combined layout: routing
    # nodeproxy's startup through a caller that did `is_err() -> exit(1)`
    # left an unreachable-apiserver failure completely silent — a service
    # manager restarting a process whose logs say nothing at all is the
    # worst possible version of this, because the restart loop looks like
    # the symptom rather than the report.
    #
    # Deliberately layout-agnostic: bin/nodeproxy is the same entry point
    # either way, which is the point.
    local bin="$REPO_ROOT/bin/nodeproxy"
    [[ -x "$bin" ]] || skip_test "no nodeproxy binary here (--proxy=none?)"

    local scratch output rc=0
    scratch="$(mktemp -d)"
    # A kubeconfig path that cannot resolve, so this fails at client
    # construction — before it touches nft or this node's real rules.
    # KUBERNETES_SERVICE_HOST is unset too: kube's Config::infer() tries the
    # in-cluster environment *first*, so leaving it set is the one way this
    # could start successfully, reach nft, and then be killed by the timeout
    # with a non-zero status that looks like the assertion passing.
    #
    # Deliberately foreground, despite the long-running-command guideline:
    # this asserts nodeproxy *exits*, and its exit status is the primary
    # assertion. `timeout` already bounds it, and a run that needs the full
    # 30s is a failure this test should report, not a process to detach and
    # poll. setsid+nohup is for commands meant to outlive the caller; this
    # one is meant to die.
    output="$(env -u KUBERNETES_SERVICE_HOST -u KUBERNETES_SERVICE_PORT \
        KUBECONFIG="$scratch/nonexistent" timeout 30 "$bin" 2>&1)" || rc=$?
    rm -rf "$scratch"

    assert_not_eq "$rc" "0" "nodeproxy should exit non-zero when it can't reach an apiserver"
    assert_not_empty "$output" "nodeproxy should say why it exited, not fail silently"
    assert_contains "$output" "kube client" "the failure message should name what actually failed"
}

register_test test_combined_binary_contains_every_component
register_test test_combined_binary_rejects_an_unknown_component
register_test test_installed_component_binaries_are_runnable_whatever_the_layout
register_test test_a_failing_component_says_why_before_it_exits
