#!/usr/bin/env python3
"""Cheap contract checks; these do not claim real release/bootstrap success."""
import hashlib
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('summary', ROOT / 'deploy/release-validation-summary.py')
summary = importlib.util.module_from_spec(spec)
spec.loader.exec_module(summary)


class ReleaseValidationTests(unittest.TestCase):
    def test_ci_display_names_fit_the_actions_ui(self):
        paths = sorted((ROOT / '.github/workflows').glob('*.yml'))
        paths += sorted((ROOT / '.github/actions').glob('*/action.yml'))
        for path in paths:
            document = yaml.safe_load(path.read_text())
            labels = [('title', document.get('name'))]
            groups = document.get('jobs', {'action': document.get('runs', {})})
            for job_id, job in groups.items():
                if 'name' in job:
                    labels.append((job_id, job['name']))
                for index, step in enumerate(job.get('steps', [])):
                    labels.append((f'{job_id} step {index + 1}', step.get('name')))
            for location, label in labels:
                with self.subTest(path=str(path.relative_to(ROOT)), location=location):
                    self.assertIsInstance(label, str, 'Explicit display name required')
                    self.assertTrue(label.strip())
                    self.assertNotIn('${{', label, 'Use a bounded, static display name')
                    self.assertLessEqual(len(label), 20, label)

    def test_concurrent_result_writers_preserve_every_shard(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            remote = root / 'remote.git'
            subprocess.run(['git', 'init', '--bare', str(remote)], check=True, capture_output=True)
            config = root / 'gitconfig'
            subprocess.run(['git', 'config', '--file', str(config),
                            f'url.{remote}.insteadOf', 'https://github.com/fixture/results.git'], check=True)
            gh = root / 'gh'
            gh.write_text('#!/bin/sh\nexit 0\n')
            gh.chmod(0o755)
            env = dict(os.environ, PATH=str(root) + ':' + os.environ['PATH'], GH_TOKEN='fixture',
                       GITHUB_REPOSITORY='fixture/results', GITHUB_RUN_ID='fixture',
                       GIT_CONFIG_GLOBAL=str(config), GIT_CONFIG_NOSYSTEM='1')
            publisher = ['bash', str(ROOT / 'deploy/publish-validation-results.sh')]
            initial = root / 'initial'
            initial.mkdir()
            (initial / 'README.md').write_text('validation running\n')
            first = subprocess.run(publisher + [str(initial), 'e2e-prof-v0.8.0'], env=env, capture_output=True)
            self.assertEqual(first.returncode, 0, first.stderr)
            writers = []
            for shard in range(5):
                payload = root / f'payload-{shard}'
                payload.mkdir()
                (payload / f'results-shard-{shard}.txt').write_text(f'shard {shard}\n')
                writers.append(subprocess.Popen(publisher + [str(payload), 'e2e-prof-v0.8.0', 'e2e/fixture'],
                                                env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE))
            for writer in writers:
                _, stderr = writer.communicate(timeout=60)
                self.assertEqual(writer.returncode, 0, stderr)
            for shard in range(5):
                result = subprocess.run(['git', '--git-dir', str(remote), 'show',
                    f'e2e-prof-v0.8.0:e2e/fixture/results-shard-{shard}.txt'], capture_output=True, text=True)
                self.assertEqual(result.stdout, f'shard {shard}\n', result.stderr)

    def test_release_graph_is_post_publication_and_has_no_workflow_calls(self):
        text = (ROOT / '.github/workflows/release.yml').read_text()
        workflow = yaml.safe_load(text)
        jobs = workflow['jobs']
        self.assertNotIn('skip_e2e', text)
        self.assertNotIn('profile_after_release', text)
        self.assertFalse(any('uses' in job for job in jobs.values()))
        self.assertEqual(jobs['build-release']['needs'], ['build-and-test', 'release-identity'])
        for name in ('release-e2e', 'release-flamegraphs', 'release-comparison'):
            self.assertIn('prepare-validation', jobs[name]['needs'])
            self.assertIn('publish-release', jobs[name]['needs'])
            self.assertLessEqual(jobs[name]['timeout-minutes'], 360)
            self.assertNotIn('max-parallel', jobs[name].get('strategy', {}))
        self.assertEqual(jobs['release-comparison']['strategy']['matrix']['backend'], ['notk8s', 'k8s', 'k3s'])
        for name in ('release-flamegraphs', 'release-comparison'):
            self.assertEqual(jobs[name]['env']['PROFILE_WORKLOAD'], 'heavy')
            self.assertEqual(jobs[name]['env']['PROFILE_SECONDS'], '300')
        self.assertIn('e2e-prof-v$VERSION', text)

    def test_composite_actions_have_explicit_shells_and_no_caller_matrix(self):
        for path in (ROOT / '.github/actions').glob('*/action.yml'):
            text = path.read_text()
            action = yaml.safe_load(text)
            self.assertNotIn('matrix.', text, str(path))
            self.assertNotIn('secrets.', text, str(path))
            for step in action['runs']['steps']:
                if 'run' in step:
                    self.assertEqual(step['shell'], 'bash', str(path))
                    # Check shell syntax even in conditional/unexercised paths.
                    result = subprocess.run(['bash', '-n'], input=step['run'], text=True, capture_output=True)
                    self.assertEqual(result.returncode, 0, f'{path}: {result.stderr}')

    def test_missing_skipped_cancelled_or_failed_job_is_not_success(self):
        names = ('prepare-validation', 'release-e2e', 'release-flamegraphs',
                 'release-comparison', 'release-comparison-report')
        results = {name: {'result': 'success'} for name in names}
        self.assertEqual(summary.render('v0.8.0', 'owner/repo', '12', '1', results)[0], 'success')
        for name in names:
            for outcome in ('failure', 'skipped', 'cancelled'):
                changed = dict(results, **{name: {'result': outcome}})
                self.assertEqual(summary.render('v0.8.0', 'owner/repo', '12', '1', changed)[0], 'failure')
        self.assertEqual(summary.render('v0.8.0', 'owner/repo', '12', '1', {})[0], 'failure')

    def test_download_uses_exact_tag_and_rejects_corruption(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = root / 'fixture'
            fixture.mkdir()
            asset = 'notk8s-0.8.0-linux-x86_64-release'
            data = b'test fixture; never executed\n'
            (fixture / asset).write_bytes(data)
            (fixture / 'SHA256SUMS').write_text(hashlib.sha256(data).hexdigest() + '  ' + asset + '\n')
            gh = root / 'gh'
            gh.write_text('''#!/usr/bin/env bash
set -euo pipefail
[[ "$1 $2 $3" == 'release download v0.8.0' ]] || exit 91
while [[ $# -gt 0 ]]; do
  if [[ "$1" == --dir ]]; then cp "$FIXTURE/"* "$2/"; exit; fi
  shift
done
exit 92
''')
            gh.chmod(0o755)
            env = dict(os.environ, PATH=str(root) + ':' + os.environ['PATH'], GH_TOKEN='fixture',
                       GITHUB_REPOSITORY='owner/repo', FIXTURE=str(fixture))
            command = ['bash', str(ROOT / 'deploy/download-release-runtime.sh'), 'v0.8.0', 'release']
            good = subprocess.run(command + [str(root / 'good')], env=env, capture_output=True)
            self.assertEqual(good.returncode, 0, good.stderr)
            self.assertEqual((root / 'good/notk8s').read_bytes(), data)
            (fixture / asset).write_bytes(b'corrupted')
            bad = subprocess.run(command + [str(root / 'bad')], env=env, capture_output=True)
            self.assertNotEqual(bad.returncode, 0)
            self.assertFalse((root / 'bad/notk8s').exists())


if __name__ == '__main__':
    unittest.main()
