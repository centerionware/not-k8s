#!/usr/bin/env bash
# changelog.sh — render commit history since the last release tag as a
# release-notes markdown body. Pure text-in/text-out (given a git repo
# with the right history checked out), so it's runnable/testable outside
# CI.
#
# Usage: changelog.sh <new-tag>
set -euo pipefail

NEW_TAG="${1:?usage: changelog.sh <new-tag>}"

PREV_TAG="$(git describe --tags --abbrev=0 --match 'v*' 2>/dev/null || true)"

echo "## $NEW_TAG"
echo ""
if [[ -n "$PREV_TAG" ]]; then
    echo "Changes since $PREV_TAG:"
    echo ""
    git log "${PREV_TAG}..HEAD" --no-merges --pretty=format:"- %s (%h)"
else
    echo "Initial release."
    echo ""
    git log --no-merges --pretty=format:"- %s (%h)"
fi
echo ""
