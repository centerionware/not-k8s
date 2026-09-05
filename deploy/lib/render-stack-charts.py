#!/usr/bin/env python3
"""Render per-component and PID-deduplicated aggregate CPU/RSS/PSS charts."""
import argparse
import csv
from pathlib import Path


def load_rows(path):
    with open(path, newline="") as stream:
        rows = list(csv.DictReader(stream))
    if not rows:
        raise ValueError(f"empty measurements: {path}")
    seen = set()
    for row in rows:
        key = (row["elapsed_seconds"], row["pid"])
        if key in seen:
            raise ValueError(f"duplicate PID within one sample: {key}")
        seen.add(key)
    return rows


def component_series(rows, component, metric):
    selected = [row for row in rows if row["component"] == component]
    if not selected:
        raise ValueError(f"missing component: {component}")
    if any(row[metric] in ("", "None") for row in selected):
        return None
    # Multiple processes (e.g. CoreDNS replicas) can own the same slot.
    totals = {}
    for row in selected:
        second = float(row["elapsed_seconds"])
        totals[second] = totals.get(second, 0) + float(row[metric])
    return sorted(totals.items())


def render(output, sources, selected):
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    import numpy as np
    output.mkdir(parents=True, exist_ok=True)
    metrics = [("cpu_pct_one_core", "CPU (% of one logical CPU)", 1),
               ("rss_kib", "RSS (MiB; shared pages may be counted repeatedly)", 1024),
               ("pss_kib", "PSS (MiB; proportional shared-page accounting)", 1024)]
    notes = []
    for phase in ("idle", "load"):
        rows = {label: load_rows(root / phase / "timeseries.csv") for label, root in sources.items()}
        components = selected or sorted(set(row["component"] for row in next(iter(rows.values()))))
        for metric, ylabel, divisor in metrics:
            aggregates = {}
            summaries = {}
            for component in components:
                fig, ax = plt.subplots(figsize=(10, 4), dpi=140)
                plotted = False
                for label, samples in rows.items():
                    values = component_series(samples, component, metric)
                    if values is None:
                        notes.append(f"{phase}/{component}/{label}: {metric} unavailable, not zero")
                        continue
                    x, y = zip(*values)
                    y = np.asarray(y) / divisor
                    ax.plot(x, y, label=label)
                    plotted = True
                    aggregates.setdefault(label, []).append((x, y))
                    summaries.setdefault(label, {})[component] = float(np.mean(y))
                if plotted:
                    ax.set(xlabel="Seconds since phase start", ylabel=ylabel, title=f"{phase}: {component}")
                    ax.legend(); ax.grid(alpha=.2); fig.tight_layout()
                    fig.savefig(output / f"{phase}-{component}-{metric}.png")
                plt.close(fig)
            fig, ax = plt.subplots(figsize=(10, 4), dpi=140)
            plotted = False
            for label, series in aggregates.items():
                if len(series) != len(components):
                    notes.append(f"{phase}/{label}: aggregate {metric} omitted because a component is missing")
                    continue
                end = min(x[-1] for x, _ in series)
                start = max(x[0] for x, _ in series)
                grid = np.linspace(start, end, max(2, int(end - start) + 1))
                total = sum(np.interp(grid, x, y) for x, y in series)
                ax.plot(grid, total, label=label)
                plotted = True
            if plotted:
                ax.set(xlabel="Seconds since phase start", ylabel=ylabel,
                       title=f"{phase}: selected components combined ({', '.join(components)})")
                ax.legend(); ax.grid(alpha=.2); fig.tight_layout()
                fig.savefig(output / f"{phase}-combined-{metric}.png")
            plt.close(fig)
            fig, ax = plt.subplots(figsize=(12, 5), dpi=140)
            count = max(1, len(summaries))
            for index, (label, values) in enumerate(summaries.items()):
                if set(values) != set(components):
                    continue
                ax.bar(np.arange(len(components)) + index * .8 / count,
                       [values[component] for component in components], width=.8 / count, label=label)
            ax.set_xticks(np.arange(len(components)) + .4, components, rotation=25, ha="right")
            ax.set(ylabel=f"Mean {ylabel}", title=f"{phase}: component comparison")
            if ax.containers:
                ax.legend(); fig.tight_layout()
                fig.savefig(output / f"{phase}-summary-{metric}.png")
            plt.close(fig)
    (output / "chart-notes.txt").write_text("\n".join(sorted(set(notes))) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--series", action="append", required=True, help="label=profile-directory")
    parser.add_argument("--components", default="", help="comma-separated canonical component names")
    args = parser.parse_args()
    sources = dict(item.split("=", 1) for item in args.series)
    render(args.output, {label: Path(path) for label, path in sources.items()},
           args.components.split(",") if args.components else None)


if __name__ == "__main__":
    main()
