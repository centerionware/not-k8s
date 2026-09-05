#!/usr/bin/env bash
# Render only after every simultaneous capture has stopped: symbolization
# can otherwise become competing load inside another component's sample.
set -euo pipefail
out=${1:?output directory required}
label=${2:?profile label required}
tools_dir=${FLAMEGRAPH_DIR:?set FLAMEGRAPH_DIR to the pinned toolkit directory}
perf script --no-inline -i "$out/perf.data" > "$out/perf.script" 2> "$out/perf-script.txt"
perf report --stdio --no-inline -i "$out/perf.data" --sort comm,dso,symbol --percent-limit 0 > "$out/perf-report.txt" 2>&1
perf report --stdio --no-children --no-inline -i "$out/perf.data" --percent-limit 0 > "$out/perf-self-report.txt" 2>&1
if command -v rustfilt >/dev/null 2>&1; then
    rustfilt < "$out/perf.script" > "$out/perf-rustfilt.script"
    input="$out/perf-rustfilt.script"
else
    input="$out/perf.script"
fi
perl "$tools_dir/stackcollapse-perf.pl" "$input" > "$out/out.folded" 2> "$out/stackcollapse.err"
if [[ -s "$out/out.folded" ]]; then
    perl "$tools_dir/flamegraph.pl" --title "$label CPU samples" "$out/out.folded" > "$out/flamegraph.svg"
else
    echo 'No sampled stacks; this is not evidence of zero CPU usage.' > "$out/no-samples.txt"
fi
