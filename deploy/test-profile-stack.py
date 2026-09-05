#!/usr/bin/env python3
"""Fast contract checks; no cluster, Rust build, privileges, or actual perf."""
import importlib.util
import csv
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("profile_stack", ROOT / "deploy/profile-stack.py")
stack = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(stack)
CHART_SPEC = importlib.util.spec_from_file_location("stack_charts", ROOT / "deploy/lib/render-stack-charts.py")
charts = importlib.util.module_from_spec(CHART_SPEC)
CHART_SPEC.loader.exec_module(charts)


class ProfileStackTests(unittest.TestCase):
    def test_heavy_job_requires_completion_and_foreground_gc(self):
        workload = stack.Workload(Path("unused"), 10, 4, "heavy")
        calls = []
        def kubectl(*args, **kwargs):
            calls.append((args, kwargs))
            if args[0] == "delete":
                workload.stop.set()
        with patch.object(workload, "kubectl", side_effect=kubectl):
            workload.job_worker()
        self.assertEqual(calls[0][1]["body"]["kind"], "Job")
        self.assertIn("--for=condition=Complete", calls[1][0])
        self.assertIn("--cascade=foreground", calls[2][0])
        self.assertIn("--wait=true", calls[2][0])

    def test_heavy_job_failure_cannot_be_reported_as_completed_work(self):
        workload = stack.Workload(Path("unused"), 10, 4, "heavy")
        with patch.object(workload, "kubectl", side_effect=["", RuntimeError("Job never completed")]):
            with self.assertRaisesRegex(RuntimeError, "Job never completed"):
                workload.job_worker()

    def test_upstream_mapping_and_k3s_monolith(self):
        for component, upstream in stack.UPSTREAM.items():
            self.assertEqual(stack.component_name([f"/usr/bin/{upstream}"], "k8s"), component)
        self.assertEqual(stack.component_name(["k3s server"], "k3s"), "k3s")
        self.assertIsNone(stack.component_name(["/usr/bin/containerd"], "k3s", True))
        self.assertEqual(stack.component_name(["containerd"], "k3s", True,
                                             "/var/lib/rancher/k3s/data/hash/bin/containerd"), "containerd")
        self.assertIsNone(stack.component_name(["kubelet"], "notk8s"))
        self.assertEqual(stack.component_name(["containerd "], "k3s", True,
            "/usr/local/bin/k3s", "0::/system.slice/k3s.service\n"), "containerd")
        self.assertIsNone(stack.component_name(["containerd"], "k3s", True,
            "/usr/bin/containerd", "0::/system.slice/containerd.service\n"))
        self.assertIsNone(stack.component_name(["containerd-shim-runc-v2"], "k3s", True,
            "/var/lib/rancher/k3s/data/hash/bin/containerd-shim-runc-v2", "0::/system.slice/k3s.service\n"))

    def test_embedded_component_comparison_is_rejected_before_setup(self):
        result = subprocess.run(["python3", str(ROOT / "deploy/profile-stack.py"),
                                 "--backend", "k3s", "--metrics-only", "--output", "unused"],
                                text=True, capture_output=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("only whole-stack", result.stderr)

    def test_multiple_processes_sum_and_missing_pss_is_not_zero(self):
        rows = [{"component": "coredns", "elapsed_seconds": "1", "rss_kib": "10", "pss_kib": ""},
                {"component": "coredns", "elapsed_seconds": "1", "rss_kib": "20", "pss_kib": "5"}]
        self.assertEqual(charts.component_series(rows, "coredns", "rss_kib"), [(1.0, 30.0)])
        self.assertIsNone(charts.component_series(rows, "coredns", "pss_kib"))
        with self.assertRaisesRegex(ValueError, "missing component"):
            charts.component_series(rows, "nodelet", "rss_kib")

    def test_duplicate_pid_cannot_inflate_aggregate(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "duplicate.csv"
            path.write_text('elapsed_seconds,pid,component\n1,42,nodelet\n1,42,k3s\n')
            with self.assertRaisesRegex(ValueError, "duplicate PID"):
                charts.load_rows(path)

    @unittest.skipUnless(importlib.util.find_spec("matplotlib"), "chart dependency installed in CI only")
    def test_component_and_whole_stack_graphs_render_without_fake_k3s_slots(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            sources = {label: root / label for label in ("notk8s", "k8s", "k3s")}
            for label, directory in sources.items():
                for phase in ("idle", "load"):
                    (directory / phase).mkdir(parents=True)
                    with (directory / phase / "timeseries.csv").open("w") as stream:
                        writer = csv.writer(stream)
                        writer.writerow(['elapsed_seconds', 'pid', 'component', 'cpu_pct_one_core', 'rss_kib', 'pss_kib'])
                        names = ['k3s'] if label == 'k3s' else ['nodeapiserver', 'nodestore']
                        for second, rss in ((1.0, 1024), (1.4, 4096), (2.1, 1024)):
                            for pid, name in enumerate(names, 1):
                                writer.writerow([second, pid, name, 2, rss, 512])
            charts.render(root / "whole", sources, None, True)
            charts.render(root / "selected", {k: v for k, v in sources.items() if k != "k3s"}, ['nodeapiserver', 'nodestore'])
            for mode in ('whole', 'selected'):
                self.assertTrue((root / mode / 'load-combined-cpu_pct_one_core.png').is_file())
                self.assertTrue((root / mode / 'load-nodeapiserver-rss_kib.png').is_file())
            self.assertIn('not separately attributable', (root / 'whole/chart-notes.txt').read_text())
            with (root / 'whole/summary.csv').open() as stream:
                stats = list(csv.DictReader(stream))
            value = next(row for row in stats if row['phase'] == 'load' and row['stack'] == 'notk8s'
                         and row['component'] == 'combined' and row['metric'] == 'rss_kib')
            self.assertEqual(value['unit'], 'MiB')
            self.assertEqual([float(value[k]) for k in ('min', 'mean', 'max')], [2, 4, 8])

    def test_exact_process_identity(self):
        self.assertEqual(stack.component_name(["/bin/nodelet"]), "nodelet")
        self.assertEqual(stack.component_name(["/bin/notk8s", "nodecontroller"]), "nodecontroller")
        self.assertIsNone(stack.component_name(["sh", "-c", "/bin/nodelet"]))
        self.assertIsNone(stack.component_name(["python", "--pattern", "nodelet"]))
        self.assertIsNone(stack.component_name(["/bin/notk8s", "nodebootstrap"]))

    def test_cpu_stat_handles_spaces_and_parentheses(self):
        fields = ["0"] * 22
        fields[0], fields[11], fields[12], fields[19], fields[21] = "S", "10", "5", "30", "2"
        with patch.object(Path, "read_text", side_effect=["123 (with ) spaces) " + " ".join(fields), "Pss: 3 kB\n"]):
            with patch.object(os, "sysconf", return_value=4096):
                self.assertEqual(stack.counters(123), (15, 8, 3, "30"))

    def test_workload_errors_are_recorded_and_not_suppressed(self):
        workload = stack.Workload(Path("unused"), 1, 1)
        result = subprocess.CompletedProcess([], 1, "", "Forbidden")
        with patch.object(subprocess, "run", return_value=result):
            with self.assertRaisesRegex(RuntimeError, "Forbidden"):
                workload.kubectl("get", "pods")
        self.assertEqual(workload.events[0]["exit_code"], 1)

    def test_stopped_worker_does_not_issue_more_requests(self):
        workload = stack.Workload(Path("unused"), 1, 1)
        workload.stop.set()
        with patch.object(workload, "kubectl") as kubectl:
            workload.api_worker(0)
            kubectl.assert_not_called()

    def test_perf_header_is_not_success_when_record_exits_nonzero(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            perf = directory / "perf"
            perf.write_text('#!/bin/sh\nwhile [ "$#" -gt 0 ]; do\n'
                            'if [ "$1" = -o ]; then shift; printf header > "$1"; fi\nshift\ndone\nexit 1\n')
            perf.chmod(0o755)
            env = dict(os.environ, PATH=f"{directory}:{os.environ['PATH']}", PROFILE_REQUIRE_PERF="1")
            # Some development sandboxes expose host /proc with a nested
            # PID namespace. Use the identity in the mounted procfs.
            pid = Path("/proc/self/stat").read_text().split(" ", 1)[0]
            result = subprocess.run(["bash", str(ROOT / "deploy/profile-process.sh"),
                                     "--pid", pid, "--duration", "1", "--output", str(directory / "out")],
                                    env=env, text=True, capture_output=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("refusing to label fallback data", result.stderr)


if __name__ == "__main__":
    unittest.main()
