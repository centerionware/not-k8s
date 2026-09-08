#!/usr/bin/env python3
"""Final index for the immutable release identity, including incomplete jobs."""
import json
import os
from pathlib import Path


def render(tag, repository, run, attempt, results):
    required = ('prepare-validation', 'release-e2e', 'release-flamegraphs',
                'release-comparison', 'release-comparison-publish', 'release-comparison-report')
    passed = all(results.get(name, {}).get('result') == 'success' for name in required)
    conclusion = 'success' if passed else 'failure'
    lines = [f'# {tag} validation', '', f'**Validation: {conclusion.upper()}**', '',
             f'[Release](https://github.com/{repository}/releases/tag/{tag}) · '
             f'[Workflow](https://github.com/{repository}/actions/runs/{run})', '',
             '| Job | Result |', '| --- | --- |']
    lines += [f"| {name} | {results.get(name, {}).get('result', 'missing')} |" for name in required]
    lines += ['', f'[Full e2e shard logs](e2e/{run}-{attempt}/)', '',
              '[Latest stack flamegraphs](latest-stack.md)', '',
              '[Latest three-way comparison](latest-comparison.md)', '',
              f"[This attempt's comparison data](comparisons/{run}-{attempt}/)", '',
              'Links may be absent when their producing job failed before publication.',
              'Latest-profile links can refer to an earlier attempt; check run/attempt identity.', '',
              'Both profiles use heavy load with 300-second idle and loaded windows.',
              'E2e and comparisons execute the checksum-verified published release runtime.',
              'Flamegraphs execute its additional optimized symbolized diagnostic asset.',
              'Single-run measurements are diagnostic evidence, not universal performance claims.', '']
    return conclusion, '\n'.join(lines)


if __name__ == '__main__':
    results = json.loads(os.environ['VALIDATION_RESULTS'])
    conclusion, readme = render(os.environ['RELEASE_TAG'], os.environ['GITHUB_REPOSITORY'],
                                os.environ['GITHUB_RUN_ID'], os.environ['GITHUB_RUN_ATTEMPT'], results)
    output = Path('validation-summary')
    output.mkdir(exist_ok=True)
    (output / 'README.md').write_text(readme)
    (output / 'conclusion.txt').write_text(conclusion + '\n')
    (output / 'jobs.json').write_text(json.dumps(results, indent=2) + '\n')
