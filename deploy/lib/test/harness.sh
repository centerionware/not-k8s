# lib/test/harness.sh — test registration, running, and assertions for
# test-e2e.sh. Deliberately not a generic test framework — it exists to run
# a fixed, known list of `test_*` functions defined in lib/test/cases/*.sh
# against a real, already-running cluster (apiserver + nodelet), print a
# PASS/FAIL/SKIP line per test, and exit nonzero if anything failed.
#
# Expects lib/common.sh (log/warn/die) already sourced, and these globals
# set by test-e2e.sh before use: TEST_NAMESPACE, KEEP_NAMESPACE, ONLY_PATTERN.

TESTS_REGISTERED=()
TESTS_PASSED=0
TESTS_FAILED=0
TESTS_SKIPPED=0
FAILED_TEST_NAMES=()
# Round 124: explicit, at-registration-time infrastructure requirements —
# deliberately NOT the same mechanism as _reorder_env_reconfiguring_tests_
# last()'s auto-detection below. That one stays auto-detected on purpose
# (see its own doc comment: a hand-maintained list/tag there would silently
# stop protecting a new restart-heavy test the moment someone forgot to tag
# it). csi_dra is different in kind, not just degree — it answers "does
# the shard running this test need extra infrastructure installed before
# any test in it can run", a real per-shard setup-cost question with no
# auto-detectable signal in the test's own body the way a restart call has
# (a test needing TEST_CSI_STORAGE_CLASS to be set is often gated by a
# plain `if [[ -z "$TEST_CSI_..." ]]; then skip_test` that no regex could
# reliably distinguish from a hundred other env-var-gated skips in this
# suite that need nothing special).
declare -A TEST_NEEDS_CSI_DRA=()

# register_test <function-name> [csi_dra] — call once per test_* function,
# in the order you want them to run. Order matters a little (cheap/
# foundational tests first is a better failure signal than an expensive
# one failing first and hiding a simpler break), but nothing here depends
# on a specific order being correct. The optional csi_dra tag marks a test
# as needing the real CSI/DRA reference drivers (e2e-full-setup.sh)
# installed — see _cluster_csi_dra_tests_onto_driver_shards()'s own doc
# comment for what this drives.
register_test() {
    local name="$1"
    shift
    TESTS_REGISTERED+=("$name")
    local tag
    for tag in "$@"; do
        case "$tag" in
            csi_dra) TEST_NEEDS_CSI_DRA["$name"]=1 ;;
            *) echo "register_test: unknown tag '$tag' for $name" >&2; exit 1 ;;
        esac
    done
}

# skip_test <reason> — call from inside a test_* function to bail out
# early as SKIP rather than PASS/FAIL, e.g. when a prerequisite (cri
# runtime, a feature flag) isn't present. Uses a bash exception-by-exit-code
# convention: exit 99 from the test function's subshell is "skipped".
skip_test() {
    echo "    (skip: $*)"
    exit 99
}

# run_test <function-name> — invoked by the main loop. Runs the test in a
# subshell (so a `set -e` failure or stray `exit` inside a test can't kill
# the whole suite) with its own timeout, reports PASS/FAIL/SKIP, and always
# runs the test's namespace-scoped cleanup step regardless of outcome.
run_test() {
    local name="$1"
    printf '\033[1;36m▶ %s\033[0m\n' "$name"
    local start
    start=$(date +%s)
    if ( set -euo pipefail; "$name" ); then
        local elapsed=$(( $(date +%s) - start ))
        printf '\033[1;32m  ✔ PASS\033[0m (%ss)\n' "$elapsed"
        TESTS_PASSED=$((TESTS_PASSED + 1))
    else
        local code=$?
        local elapsed=$(( $(date +%s) - start ))
        if [[ "$code" -eq 99 ]]; then
            printf '\033[1;33m  ○ SKIP\033[0m (%ss)\n' "$elapsed"
            TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        else
            printf '\033[1;31m  ✘ FAIL\033[0m (%ss, exit %s)\n' "$elapsed" "$code"
            TESTS_FAILED=$((TESTS_FAILED + 1))
            FAILED_TEST_NAMES+=("$name")
            # Opt-in (CI sets this; local runs stay quiet by default) —
            # see e2e-quick-diag.sh's own doc comment for why this is a
            # small per-failure snapshot rather than the full end-of-run
            # dump repeated every time.
            [[ "${NOTK8S_E2E_DEBUG_ON_FAIL:-0}" == "1" ]] && bash "$LIB_DIR/e2e-quick-diag.sh"
        fi
    fi
}

# _reorder_env_reconfiguring_tests_last — moves every registered test
# whose own body touches nodelet_restart_with_env / nodeproxy_restart_*
# / nodeproxy_restore_env
# (a real systemd restart) or restarts/reconfigures another host-level
# service (containerd, swap) to the END of TESTS_REGISTERED, preserving relative
# order within each of the two groups otherwise. Round 123: found live in
# CI — a batch mixing these with ordinary pod-creation tests produced
# flaky "pod never reached Running" failures that moved to a DIFFERENT
# test on every rerun, tracking down to real contention (node briefly
# NotReady, CSI/DRA plugins re-registering, volumes re-materializing)
# from whichever restart-ish test happened to run nearby. Detection is
# by grepping each test function's real source (`declare -f`) rather
# than a hand-maintained list, so a new test that starts doing this
# automatically gets deferred too, without anyone remembering to update
# a list here.
_reorder_env_reconfiguring_tests_last() {
    local -a normal=() deferred=()
    local name
    for name in "${TESTS_REGISTERED[@]}"; do
        # stop|start as well as restart: a test that deliberately takes
        # containerd down for a while (cases/retry_backoff.sh) disrupts every
        # concurrent pod on this node just as much as one that restarts it.
        #
        # k3s belongs here for the same reason and more so: restarting the
        # control plane takes the apiserver away from every other test at
        # once, and the node agent needs a moment afterwards to re-establish
        # its watches (cases/watch_recovery.sh, which is the test that does
        # this deliberately). Left in place, it would look exactly like a
        # flaky unrelated test rather than a disruption someone chose.
        if declare -f "$name" 2>/dev/null | grep -qE 'nodelet_restart_with_env|nodeproxy_(restart|restore)_|sudo (systemctl (restart|stop|start) (containerd|k3s)|swapon|mkswap)'; then
            deferred+=("$name")
        else
            normal+=("$name")
        fi
    done
    TESTS_REGISTERED=("${normal[@]}" "${deferred[@]}")
}

# Round 124: how many of the shards actually install the real CSI/DRA
# reference drivers (e2e-full-setup.sh). Fixed and small on purpose, NOT
# "however many shards happen to draw a csi_dra test" — plain round-robin
# across all shards was tried first and confirmed live to scatter even a
# small csi_dra set (12 of 151 tests) across literally every shard, since
# they're spread too thin to avoid it. Keeping this fixed is what makes
# skipping driver setup on the other shards possible at all. e2e.yml's own
# matrix must match this exactly (needs_drivers: true only for shard
# indices <= this) — the two aren't wired together automatically, so a
# change here needs the matrix updated too.
NUM_DRIVER_SHARDS=2

# _filter_shard <index> <total> — keep only this shard's slice of
# TESTS_REGISTERED. Round 124: splits one long serial e2e run across up to
# 5 parallel CI runners (test-e2e.sh's own --shard=N/M flag, wired from
# e2e.yml's matrix), each bringing up its own independent cluster — this
# only selects WHICH of the already-registered tests this process runs,
# nothing about how each one runs.
#
# csi_dra-tagged tests (register_test's own tag, TEST_NEEDS_CSI_DRA) are
# round-robined ONLY across the first NUM_DRIVER_SHARDS shard indices,
# separately from everything else — see that constant's own comment for
# why. Every other registered test round-robins across all <total> shards,
# same as before this split existed. Round-robin (not a contiguous chunk
# per shard) within each group on purpose: consecutive tests in one case
# file tend to be related/similar in cost (all the eviction tests, all the
# hostUsers tests, ...), so a contiguous split would risk one shard
# getting disproportionately slow/flaky tests while another gets all the
# fast ones; interleaving spreads that variance evenly instead.
_filter_shard() {
    local index="$1" total="$2"
    local -a driver_tests=() other_tests=()
    local name
    for name in "${TESTS_REGISTERED[@]}"; do
        if [[ -n "${TEST_NEEDS_CSI_DRA[$name]:-}" ]]; then
            driver_tests+=("$name")
        else
            other_tests+=("$name")
        fi
    done

    local -a kept=()
    local i
    if (( index <= NUM_DRIVER_SHARDS )); then
        local driver_total=$(( total < NUM_DRIVER_SHARDS ? total : NUM_DRIVER_SHARDS ))
        for i in "${!driver_tests[@]}"; do
            if (( i % driver_total == index - 1 )); then
                kept+=("${driver_tests[$i]}")
            fi
        done
    fi
    for i in "${!other_tests[@]}"; do
        if (( i % total == index - 1 )); then
            kept+=("${other_tests[$i]}")
        fi
    done
    TESTS_REGISTERED=("${kept[@]}")
}

run_all_registered_tests() {
    if [[ -n "${SHARD_TOTAL:-}" ]]; then
        _filter_shard "$SHARD_INDEX" "$SHARD_TOTAL"
        log "Shard $SHARD_INDEX/$SHARD_TOTAL: ${#TESTS_REGISTERED[@]} test(s) selected"
    fi
    _reorder_env_reconfiguring_tests_last
    # NOTK8S_E2E_MAX_FAILURES stops the whole run once this many tests
    # have failed, instead of always running every remaining test —
    # unset/0 means unlimited (the historical, still-default local
    # behavior). Set to a small number in CI: a systemic break (the node
    # itself degraded, a real regression affecting a whole category of
    # tests) shows the same failure shape over and over, and running the
    # other ~130 tests anyway just to also fail from the same root cause
    # costs 30+ minutes for zero extra signal — confirmed for real, round
    # 123's first full CI run took 34 minutes to report 15 failures that
    # were all downstream of the same thing. A few failures' worth of
    # signal is enough to start root-causing; more than that is waste,
    # not thoroughness.
    local max_failures="${NOTK8S_E2E_MAX_FAILURES:-0}"
    # ONLY_PATTERN may be a comma-separated list (round 123) — lets one CI
    # dispatch cover several known-failing tests back to back instead of
    # one dispatch per test, without giving up substring matching (each
    # comma-separated piece is still matched the same way a bare
    # --only=<substring> always has been). A test matching ANY piece runs.
    local -a only_patterns=()
    if [[ -n "$ONLY_PATTERN" ]]; then
        IFS=',' read -ra only_patterns <<< "$ONLY_PATTERN"
    fi
    for name in "${TESTS_REGISTERED[@]}"; do
        if ((${#only_patterns[@]} > 0)); then
            local matched=false
            for p in "${only_patterns[@]}"; do
                if [[ "$name" == *"$p"* ]]; then
                    matched=true
                    break
                fi
            done
            [[ "$matched" == false ]] && continue
        fi
        run_test "$name"
        if [[ "$max_failures" -gt 0 && "$TESTS_FAILED" -ge "$max_failures" ]]; then
            warn "stopping early: $TESTS_FAILED failures reached NOTK8S_E2E_MAX_FAILURES=$max_failures (${#TESTS_REGISTERED[@]} tests registered total, not all of them ran) — see the failures above/print_summary below for what's already known, rather than burning time re-discovering the same root cause across the rest of the suite."
            break
        fi
    done
}

print_summary() {
    echo
    printf '\033[1;34m════════════════════════════════════════\033[0m\n'
    printf 'Results: \033[1;32m%s passed\033[0m, \033[1;31m%s failed\033[0m, \033[1;33m%s skipped\033[0m\n' \
        "$TESTS_PASSED" "$TESTS_FAILED" "$TESTS_SKIPPED"
    if [[ "$TESTS_FAILED" -gt 0 ]]; then
        echo "Failed:"
        for n in "${FAILED_TEST_NAMES[@]}"; do
            printf '  - %s\n' "$n"
        done
    fi
    printf '\033[1;34m════════════════════════════════════════\033[0m\n'
}

# ── Assertions — each dies (fails the current test) with a clear message. ──

assert_eq() { # assert_eq <actual> <expected> <description>
    [[ "$1" == "$2" ]] || die "assertion failed: $3 — got '$1', want '$2'"
}

assert_not_eq() {
    [[ "$1" != "$2" ]] || die "assertion failed: $3 — got '$1', which should have differed from '$2'"
}

assert_contains() { # assert_contains <haystack> <needle> <description>
    [[ "$1" == *"$2"* ]] || die "assertion failed: $3 — '$1' does not contain '$2'"
}

assert_not_contains() { # assert_not_contains <haystack> <needle> <description>
    [[ "$1" != *"$2"* ]] || die "assertion failed: $3 — '$1' contains '$2', which it should not"
}

assert_not_empty() { # assert_not_empty <value> <description>
    [[ -n "$1" ]] || die "assertion failed: $2 — value was empty"
}

assert_true() { # assert_true <command...> — runs the command, asserts exit 0
    "$@" || die "assertion failed: command failed: $*"
}

# try_wait_until <timeout-seconds> <command...> — like k8s.sh's wait_until,
# but returns 1 on timeout instead of dying. For conditions that depend on
# optional cluster setup (e.g. RBAC a fresh deployment may not have granted
# yet) where "never happened" should be a SKIP with an explanatory message,
# not a hard FAIL.
try_wait_until() {
    local timeout="$1"
    shift
    # Real elapsed wall-clock time, not "iterations * 2s" — the old
    # accounting only counted the `sleep 2` between attempts, silently
    # ignoring how long "$@" itself took to return. Confirmed for real in
    # CI (round 123): once kubectl calls started carrying their own
    # --request-timeout=30s, a condition whose command legitimately took
    # close to 30s per attempt turned a nominal "timeout 60" into several
    # real minutes, because each 30s-long failed attempt only "cost" 2s
    # against the budget.
    local start_s=$SECONDS
    while ! "$@" >/dev/null 2>&1; do
        [[ "$((SECONDS - start_s))" -ge "$timeout" ]] && return 1
        sleep 2
    done
    return 0
}
