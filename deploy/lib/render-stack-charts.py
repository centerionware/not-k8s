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


def render(output, sources, selected, whole=False):
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    import numpy as np
    output.mkdir(parents=True, exist_ok=True)
    metrics = [("cpu_pct_one_core", "CPU (% of one logical CPU)", 1),
               ("rss_kib", "RSS (MiB; shared pages may be counted repeatedly)", 1024),
               ("pss_kib", "PSS (MiB; proportional shared-page accounting)", 1024)]
    notes = []
    statistics = []
    def describe(phase, label, component, metric, values, divisor):
        low, mean, high = float(np.min(values)), float(np.mean(values)), float(np.max(values))
        statistics.append([phase, label, component, metric, 'MiB' if divisor == 1024 else '% of one CPU', low, mean, high])
        return f"{label}: min {low:.2f} | mean {mean:.2f} | max {high:.2f}"
    for phase in ("idle", "load"):
        rows = {label: load_rows(root / phase / "timeseries.csv") for label, root in sources.items()}
        available = {label: {row["component"] for row in samples} for label, samples in rows.items()}
        components = selected or sorted(set.union(*available.values()))
        for metric, ylabel, divisor in metrics:
            aggregates = {}
            summaries = {}
            for component in components:
                fig, ax = plt.subplots(figsize=(10, 4), dpi=140)
                plotted = False
                for label, samples in rows.items():
                    if whole and component not in available[label]:
                        notes.append(f"{phase}/{component}/{label}: not separately attributable, not zero")
                        continue
                    values = component_series(samples, component, metric)
                    if values is None:
                        notes.append(f"{phase}/{component}/{label}: {metric} unavailable, not zero")
                        continue
                    x, y = zip(*values)
                    y = np.asarray(y) / divisor
                    ax.plot(x, y, label=describe(phase, label, component, metric, y, divisor))
                    plotted = True
                    aggregates.setdefault(label, []).append((x, y))
                    summaries.setdefault(label, {})[component] = (float(np.min(y)), float(np.mean(y)), float(np.max(y)))
                if plotted:
                    ax.set(xlabel="Seconds since phase start", ylabel=ylabel, title=f"{phase}: {component}")
                    ax.legend(); ax.grid(alpha=.2); fig.tight_layout()
                    fig.savefig(output / f"{phase}-{component}-{metric}.png")
                plt.close(fig)
            fig, ax = plt.subplots(figsize=(10, 4), dpi=140)
            plotted = False
            for label, series in aggregates.items():
                expected = available[label] if whole else set(components)
                if len(series) != len(expected):
                    notes.append(f"{phase}/{label}: aggregate {metric} omitted because a component is missing")
                    continue
                grid = series[0][0]
                if any(x != grid for x, _ in series):
                    raise ValueError(f"{label}/{phase}: component sample timestamps do not align")
                # Preserve actual sample peaks; interpolation onto a new grid
                # can smooth away the maxima users need for capacity planning.
                total = sum(y for _, y in series)
                ax.plot(grid, total, label=describe(phase, label, 'combined', metric, total, divisor))
                plotted = True
            if plotted:
                ax.set(xlabel="Seconds since phase start", ylabel=ylabel,
                       title=f"{phase}: {'distribution daemons' if whole else 'selected components'} combined")
                ax.legend(); ax.grid(alpha=.2); fig.tight_layout()
                fig.savefig(output / f"{phase}-combined-{metric}.png")
            plt.close(fig)
            fig, ax = plt.subplots(figsize=(14, 7), dpi=140)
            count = max(1, len(summaries))
            for index, (label, values) in enumerate(summaries.items()):
                triples = [values.get(component, (float('nan'),) * 3) for component in components]
                means = [v[1] for v in triples]
                bars = ax.bar(np.arange(len(components)) + index * .8 / count,
                       means, yerr=[[v[1]-v[0] for v in triples], [v[2]-v[1] for v in triples]],
                       capsize=3, width=.8 / count, label=label)
                for bar, (low, mean, high) in zip(bars, triples):
                    if np.isfinite(mean):
                        ax.annotate(f"{mean:.2f}\n[{low:.2f}, {high:.2f}]",
                            (bar.get_x()+bar.get_width()/2, high), xytext=(0, 4),
                            textcoords='offset points', ha='center', va='bottom', fontsize=7, rotation=30)
            ax.margins(y=.3)
            ax.set_xticks(np.arange(len(components)) + .4, components, rotation=25, ha="right")
            ax.set(ylabel=f"Mean {ylabel}", title=f"{phase}: component comparison — mean [min, max]; whiskers show range")
            if ax.containers:
                ax.legend(); fig.tight_layout()
                fig.savefig(output / f"{phase}-summary-{metric}.png")
            plt.close(fig)
    (output / "chart-notes.txt").write_text("\n".join(sorted(set(notes))) + "\n")
    with (output / "summary.csv").open('w', newline='') as stream:
        writer = csv.writer(stream)
        writer.writerow(['phase', 'stack', 'component', 'metric', 'unit', 'min', 'mean', 'max'])
        writer.writerows(statistics)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--series", action="append", required=True, help="label=profile-directory")
    parser.add_argument("--components", default="", help="comma-separated canonical component names")
    parser.add_argument("--whole-stack", action="store_true")
    args = parser.parse_args()
    sources = dict(item.split("=", 1) for item in args.series)
    render(args.output, {label: Path(path) for label, path in sources.items()},
           args.components.split(",") if args.components else None, args.whole_stack)


if __name__ == "__main__":
    main()
