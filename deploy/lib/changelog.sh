#!/usr/bin/env bash
# changelog.sh — render commit history since the last release tag as a
# release-notes markdown body. Pure text-in/text-out (given a git repo
# with the right history checked out), so it's runnable/testable outside
# CI.
#
# Commits following the Conventional Commits format CONTRIBUTING.md
# requires get grouped by type, with the type prefix stripped from the
# rendered line (the section heading already says it). Anything else —
# every commit from before that convention was adopted, and the odd merge
# — falls through to a plain "Other changes" section verbatim, so this
# never drops a commit just because it predates the rules.
#
# Usage: changelog.sh <new-tag>
set -euo pipefail

NEW_TAG="${1:?usage: changelog.sh <new-tag>}"

PREV_TAG="$(git describe --tags --abbrev=0 --match 'v*' 2>/dev/null || true)"

# Sections in the order they're rendered. Types with no commits are
# skipped entirely rather than printed as empty headings.
SECTION_TYPES=(feat fix perf refactor docs test build ci style chore revert)
declare -A SECTION_TITLES=(
    [feat]="Features"
    [fix]="Fixes"
    [perf]="Performance"
    [refactor]="Refactoring"
    [docs]="Documentation"
    [test]="Tests"
    [build]="Build"
    [ci]="CI"
    [style]="Style"
    [chore]="Chores"
    [revert]="Reverts"
)

echo "## $NEW_TAG"
echo ""

if [[ -n "$PREV_TAG" ]]; then
    echo "Changes since $PREV_TAG:"
    RANGE="${PREV_TAG}..HEAD"
else
    echo "Initial release."
    RANGE=""
fi
echo ""

# %s%x09%h — subject and short hash, tab-separated, so a subject
# containing anything at all can't confuse the split.
if [[ -n "$RANGE" ]]; then
    mapfile -t commits < <(git log "$RANGE" --no-merges --pretty=format:"%s%x09%h")
else
    mapfile -t commits < <(git log --no-merges --pretty=format:"%s%x09%h")
fi

declare -A grouped=()
breaking=""
other=""

for entry in "${commits[@]}"; do
    [[ -n "$entry" ]] || continue
    subject="${entry%%$'\t'*}"
    short="${entry##*$'\t'}"

    # type(scope)!: description
    if [[ "$subject" =~ ^([a-z]+)(\(([^\)]+)\))?(!)?:\ (.+)$ ]]; then
        type="${BASH_REMATCH[1]}"
        scope="${BASH_REMATCH[3]}"
        bang="${BASH_REMATCH[4]}"
        desc="${BASH_REMATCH[5]}"
    else
        other+="- $subject ($short)"$'\n'
        continue
    fi

    # An unknown-but-well-formed type (someone's local convention, or a
    # type added to the checker later than this script) is still better
    # rendered verbatim than silently dropped.
    if [[ -z "${SECTION_TITLES[$type]+set}" ]]; then
        other+="- $subject ($short)"$'\n'
        continue
    fi

    if [[ -n "$scope" ]]; then
        line="- **${scope}:** ${desc} (${short})"
    else
        line="- ${desc} (${short})"
    fi

    # Breaking changes get called out at the top as well as listed in
    # their own section — the whole point of the `!` marker is that it
    # shouldn't need hunting for.
    if [[ -n "$bang" ]]; then
        breaking+="$line"$'\n'
    fi

    grouped[$type]+="$line"$'\n'
done

if [[ -n "$breaking" ]]; then
    echo "### ⚠ BREAKING CHANGES"
    echo ""
    printf '%s' "$breaking"
    echo ""
fi

for type in "${SECTION_TYPES[@]}"; do
    [[ -n "${grouped[$type]:-}" ]] || continue
    echo "### ${SECTION_TITLES[$type]}"
    echo ""
    printf '%s' "${grouped[$type]}"
    echo ""
done

if [[ -n "$other" ]]; then
    echo "### Other changes"
    echo ""
    printf '%s' "$other"
    echo ""
fi
