#!/usr/bin/env bash
# version-bump.sh — read-then-bump against the `version` orphan branch's
# single VERSION file (MAJOR.MINOR.PATCH). Prints the version THIS build
# should use to stdout, then commits+pushes the incremented PATCH back to
# the version branch for the next build. MAJOR/MINOR are never touched
# here — bump those manually (edit VERSION on the version branch
# directly) ahead of a release that should carry one.
#
# Usage: version-bump.sh <repo-url>
# Requires: git configured with push access to <repo-url> (relies on the
# ambient credential helper / token already set up by the caller — in CI,
# actions/checkout's own git config).
set -euo pipefail

REPO_URL="${1:?usage: version-bump.sh <repo-url>}"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

git clone --quiet --branch version --depth 1 "$REPO_URL" "$WORK_DIR/version-branch" >&2

CURRENT="$(cat "$WORK_DIR/version-branch/VERSION")"
echo "using version $CURRENT for this build" >&2

IFS='.' read -r major minor patch <<<"$CURRENT"
NEXT="$major.$minor.$((patch + 1))"

echo "$NEXT" > "$WORK_DIR/version-branch/VERSION"
cd "$WORK_DIR/version-branch"
git config user.name "github-actions[bot]"
git config user.email "github-actions[bot]@users.noreply.github.com"
git commit -aqm "bump version: $CURRENT -> $NEXT"
git push --quiet origin version >&2
echo "bumped version branch to $NEXT for the next build" >&2

# The only thing on stdout: the version THIS build/release should use.
echo "$CURRENT"
