#!/usr/bin/env bash
# profiling-report.sh — render a nodelet-vs-stock-k3s idle CPU%/RSS
# comparison as a GitHub-flavored-markdown table plus a dependency-free
# ASCII bar chart (block characters in a code fence — renders reliably in
# a GitHub Actions step summary without needing matplotlib/an image
# host). Pure text in, text out, so this is testable without a real
# runner.
#
# Usage: profiling-report.sh <notk8s_cpu> <notk8s_rss_mb> <stock_cpu> <stock_rss_mb>
set -euo pipefail

NOTK8S_CPU="${1:?usage: profiling-report.sh <notk8s_cpu> <notk8s_rss_mb> <stock_cpu> <stock_rss_mb>}"
NOTK8S_RSS="${2:?}"
STOCK_CPU="${3:?}"
STOCK_RSS="${4:?}"

# Proportional bar: `value` scaled against `max_value` into a bar `width`
# characters wide (minimum 1 char if value > 0, so a real-but-small number
# doesn't silently render as nothing).
bar() {
    local value="$1" max_value="$2" width="$3"
    awk -v v="$value" -v m="$max_value" -v w="$width" 'BEGIN {
        if (m <= 0) { n = 0 }
        else {
            n = int((v / m) * w + 0.5)
            if (n < 1 && v > 0) n = 1
        }
        s = ""
        for (i = 0; i < n; i++) s = s "█"
        printf "%s", s
    }'
}

CPU_MAX="$(awk -v a="$NOTK8S_CPU" -v b="$STOCK_CPU" 'BEGIN { print (a > b) ? a : b }')"
RSS_MAX="$(awk -v a="$NOTK8S_RSS" -v b="$STOCK_RSS" 'BEGIN { print (a > b) ? a : b }')"
CPU_SAVINGS="$(awk -v a="$NOTK8S_CPU" -v b="$STOCK_CPU" 'BEGIN { if (b <= 0) { print "N/A" } else { printf "%.0f%%", (1 - a / b) * 100 } }')"
RSS_SAVINGS="$(awk -v a="$NOTK8S_RSS" -v b="$STOCK_RSS" 'BEGIN { if (b <= 0) { print "N/A" } else { printf "%.0f%%", (1 - a / b) * 100 } }')"

cat <<EOF
## Idle resource footprint: not-k8s vs stock k3s (real kubelet)

30s idle steady-state sample, same runner, same containerd. "not-k8s" is
nodelet + a stripped \`k3s server --disable-agent\`; "stock k3s" is a
default single-node \`k3s server\` (no disables — its embedded agent's
real kubelet is running).

| | CPU % | RSS (MB) |
|---|---|---|
| not-k8s (nodelet + stripped k3s) | ${NOTK8S_CPU}% | ${NOTK8S_RSS} |
| stock k3s (real kubelet) | ${STOCK_CPU}% | ${STOCK_RSS} |
| **savings** | **${CPU_SAVINGS}** | **${RSS_SAVINGS}** |

\`\`\`
CPU %   not-k8s   $(bar "$NOTK8S_CPU" "$CPU_MAX" 40) ${NOTK8S_CPU}%
        stock k3s $(bar "$STOCK_CPU" "$CPU_MAX" 40) ${STOCK_CPU}%

RSS MB  not-k8s   $(bar "$NOTK8S_RSS" "$RSS_MAX" 40) ${NOTK8S_RSS} MB
        stock k3s $(bar "$STOCK_RSS" "$RSS_MAX" 40) ${STOCK_RSS} MB
\`\`\`
EOF
