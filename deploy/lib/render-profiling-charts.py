#!/usr/bin/env python3
"""render-profiling-charts.py — aggregate N replicate runs per node agent
(measure.sh's per-second timeseries CSVs + summary.txt MEASURE_* blocks)
into charts and a markdown stats table, for the nodelet-vs-real-upstream-
kubelet profiling comparison.

Round 124: built after two mermaid xychart-beta attempts turned out to be
riskier than warranted — that grammar has no reliable multi-series legend
support, and its exact syntax isn't something this environment could
actually render-test before publishing. matplotlib is a much more solid
choice: real legends, mean±range bands across replicates, and it's trivial
to `pip install` in CI. A single run's numbers can be noisy (one GC cycle
landing inside vs. just outside the window, a slow syscall, whatever) —
this script is built around N replicates per agent from the start, not a
single sample, and reports the spread honestly rather than only a point
estimate.

Usage:
    render-profiling-charts.py <out_dir> \
        --series "nodelet=csv1,csv2,csv3" \
        --series "upstream kubelet=csv1,csv2,csv3" \
        --summary "nodelet=summary1.txt,summary2.txt,summary3.txt" \
        --summary "upstream kubelet=summary1.txt,summary2.txt,summary3.txt"

Writes <out_dir>/rss-over-time.png, <out_dir>/cpu-over-time.png, and
<out_dir>/stats.md. Any input file that's missing/empty/unparseable is
skipped, not fabricated — everything downstream just works with whatever
replicates actually loaded, and reports on stdout how many that was.
"""
import argparse
import csv
import os
import re
import sys

import matplotlib

matplotlib.use("Agg")  # headless — no display available in CI
import matplotlib.pyplot as plt  # noqa: E402
import numpy as np  # noqa: E402


def load_timeseries(path):
    """(seconds, rss_mb, cpu_pct) numpy arrays from one measure.sh CSV, or
    None if the file is missing/empty/unparseable."""
    if not path or not os.path.isfile(path):
        return None
    seconds, rss_mb, cpu_pct = [], [], []
    with open(path, newline="") as f:
        for row in csv.DictReader(f):
            try:
                seconds.append(int(row["second"]))
                rss_mb.append(float(row["rss_kb"]) / 1024.0)
                cpu_pct.append(float(row["cpu_pct"]))
            except (KeyError, ValueError):
                continue
    if not seconds:
        return None
    return np.array(seconds), np.array(rss_mb), np.array(cpu_pct)


def load_summary_vars(path):
    """MEASURE_<key>=value pairs from one measure.sh summary.txt, as a
    dict — only the block between the === MEASURE === markers, so this
    can't accidentally pick up something from the human-readable table
    above it."""
    if not path or not os.path.isfile(path):
        return {}
    out = {}
    in_block = False
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line == "=== MEASURE ===":
                in_block = True
                continue
            if line == "=== END MEASURE ===":
                break
            if in_block and "=" in line:
                k, v = line.split("=", 1)
                out[k.removeprefix("MEASURE_")] = v
    return out


def parse_float(value):
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def align_and_stack(series_list):
    """Given a list of (seconds, values) arrays (one per replicate),
    truncate all to the shortest common length and stack into a 2D array
    (replicate, second) so mean/min/max across replicates is a simple
    axis-0 reduction. Replicates should all share the same SAMPLE_SECS in
    practice; truncating instead of erroring keeps one short/interrupted
    replicate from sinking the whole chart."""
    if not series_list:
        return None, None
    min_len = min(len(s) for _, s in series_list)
    if min_len == 0:
        return None, None
    seconds = series_list[0][0][:min_len]
    stacked = np.stack([values[:min_len] for _, values in series_list])
    return seconds, stacked


def render_band_chart(agents, value_key, ylabel, title, out_path):
    """agents: [(label, [(seconds, rss_mb, cpu_pct), ...replicates]), ...].
    One shaded mean±range band + mean line per agent, all on one chart
    with a real legend — this is exactly the multi-series comparison
    mermaid's xychart-beta couldn't reliably do."""
    fig, ax = plt.subplots(figsize=(9, 4.5), dpi=140)
    plotted = 0
    for label, replicates in agents:
        idx = 1 if value_key == "rss" else 2
        series_list = [(r[0], r[idx]) for r in replicates]
        seconds, stacked = align_and_stack(series_list)
        if seconds is None:
            continue
        mean = stacked.mean(axis=0)
        lo = stacked.min(axis=0)
        hi = stacked.max(axis=0)
        (line,) = ax.plot(seconds, mean, label=f"{label} (n={len(replicates)})", linewidth=1.8)
        ax.fill_between(seconds, lo, hi, color=line.get_color(), alpha=0.15)
        plotted += 1
    if plotted == 0:
        plt.close(fig)
        return False
    ax.set_xlabel("elapsed seconds")
    ax.set_ylabel(ylabel)
    ax.set_title(f"{title}\n(shaded band = min–max across replicates, line = mean)")
    ax.set_ylim(bottom=0)
    ax.grid(True, alpha=0.3)
    ax.legend(loc="upper right")
    fig.tight_layout()
    fig.savefig(out_path)
    plt.close(fig)
    return True


def replicate_stats(values):
    values = [v for v in values if v is not None]
    if not values:
        return None
    arr = np.array(values)
    return {"mean": arr.mean(), "min": arr.min(), "max": arr.max(), "n": len(arr)}


def fmt_stat(stats, decimals=1):
    if stats is None:
        return "N/A"
    fmt = f"%.{decimals}f"
    if stats["n"] == 1:
        return fmt % stats["mean"]
    return f"{fmt % stats['mean']} (range {fmt % stats['min']}–{fmt % stats['max']}, n={stats['n']})"


def render_stats_table(agents_summaries, out_path):
    """agents_summaries: [(label, [summary_dict, ...replicates]), ...].
    Writes a markdown fragment: one aggregated (mean + range) row per
    agent, plus a full per-replicate raw table underneath for anyone who
    wants to see the individual runs behind the aggregate."""
    metrics = [
        ("RSS (MB)", "NODELET_RSS_MB", 1),
        ("avg CPU %", "NODELET_CPU_AVG_PCT", 2),
        ("CPU-seconds used", "NODELET_CPU_SECONDS", 3),
        ("cycles", "NODELET_CYCLES", 0),
        ("instructions", "NODELET_INSTRUCTIONS", 0),
        ("IPC", "NODELET_IPC", 3),
    ]

    lines = []
    lines.append("| agent | " + " | ".join(m[0] for m in metrics) + " |")
    lines.append("|---|" + "---|" * len(metrics))
    for label, summaries in agents_summaries:
        cells = []
        for _, key, decimals in metrics:
            values = [parse_float(s.get(key)) for s in summaries]
            cells.append(fmt_stat(replicate_stats(values), decimals))
        lines.append(f"| **{label}** | " + " | ".join(cells) + " |")

    lines.append("")
    lines.append("<details><summary>Individual replicate runs</summary>")
    lines.append("")
    lines.append("| agent | replicate | " + " | ".join(m[0] for m in metrics) + " |")
    lines.append("|---|---|" + "---|" * len(metrics))
    for label, summaries in agents_summaries:
        for i, s in enumerate(summaries, 1):
            cells = []
            for _, key, decimals in metrics:
                v = parse_float(s.get(key))
                cells.append(("%." + str(decimals) + "f") % v if v is not None else "N/A")
            lines.append(f"| {label} | {i} | " + " | ".join(cells) + " |")
    lines.append("")
    lines.append("</details>")

    with open(out_path, "w") as f:
        f.write("\n".join(lines) + "\n")


def parse_labeled_list_arg(spec):
    """"label=path1,path2,path3" -> (label, [path1, path2, path3])."""
    if "=" not in spec:
        raise ValueError(f"expected LABEL=PATH1,PATH2,... got: {spec}")
    label, rest = spec.split("=", 1)
    return label, [p for p in re.split(r"\s*,\s*", rest) if p]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("out_dir")
    parser.add_argument(
        "--series", action="append", default=[], metavar="LABEL=CSV1,CSV2,...",
        help="repeatable; one per agent, comma-separated replicate timeseries CSV paths",
    )
    parser.add_argument(
        "--summary", action="append", default=[], metavar="LABEL=TXT1,TXT2,...",
        help="repeatable; one per agent, comma-separated replicate summary.txt paths",
    )
    args = parser.parse_args()
    os.makedirs(args.out_dir, exist_ok=True)

    agents_timeseries = []
    for spec in args.series:
        label, paths = parse_labeled_list_arg(spec)
        replicates = []
        for p in paths:
            data = load_timeseries(p)
            if data is None:
                print(f"no usable timeseries for '{label}' at {p} — skipping this replicate")
                continue
            replicates.append(data)
        print(f"'{label}': loaded {len(replicates)}/{len(paths)} timeseries replicate(s)")
        agents_timeseries.append((label, replicates))

    agents_summaries = []
    for spec in args.summary:
        label, paths = parse_labeled_list_arg(spec)
        summaries = []
        for p in paths:
            v = load_summary_vars(p)
            if not v:
                print(f"no usable summary for '{label}' at {p} — skipping this replicate")
                continue
            summaries.append(v)
        print(f"'{label}': loaded {len(summaries)}/{len(paths)} summary replicate(s)")
        agents_summaries.append((label, summaries))

    rss_path = os.path.join(args.out_dir, "rss-over-time.png")
    cpu_path = os.path.join(args.out_dir, "cpu-over-time.png")
    stats_path = os.path.join(args.out_dir, "stats.md")

    rss_ok = render_band_chart(agents_timeseries, "rss", "RSS (MB)", "RSS over time", rss_path)
    cpu_ok = render_band_chart(agents_timeseries, "cpu", "CPU %", "CPU % over time", cpu_path)
    render_stats_table(agents_summaries, stats_path)

    print(f"RSS chart: {'written to ' + rss_path if rss_ok else 'skipped (no data)'}")
    print(f"CPU chart: {'written to ' + cpu_path if cpu_ok else 'skipped (no data)'}")
    print(f"stats table: written to {stats_path}")

    if not any(replicates for _, replicates in agents_timeseries):
        sys.exit(1)


if __name__ == "__main__":
    main()
