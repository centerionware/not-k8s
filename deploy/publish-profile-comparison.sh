#!/usr/bin/env bash
# Small metrics only, never Actions build artifacts. Matrix legs publish unique
# paths; the dependent report job combines them after every leg succeeds.
set -euo pipefail
mode=${1:?leg or report}
data=${2:?data directory}
backend=${3:-}
: "${GH_TOKEN:?}" "${GITHUB_REPOSITORY:?}" "${GITHUB_RUN_ID:?}"
case "$mode:$backend" in leg:notk8s|leg:k8s|leg:k3s|report:) ;; *) exit 2 ;; esac
work=$(mktemp -d)
gh auth setup-git
git clone --depth=1 --single-branch --branch profiling-results "https://github.com/$GITHUB_REPOSITORY.git" "$work/results"
relative="comparisons/$GITHUB_RUN_ID-${GITHUB_RUN_ATTEMPT:-1}"
dest="$work/results/$relative"
if [[ "$mode" == leg ]]; then
    mkdir -p "$dest/$backend"
    cp -a "$data/." "$dest/$backend/"
else
    mkdir -p "$dest/charts"
    cp -a "$data/." "$dest/"
    printf '# Latest stack comparison\n\n[Results](%s/README.md)\n' "$relative" > "$work/results/latest-comparison.md"
fi
git -C "$work/results" config user.name not-k8s-profiling-bot
git -C "$work/results" config user.email actions@users.noreply.github.com
git -C "$work/results" add "$relative"
[[ "$mode" != report ]] || git -C "$work/results" add latest-comparison.md
git -C "$work/results" commit -m "perf: record $mode comparison ${backend:-charts} run $GITHUB_RUN_ID"
for attempt in 1 2 3 4; do
    if git -C "$work/results" push origin HEAD:profiling-results; then
        printf '[Comparison data](https://github.com/%s/tree/profiling-results/%s)\n' "$GITHUB_REPOSITORY" "$relative" >> "${GITHUB_STEP_SUMMARY:-/dev/stdout}"
        exit 0
    fi
    git -C "$work/results" fetch origin profiling-results
    git -C "$work/results" rebase FETCH_HEAD
done
exit 1
