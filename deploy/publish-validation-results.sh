#!/usr/bin/env bash
# Publish only this job's payload from a separate checkout. Never force-push
# or switch the source checkout out from under subsequent composite steps.
set -euo pipefail
data=${1:?payload directory}
branch=${2:?results branch}
prefix=${3:-}
: "${GH_TOKEN:?}" "${GITHUB_REPOSITORY:?}"
git check-ref-format --branch "$branch" >/dev/null
[[ -d "$data" && "$prefix" != /* && "$prefix" != *..* ]] || exit 2
work=$(mktemp -d)
gh auth setup-git
remote="https://github.com/$GITHUB_REPOSITORY.git"
if git ls-remote --exit-code --heads "$remote" "$branch" >/dev/null; then
    git clone --filter=blob:none --no-checkout --depth=1 --single-branch --branch "$branch" "$remote" "$work/results"
    # A root payload only needs top-level index/status files, not old perf archives.
    patterns=('/*' '!/*/')
    [[ -z "$prefix" ]] || patterns+=("/$prefix/")
    git -C "$work/results" sparse-checkout set --no-cone "${patterns[@]}"
    git -C "$work/results" checkout "$branch"
else
    git init "$work/results"
    git -C "$work/results" checkout --orphan "$branch"
    git -C "$work/results" remote add origin "$remote"
fi
mkdir -p "$work/results/$prefix"
cp -a "$data/." "$work/results/$prefix/"
git -C "$work/results" config user.name github-actions\[bot\]
git -C "$work/results" config user.email github-actions\[bot\]@users.noreply.github.com
git -C "$work/results" add -- "${prefix:-.}"
if git -C "$work/results" diff --cached --quiet; then exit 0; fi
git -C "$work/results" commit -m "test(results): publish validation data for run ${GITHUB_RUN_ID:-local}"
for attempt in {1..10}; do
    if git -C "$work/results" push origin "HEAD:$branch"; then exit 0; fi
    git -C "$work/results" fetch origin "$branch"
    git -C "$work/results" rebase FETCH_HEAD
    sleep "$((attempt < 6 ? attempt : 5))"
done
echo 'result publication failed after concurrent-write retries' >&2
exit 1
