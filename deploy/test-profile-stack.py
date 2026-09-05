#!/usr/bin/env python3
"""Fast contract checks; no cluster, Rust build, privileges, or actual perf."""
import importlib.util
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


class ProfileStackTests(unittest.TestCase):
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
