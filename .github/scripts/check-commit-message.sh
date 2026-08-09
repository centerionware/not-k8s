#!/usr/bin/env bash
# check-commit-message.sh — validate one commit message (or PR title)
# against this repo's Conventional Commits rules. See CONTRIBUTING.md for
# the human-readable version; this file is the authority on what actually
# passes.
#
# Pure text-in/text-out so it's runnable and testable outside CI, the same
# way deploy/lib/changelog.sh is:
#
#   .github/scripts/check-commit-message.sh --message "feat(nodeproxy): add a thing"
#   .github/scripts/check-commit-message.sh --file .git/COMMIT_EDITMSG
#   git log -1 --format=%B | .github/scripts/check-commit-message.sh
#   .github/scripts/check-commit-message.sh --title "fix(ci): ..."   # header-only mode
#
# Exit 0 = valid, 1 = invalid (reasons printed to stderr), 2 = usage error.
#
# --title mode checks only the header line and is what the PR-title check
# uses: a squash merge takes the PR title as the resulting commit's
# subject, so a tidy commit history can still be wrecked at merge time by
# a sloppy title. There's no body to check in that case.
set -uo pipefail

# The standard Conventional Commits type set. Deliberately not extended
# with project-specific types — the scope field already carries "which
# part of not-k8s", so a `deploy:`/`e2e:` type would only duplicate it
# less precisely (`fix(e2e):` says strictly more than `e2e:` does).
readonly TYPES='build|chore|ci|docs|feat|fix|perf|refactor|revert|style|test'

# 100, not the traditional 50 or 72. This repo's own history has a median
# subject of 71 characters and a max of 153, because subjects here carry
# real information ("fix the retry's own success check — -s isn't -n")
# rather than a label. A 72-char cap would fight that on nearly half of
# all existing commits. 100 is also commitlint's own default, so this
# isn't an idiosyncratic number.
readonly MAX_HEADER=100

MODE=full
MESSAGE=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --message) MESSAGE="${2:?--message needs a value}"; shift 2 ;;
        --title)   MESSAGE="${2:?--title needs a value}"; MODE=title; shift 2 ;;
        --file)    MESSAGE="$(cat "${2:?--file needs a path}")"; shift 2 ;;
        -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)         echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

# No explicit source: read the message from stdin.
if [[ -z "$MESSAGE" && ! -t 0 ]]; then
    MESSAGE="$(cat)"
fi
[[ -n "${MESSAGE//[[:space:]]/}" ]] || { echo "empty commit message" >&2; exit 1; }

errors=()
fail() { errors+=("$1"); }

header="$(printf '%s\n' "$MESSAGE" | head -n1)"
body="$(printf '%s\n' "$MESSAGE" | tail -n +2)"
second_line="$(printf '%s\n' "$MESSAGE" | sed -n '2p')"

# ── Exemptions ────────────────────────────────────────────────────────
# Merge commits and git's own auto-generated revert subjects are produced
# by git itself, not typed by a human, so holding them to the format would
# just mean telling people to hand-edit machine output. `revert` is still
# a valid type for a revert you write yourself.
machine_re='^(Merge |Revert ")'
if [[ "$header" =~ $machine_re ]]; then
    exit 0
fi

# ── Header format ─────────────────────────────────────────────────────
# type(optional-scope)!: description
#
# Both patterns live in variables rather than inline: bash's [[ =~ ]]
# parser mis-tokenizes a bracket expression containing an unescaped ')'
# (as in [^)]*) and fails with a syntax error at parse time, before any
# input is ever matched. Confirmed the hard way.
readonly HEADER_RE="^($TYPES)(\([a-zA-Z0-9._/-]+\))?!?: .+"
readonly SHAPE_RE='^[A-Za-z]+(\([^)]*\))?!?:'
if [[ ! "$header" =~ $HEADER_RE ]]; then
    if [[ "$header" =~ $SHAPE_RE ]]; then
        # Right shape, wrong type — much more useful than the generic message.
        fail "unknown type in '${header%%:*}'. Valid types: ${TYPES//|/, }"
    else
        fail "header must be 'type(scope): description' or 'type: description' — e.g. 'fix(nodeproxy): exit non-zero when nft is unusable'. Valid types: ${TYPES//|/, }"
    fi
else
    description="${header#*: }"

    [[ "${#header}" -le "$MAX_HEADER" ]] \
        || fail "header is ${#header} characters, max is $MAX_HEADER"

    [[ "$header" != *"  "* ]] || fail "header has a double space"

    [[ "$description" != *. ]] \
        || fail "description must not end with a period"

    [[ "${#description}" -ge 10 ]] \
        || fail "description '$description' is too short to say anything useful (min 10 characters)"

    # Sentence-case descriptions are the most common drift once a repo
    # adopts this format, and they read badly next to the lowercase type.
    sentence_case_re='^[A-Z][a-z]'
    [[ ! "$description" =~ $sentence_case_re ]] \
        || fail "description should start lowercase ('${description:0:1}' → '$(printf '%s' "${description:0:1}" | tr '[:upper:]' '[:lower:]')') — the type prefix already opens the sentence"

    # Subjects that describe the act of committing rather than the change.
    placeholder_re='^(wip|WIP|stuff|things|updates?|changes?|fixes?|misc|cleanup|minor|tweaks?)$'
    if [[ "$description" =~ $placeholder_re ]]; then
        fail "description '$description' says nothing about what changed — describe the change, not the act of committing"
    fi
fi

# ── fixup!/squash! ────────────────────────────────────────────────────
# These are legitimate mid-review, but must be autosquashed before merge
# (`git rebase -i --autosquash`) rather than landing on main.
autosquash_re='^(fixup|squash)!'
if [[ "$header" =~ $autosquash_re ]]; then
    fail "'$header' must be autosquashed before merge (git rebase -i --autosquash)"
fi

# ── Body ──────────────────────────────────────────────────────────────
if [[ "$MODE" == "full" ]]; then
    if [[ -n "${body//[[:space:]]/}" ]]; then
        [[ -z "$second_line" ]] \
            || fail "the line after the header must be blank (separates subject from body)"
    fi

    # BREAKING CHANGE is a footer token with an exact spelling; a
    # near-miss silently means "no breaking change was declared", which is
    # worse than no footer at all.
    readonly NEAR_MISS_RE='^[Bb][Rr][Ee][Aa][Kk][Ii][Nn][Gg][ _-][Cc][Hh][Aa][Nn][Gg][Ee][Ss]?:'
    readonly EXACT_RE='^BREAKING CHANGE:'
    while IFS= read -r line; do
        if [[ "$line" =~ $NEAR_MISS_RE ]] && [[ ! "$line" =~ $EXACT_RE ]]; then
            fail "'${line%%:*}:' must be spelled exactly 'BREAKING CHANGE:' to count as a breaking-change footer"
        fi
    done <<< "$body"
fi

# ── Report ────────────────────────────────────────────────────────────
if [[ "${#errors[@]}" -gt 0 ]]; then
    {
        echo "✘ invalid commit message:"
        echo ""
        echo "    $header"
        echo ""
        for e in "${errors[@]}"; do echo "  - $e"; done
        echo ""
        echo "  See CONTRIBUTING.md. Examples:"
        echo "    feat(nodeproxy): watch EndpointSlices instead of Endpoints"
        echo "    fix(deploy): remove the stale pid file, not just the process"
        echo "    docs: explain why the profiling legs use --proxy=none"
        echo "    refactor(nodelet)!: drop the in-process service proxy"
    } >&2
    exit 1
fi

exit 0
