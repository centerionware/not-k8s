#!/usr/bin/env python3
"""Cheap contract checks; these do not claim real release/bootstrap success."""
import hashlib
import importlib.util
import os
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('summary', ROOT / 'deploy/release-validation-summary.py')
summary = importlib.util.module_from_spec(spec)
spec.loader.exec_module(summary)


class ReleaseValidationTests(unittest.TestCase):
    def test_e2e_journal_retains_api_watch_traces_without_audit_noise(self):
        action = yaml.safe_load((ROOT / '.github/actions/e2e-run/action.yml').read_text())
        script = next(step['run'] for step in action['runs']['steps']
                      if step['name'] == 'Save controller logs')
        fixture = '''
sudo() {
    if [[ " $* " == *" -u nodeapiserver "* ]]; then
        printf '%s\\n' 'unrelated audit request'
        if [[ "$TRACE_PRESENT" == 1 ]]; then
            printf '%s\\n' 'old namespace watch event boundary=storage_to_cache revision=42'
            printf '%s\\n' 'old namespace watch event boundary=cache_to_http revision=42'
        fi
        return "$JOURNAL_EXIT"
    fi
    printf '%s\\n' 'controller history'
}
'''
        with tempfile.TemporaryDirectory() as temporary:
            journal = Path(temporary) / 'journal.txt'
            script = script.replace('/tmp/e2e-controller-journal.txt', str(journal))
            for present in ('0', '1'):
                with self.subTest(trace_present=present):
                    result = subprocess.run(['bash', '-c', fixture + script], capture_output=True,
                                            text=True, env=dict(os.environ, TRACE_PRESENT=present,
                                                               JOURNAL_EXIT='0'))
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, journal.read_text())
                    self.assertIn('controller history', result.stdout)
                    self.assertNotIn('unrelated audit request', result.stdout)
                    for boundary in ('storage_to_cache', 'cache_to_http'):
                        self.assertEqual(f'boundary={boundary} revision=42' in result.stdout,
                                         present == '1')
            failure = subprocess.run(['bash', '-c', fixture + script], capture_output=True,
                                     env=dict(os.environ, TRACE_PRESENT='0', JOURNAL_EXIT='1'))
            self.assertNotEqual(failure.returncode, 0)

    def test_bootstrap_symlink_replaces_links_without_following_directories(self):
        action = yaml.safe_load((ROOT / '.github/actions/e2e-run/action.yml').read_text())
        script = next(step['run'] for step in action['runs']['steps'] if step['name'] == 'Bootstrap cluster')
        command = next(line.strip() for line in script.splitlines() if 'dist-bin/nodebootstrap' in line and 'ln ' in line)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'dist-bin').mkdir()
            (root / 'old').mkdir()
            (root / 'dist-bin/nodebootstrap').symlink_to(root / 'old', target_is_directory=True)
            for _ in range(2):
                subprocess.run(['bash', '-ec', command], cwd=root, check=True)
                self.assertEqual(os.readlink(root / 'dist-bin/nodebootstrap'), 'notk8s')
                self.assertEqual(list((root / 'old').iterdir()), [])

    def test_flannel_manifest_checksum_fails_before_mutation(self):
        script = (ROOT / 'deploy/setup-profile-baseline.sh').read_text()
        check = script[script.index('    FLANNEL_MANIFEST_SHA256='):script.index("    sed -i 's@10.244")]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'flannel.yml').write_text('tampered manifest\n')
            result = subprocess.run(['bash', '-ec', check], env=dict(os.environ, work=str(root)),
                                    capture_output=True)
            self.assertNotEqual(result.returncode, 0)

    def test_release_publication_reuses_tags_and_assets_after_partial_failure(self):
        workflow = yaml.safe_load((ROOT / '.github/workflows/release.yml').read_text())
        script = next(step['run'] for step in workflow['jobs']['publish-release']['steps']
                      if step.get('id') == 'publish')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            remote = root / 'remote.git'
            source = root / 'source'
            state = root / 'release'
            tools = root / 'tools'
            tools.mkdir()
            subprocess.run(['git', 'init', '--bare', str(remote)], check=True, capture_output=True)
            subprocess.run(['git', 'init', str(source)], check=True, capture_output=True)
            for key, value in [('user.name', 'fixture'), ('user.email', 'fixture@example.invalid')]:
                subprocess.run(['git', '-C', str(source), 'config', key, value], check=True)
            subprocess.run(['git', '-C', str(source), 'remote', 'add', 'origin', str(remote)], check=True)
            subprocess.run(['git', '-C', str(source), 'commit', '--allow-empty', '-m', 'fixture'],
                           check=True, capture_output=True)
            (source / 'dist').mkdir()
            (source / 'dist/notk8s-fixture').write_text('immutable binary fixture\n')
            (source / 'release-notes.md').write_text('fixture\n')
            (tools / 'gh').write_text(f'#!{sys.executable}\n' +
                'import os, pathlib, shutil, sys\n'
                'state = pathlib.Path(os.environ["RELEASE_STATE"])\n'
                'args = sys.argv[1:]\n'
                'operation = args[1]\n'
                'if operation == "view":\n'
                '    if not state.exists(): sys.exit(1)\n'
                '    print("\\n".join(p.name for p in state.iterdir()))\n'
                'elif operation == "create":\n'
                '    state.mkdir()\n'
                '    for name in args:\n'
                '        if name.startswith("dist/"): shutil.copy(name, state)\n'
                'elif operation == "download":\n'
                '    shutil.copy(state / "SHA256SUMS", args[args.index("--dir") + 1])\n'
                'elif operation == "upload":\n'
                '    path = pathlib.Path(args[3])\n'
                '    assert not (state / path.name).exists(), "must not overwrite existing assets"\n'
                '    shutil.copy(path, state)\n'
                'else: sys.exit(2)\n')
            (tools / 'gh').chmod(0o755)
            env = dict(os.environ, PATH=f"{tools}:{os.environ['PATH']}", VERSION='0.8.0',
                       RELEASE_STATE=str(state), GITHUB_OUTPUT=str(root / 'outputs'))
            def publish():
                return subprocess.run(['bash', '-c', script], cwd=source, env=env,
                                      capture_output=True, text=True)
            first = publish()
            self.assertEqual(first.returncode, 0, first.stderr)
            repeat = publish()
            self.assertEqual(repeat.returncode, 0, repeat.stderr)
            (state / 'notk8s-fixture').unlink()  # Simulate an interrupted asset upload.
            partial = publish()
            self.assertEqual(partial.returncode, 0, partial.stderr)
            self.assertEqual((state / 'notk8s-fixture').read_text(), 'immutable binary fixture\n')
            self.assertEqual((root / 'outputs').read_text().splitlines(), ['published=true'] * 3)
            (source / 'dist/notk8s-fixture').write_text('different rebuilt binary\n')
            self.assertNotEqual(publish().returncode, 0)
            self.assertEqual((state / 'notk8s-fixture').read_text(), 'immutable binary fixture\n')
            subprocess.run(['git', '-C', str(source), 'commit', '--allow-empty', '-m', 'different source'],
                           check=True, capture_output=True)
            self.assertIn('refusing to retag', publish().stderr)

    def test_stack_inputs_reject_malformed_and_out_of_range_values(self):
        action = yaml.safe_load((ROOT / '.github/actions/profile-stack-run/action.yml').read_text())
        script = next(step['run'] for step in action['runs']['steps'] if step['name'] == 'Record inputs')
        script = script.split('mkdir -p profile-data/bootstrap')[0]
        for field, values in {'PROFILE_SECONDS': ('abc', '', '29', '601', '1+30'),
                              'PROFILE_ARCHIVE_LIMIT_MIB': ('abc', '', '63', '2049', '64;true')}.items():
            for value in values:
                env = dict(os.environ, PROFILE_BUILD='profiling', PROFILE_WORKLOAD='heavy',
                           PROFILE_SECONDS='30', PROFILE_ARCHIVE_LIMIT_MIB='64')
                env[field] = value
                with self.subTest(field=field, value=value):
                    result = subprocess.run(['bash', '-c', script], env=env, capture_output=True)
                    self.assertNotEqual(result.returncode, 0)
        env = dict(os.environ, PROFILE_BUILD='profiling', PROFILE_WORKLOAD='heavy',
                   PROFILE_SECONDS='600', PROFILE_ARCHIVE_LIMIT_MIB='2048')
        self.assertEqual(subprocess.run(['bash', '-c', script], env=env).returncode, 0)

    def test_comparison_builds_cannot_publish_or_receive_a_write_token(self):
        workflow = yaml.safe_load((ROOT / '.github/workflows/profile-compare.yml').read_text())
        self.assertEqual(workflow['permissions']['contents'], 'read')
        measure = workflow['jobs']['measure']
        self.assertNotIn('permissions', measure)
        self.assertNotIn('GH_TOKEN', measure.get('env', {}))
        action = yaml.safe_load((ROOT / '.github/actions/profile-compare-leg/action.yml').read_text())
        for step in action['runs']['steps']:
            if 'GH_TOKEN' in step.get('env', {}):
                self.assertEqual(step['name'], 'Get release binary')
                self.assertIn("inputs.release_tag != ''", step['if'])
            self.assertNotIn('publish-', step.get('run', ''))
        for job in ('publish', 'report'):
            self.assertEqual(workflow['jobs'][job]['permissions']['contents'], 'write')
        release = yaml.safe_load((ROOT / '.github/workflows/release.yml').read_text())
        self.assertEqual(release['permissions']['contents'], 'read')
        self.assertNotIn('permissions', release['jobs']['release-comparison'])

    def test_shell_check_does_not_ignore_a_later_broken_script(self):
        workflow = yaml.safe_load((ROOT / '.github/workflows/profiling-check.yml').read_text())
        script = next(step['run'] for step in workflow['jobs']['scripts']['steps']
                      if step['name'] == 'Check profile shell')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'deploy/lib').mkdir(parents=True)
            (root / 'deploy/a-profile.sh').write_text('true\n')
            (root / 'deploy/lib/profiling-report.sh').write_text('if then\n')
            (root / 'deploy/lib/render-perf.sh').write_text('true\n')
            result = subprocess.run(['bash', '-c', script], cwd=root, capture_output=True)
            self.assertNotEqual(result.returncode, 0)

    def test_concurrent_profile_indexes_preserve_all_unique_results(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            remote = root / 'remote.git'
            subprocess.run(['git', 'init', '--bare', str(remote)], check=True, capture_output=True)
            config = root / 'gitconfig'
            subprocess.run(['git', 'config', '--file', str(config),
                            f'url.{remote}.insteadOf', 'https://github.com/fixture/results.git'], check=True)
            (root / 'gh').write_text('#!/bin/sh\nexit 0\n')
            (root / 'gh').chmod(0o755)
            env = dict(os.environ, PATH=str(root) + ':' + os.environ['PATH'], GH_TOKEN='fixture',
                       GITHUB_REPOSITORY='fixture/results', GITHUB_RUN_ID='initial', GITHUB_RUN_ATTEMPT='1',
                       PROFILE_SHA='fixture', PROFILE_RESULTS_BRANCH='e2e-prof-v0.8.0',
                       GIT_CONFIG_GLOBAL=str(config), GIT_CONFIG_NOSYSTEM='1')
            payload = root / 'payload'
            payload.mkdir()
            (payload / 'metadata.txt').write_text('fixture\n')
            (payload / 'README.md').write_text('comparison fixture\n')
            comparison = ['bash', str(ROOT / 'deploy/publish-profile-comparison.sh'), 'report', str(payload)]
            # A comparison must be able to create an absent, custom results branch.
            first = subprocess.run(comparison, env=env, capture_output=True, text=True)
            self.assertEqual(first.returncode, 0, first.stderr)
            real_git = shutil.which('git')
            # All publishers must finish committing before any can push. This
            # deterministically exercises shared-pointer rebase conflicts.
            gate = root / 'gate'
            gate.mkdir()
            (root / 'git').write_text('#!/bin/bash\n'
                'if [[ " $* " == *" push "* && ! -e "$GATE/$GITHUB_RUN_ID" ]]; then\n'
                '  touch "$GATE/$GITHUB_RUN_ID"\n'
                '  for attempt in {1..200}; do\n'
                '    entries=("$GATE"/*); [[ ${#entries[@]} -eq 4 ]] && break\n'
                '    sleep .05\n  done\nfi\nexec "$REAL_GIT" "$@"\n')
            (root / 'git').chmod(0o755)
            writers = []
            for index in range(4):
                command = comparison if index < 2 else ['bash', str(ROOT / 'deploy/publish-stack-profile.sh'), str(payload)]
                writers.append(subprocess.Popen(command, env=dict(env, GITHUB_RUN_ID=str(index),
                    GATE=str(gate), REAL_GIT=real_git), stdout=subprocess.PIPE, stderr=subprocess.PIPE))
            for writer in writers:
                _, error = writer.communicate(timeout=60)
                self.assertEqual(writer.returncode, 0, error.decode())
            tree = subprocess.check_output([real_git, '--git-dir', str(remote), 'ls-tree', '-r', '--name-only',
                                            env['PROFILE_RESULTS_BRANCH']], text=True).splitlines()
            for index in range(2):
                self.assertIn(f'comparisons/{index}-1/README.md', tree)
            self.assertEqual(len([p for p in tree if p.startswith('history/') and p.endswith('/metadata.txt')]), 2)
            self.assertIn('latest-stack.md', tree)
            self.assertIn('latest-comparison.md', tree)

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

    def test_manual_e2e_results_are_scoped_to_run_and_attempt(self):
        workflow = yaml.safe_load((ROOT / '.github/workflows/e2e.yml').read_text())
        prepare = next(step['run'] for step in workflow['jobs']['prepare-e2e-results']['steps']
                       if step.get('name') == 'Prepare results')
        self.assertNotIn('--force', prepare)
        self.assertNotIn('checkout --orphan', prepare)
        self.assertIn('history/$GITHUB_RUN_ID-$GITHUB_RUN_ATTEMPT', prepare)
        action = next(step for step in workflow['jobs']['e2e']['steps']
                      if step.get('uses') == './.github/actions/e2e-run')
        self.assertEqual(action['with']['results_prefix'],
                         'history/${{ github.run_id }}-${{ github.run_attempt }}')

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
            self.assertLessEqual(jobs[name]['timeout-minutes'], 120)
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
                 'release-comparison', 'release-comparison-publish', 'release-comparison-report')
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
