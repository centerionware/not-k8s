#!/usr/bin/env bash
# Publish one complete compressed bundle, small browseable summaries and SVGs
# to the existing results branch. No Actions artifact or token-bearing URL.
set -euo pipefail
out=${1:?profile directory required}
[[ -d "$out" && -f "$out/metadata.txt" ]] || { echo 'missing profiling metadata' >&2; exit 1; }
: "${GH_TOKEN:?}" "${GITHUB_REPOSITORY:?}" "${GITHUB_RUN_ID:?}" "${PROFILE_SHA:?}"
budget=${PROFILE_ARCHIVE_LIMIT_MIB:-512}
[[ "$budget" =~ ^[0-9]+$ ]] && (( budget >= 64 && budget <= 2048 )) || exit 2
branch=${PROFILE_RESULTS_BRANCH:-profiling-results}
git check-ref-format --branch "$branch" >/dev/null
work=$(mktemp -d)
archive="$work/profile.tar.gz"
tar -czf "$archive" -C "$out" .
size=$(stat -c %s "$archive")
(( size <= budget * 1024 * 1024 )) || {
    echo "complete archive is $size bytes, above ${budget} MiB budget; refusing partial publication" >&2
    exit 1
}
gh auth setup-git
stamp="$(date -u +%Y-%m-%d_%H-%M-%S)-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT:-1}-stack"
if git ls-remote --exit-code --heads "https://github.com/$GITHUB_REPOSITORY.git" "$branch" >/dev/null; then
    git clone --filter=blob:none --no-checkout --depth=1 --single-branch --branch "$branch" "https://github.com/$GITHUB_REPOSITORY.git" "$work/results"
    git -C "$work/results" sparse-checkout set --no-cone /README.md /latest-stack.md "/history/$stamp/"
    git -C "$work/results" checkout "$branch"
else
    git init "$work/results"
    git -C "$work/results" checkout --orphan "$branch"
    git -C "$work/results" remote add origin "https://github.com/$GITHUB_REPOSITORY.git"
fi
dest="$work/results/history/$stamp"
mkdir -p "$dest"
split -b 48M -d -a 3 "$archive" "$dest/profile.tar.gz.part-"
(cd "$dest" && sha256sum profile.tar.gz.part-* > SHA256SUMS)
cp "$out/metadata.txt" "$dest/"
while IFS= read -r -d '' file; do
    relative=${file#"$out/"}
    mkdir -p "$dest/$(dirname "$relative")"
    cp "$file" "$dest/$relative"
done < <(find "$out" -type f \( -name '*.svg' -o -name '*.png' -o -name '*.csv' -o -name 'SUMMARY.txt' -o -name 'no-samples.txt' -o -name 'workload.json' -o -name 'workload-config.json' \) -print0)
cat > "$dest/README.md" <<EOF
# Stack CPU profile

- Source: \`$PROFILE_SHA\`
- Run: ${GITHUB_SERVER_URL:-https://github.com}/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID
- Build: ${PROFILE_BUILD:-unknown}; capture result: ${PROFILE_RESULT:-unknown}
- Workload: ${PROFILE_WORKLOAD:-standard} (see workload-config.json for parameters)
- Complete compressed bundle: $size bytes; parts are below GitHub's per-file limit.

This is one single-node diagnostic sample, not conformance, a release performance
ratio, or a statistical benchmark. Six runtime PIDs are sampled together. The
bootstrap applet is captured separately. The load generator and perf share the
host. Inspect workload errors and restart checks before interpreting CPU numbers.

The archive includes raw perf data, decoded stacks, per-process CPU/RSS/PSS series,
workload operations, symbolized executable, build identity, and diagnostics.
An empty folded-stack file is reported as no samples, not zero CPU usage.
Exact min/mean/max values are in [charts/summary.csv](charts/summary.csv).
Memory units are MiB; CPU is percent of one logical CPU. Chart whiskers show
the observed range, not a confidence interval.

Download all parts and SHA256SUMS into an empty directory, then:

\`\`\`sh
sha256sum -c SHA256SUMS
cat profile.tar.gz.part-* | tar -xz
\`\`\`

Use the included matching executable with \`perf report --symfs\` if re-analyzing
on another host. The bundle retains the original absolute executable layout under
\`symfs/\`; rendered SVGs and text reports need no symbol setup.
EOF
printf '\n## Browseable files\n\n' >> "$dest/README.md"
while IFS= read -r file; do
    relative=${file#"$dest/"}
    printf -- '- [%s](%s)\n' "$relative" "$relative" >> "$dest/README.md"
done < <(find "$dest" -type f \( -name '*.svg' -o -name 'timeseries.csv' -o -name 'SUMMARY.txt' -o -name 'no-samples.txt' \) | sort)
printf '\n## Charts and flame graphs\n\n' >> "$dest/README.md"
while IFS= read -r file; do
    relative=${file#"$dest/"}
    printf '### %s\n\n![%s](%s)\n\n' "$relative" "$relative" "$relative" >> "$dest/README.md"
done < <(find "$dest" -type f \( -name '*.svg' -o -name '*.png' \) | sort)
# A link, not a second copy of hundreds of MiB. Preserve the legacy latest/
# comparison directory and its history when publishing this optional mode.
printf '# Latest stack profile\n\n[Results](history/%s/README.md)\n' "$stamp" > "$work/results/latest-stack.md"
if [[ ! -f "$work/results/README.md" ]]; then
    printf '# not-k8s profiling results\n\n[Latest stack profile](latest-stack.md)\n' > "$work/results/README.md"
elif ! grep -q 'latest-stack.md' "$work/results/README.md"; then
    printf '\n[Latest stack profile](latest-stack.md)\n' >> "$work/results/README.md"
fi
git -C "$work/results" config user.name not-k8s-profiling-bot
git -C "$work/results" config user.email actions@users.noreply.github.com
git -C "$work/results" add history/"$stamp" latest-stack.md README.md
git -C "$work/results" commit -m "perf: record stack profile ${PROFILE_SHA:0:12} run $GITHUB_RUN_ID"
# Never force-push measurement history. A concurrent publisher is retried
# through rebase; a conflict is visible rather than silently losing results.
for attempt in {1..10}; do
    if git -C "$work/results" push origin "HEAD:$branch"; then
        printf '[Stack profile results](https://github.com/%s/tree/%s/history/%s)\n' "$GITHUB_REPOSITORY" "$branch" "$stamp" >> "${GITHUB_STEP_SUMMARY:-/dev/stdout}"
        exit 0
    fi
    git -C "$work/results" fetch origin "$branch"
    git -C "$work/results" rebase FETCH_HEAD
    sleep "$((attempt < 6 ? attempt : 5))"
done
exit 1
