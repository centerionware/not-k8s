#!/usr/bin/env python3
"""Render only complete comparison runs; never present failed legs as zero."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--input', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    backends = json.loads(os.environ['PROFILE_BACKENDS'])
    whole = os.environ['PROFILE_WHOLE'] == 'true'
    selected = os.environ['PROFILE_COMPONENTS']
    args.output.mkdir(parents=True, exist_ok=True)
    command = [sys.executable, str(Path(__file__).parent / 'lib/render-stack-charts.py'),
               '--output', str(args.output / 'charts')]
    for backend in backends:
        command += ['--series', f'{backend}={args.input / backend}']
    command += ['--whole-stack'] if whole else ['--components', selected]
    subprocess.run(command, check=True)
    text = ['# Kubernetes stack comparison', '',
            f'Measured scope: {"distribution daemons" if whole else selected}.', '',
            'CPU and memory counters only: no perf or flamegraph collection during this comparison.',
            'Each stack runs on an independent hosted runner with the same bounded workload: a ready',
            'HTTP Deployment, service traffic, ConfigMap API/watch churn and replica scaling.',
            'This is one sample per stack, not a statistical claim. Compare hardware metadata and',
            'workload errors/operation counts before interpreting differences. Components are measured',
            'inside their respective full stacks, not swapped into an otherwise identical control plane.', '',
            'Canonical chart labels map nodestore→etcd, nodeapiserver→kube-apiserver,',
            'nodescheduler→kube-scheduler, nodecontroller→kube-controller-manager, nodelet→kubelet,',
            'and nodeproxy→kube-proxy. k3s embeds these components: they are not separately attributable.', '',
            'Whole-stack totals include these distribution daemons plus containerd, Flannel and CoreDNS',
            '(k3s embeds Flannel). They exclude workload containers, runtime shims, the load generator',
            'and unrelated host services. RSS sums can double-count shared pages; PSS apportions them.',
            'Missing values are unavailable, never zero. Component-mode totals include only selected components.', '',
            '[Exact min/mean/max values (CSV)](charts/summary.csv). Memory values are MiB;',
            'CPU values are percent of one logical CPU. Whiskers show observed range, not confidence intervals.', '',
            '## Source data', '']
    text += [f'- [{backend} metadata]({backend}/metadata.txt), [workload]({backend}/workload.json), '
             f'[idle CSV]({backend}/idle/timeseries.csv), [load CSV]({backend}/load/timeseries.csv)' for backend in backends]
    text += ['', '## Component and combined graphs', '']
    for image in sorted((args.output / 'charts').glob('*.png')):
        text += [f'### {image.stem}', '', f'![{image.stem}](charts/{image.name})', '']
    (args.output / 'README.md').write_text('\n'.join(text) + '\n')


if __name__ == '__main__':
    main()
