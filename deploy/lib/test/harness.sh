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

# register_test <function-name> — call once per test_* function, in the
# order you want them to run. Order matters a little (cheap/foundational
# tests first is a better failure signal than an expensive one failing
# first and hiding a simpler break), but nothing here depends on a specific
# order being correct.
register_test() {
    TESTS_REGISTERED+=("$1")
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

run_all_registered_tests() {
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
    local waited=0
    while ! "$@" >/dev/null 2>&1; do
        [[ "$waited" -ge "$timeout" ]] && return 1
        sleep 2
        waited=$((waited + 2))
    done
    return 0
}
