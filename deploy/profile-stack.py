#!/usr/bin/env python3
"""Bounded, single-node diagnostic workload and simultaneous component captures.

Run only against a disposable profiling cluster. No third-party Python packages.
This is a repeatable workload sample, not Kubernetes conformance or a throughput
benchmark. The load generator and perf themselves consume runner resources.
"""
import argparse
import concurrent.futures
import csv
import json
import os
from pathlib import Path
import signal
import subprocess
import threading
import time

COMPONENTS = ("nodestore", "nodeapiserver", "nodescheduler", "nodecontroller", "nodelet", "nodeproxy")
UPSTREAM = dict(zip(COMPONENTS, ("etcd", "kube-apiserver", "kube-scheduler", "kube-controller-manager", "kubelet", "kube-proxy")))
INFRASTRUCTURE = ("containerd", "flanneld", "coredns")
ROOT = Path(__file__).resolve().parent.parent


def component_name(argv, backend="notk8s", whole=False, executable=None, cgroup=""):
    if not argv:
        return None
    name = Path(argv[0]).name.strip()
    if name == "notk8s" and len(argv) > 1:
        name = argv[1]
    if backend == "notk8s" and name in COMPONENTS:
        return name
    if backend == "k8s" and name in UPSTREAM.values():
        return next(key for key, value in UPSTREAM.items() if value == name)
    if backend == "k3s" and (name == "k3s server" or (name == "k3s" and argv[1:2] == ["server"])):
        return "k3s"
    if whole and name in INFRASTRUCTURE:
        # Hosted runners may also run Docker's unused containerd. k3s uses
        # its own bundled runtime, not that unrelated system process.
        if backend == "k3s" and name == "containerd":
            # Multi-call/rewritten process titles need not resolve to an
            # executable under the k3s data directory. Systemd ownership is
            # an independent identity check, excluding Docker's containerd.
            owned = any("k3s.service" in line.split(":", 2)[-1].split("/") for line in cgroup.splitlines())
            if not owned and not (executable or argv[0]).startswith("/var/lib/rancher/k3s/"):
                return None
        return name
    return None


def counters(pid):
    # comm may contain spaces and parentheses; split after its final ')'.
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
    ticks = int(fields[11]) + int(fields[12])
    rss = int(fields[21]) * os.sysconf("SC_PAGE_SIZE") // 1024
    pss = None
    try:
        for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines():
            if line.startswith("Pss:"):
                pss = int(line.split()[1])
    except (PermissionError, FileNotFoundError):
        pass
    return ticks, rss, pss, fields[19]


def processes(backend="notk8s", selected=COMPONENTS, whole=False):
    required = ("k3s",) if backend == "k3s" else selected
    if whole:
        required = tuple(required) + (("containerd", "coredns") if backend == "k3s" else INFRASTRUCTURE)
    found = {name: [] for name in required}
    runtime_candidates = []
    for path in Path("/proc").glob("[0-9]*/cmdline"):
        try:
            argv = path.read_bytes().decode(errors="replace").rstrip("\0").split("\0")
            executable = os.readlink(path.parent / "exe")
            cgroup = (path.parent / "cgroup").read_text()
            if argv and Path(argv[0]).name.strip() == "containerd":
                # Identity only: do not print command arguments that may contain credentials.
                runtime_candidates.append(dict(pid=int(path.parent.name), executable=executable,
                                               cgroup=cgroup.strip()))
            name = component_name(argv, backend, whole, executable, cgroup)
            if name in found:
                found[name].append(int(path.parent.name))
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            continue
    bad = {name: pids for name, pids in found.items()
           if not pids or (name not in INFRASTRUCTURE and len(pids) != 1)}
    if bad:
        raise RuntimeError(f"missing or ambiguous component processes: {bad}; containerd identities: {runtime_candidates}")
    return {f"{name}@{pid}": pid for name, pids in found.items() for pid in pids}


class Workload:
    def __init__(self, output, replicas, workers, preset="standard"):
        self.output, self.replicas, self.workers = output, replicas, workers
        self.preset = preset
        self.namespace = f"nk-profile-{os.getpid()}-{time.time_ns():x}"
        self.stop = threading.Event()
        self.lock = threading.Lock()
        self.events = []

    def kubectl(self, *args, body=None, timeout=45, request_timeout="30s"):
        started = time.monotonic()
        command = ["kubectl", f"--request-timeout={request_timeout}", "-n", self.namespace, *args]
        result = subprocess.run(command, input=json.dumps(body) if body else None,
                                text=True, capture_output=True, timeout=timeout)
        with self.lock:
            self.events.append({"time": time.time(), "operation": list(args),
                                "seconds": time.monotonic() - started,
                                "exit_code": result.returncode, "error": result.stderr[-2000:]})
        if result.returncode:
            raise RuntimeError(f"kubectl {args}: {result.stderr}")
        return result.stdout

    def setup(self):
        self.kubectl("create", "namespace", self.namespace)
        image = "busybox:1.37.0"
        server = {"apiVersion": "apps/v1", "kind": "Deployment",
                  "metadata": {"name": "profile-server"}, "spec": {
                      "replicas": self.replicas, "selector": {"matchLabels": {"app": "profile-server"}},
                      "template": {"metadata": {"labels": {"app": "profile-server"}}, "spec": {
                          "containers": [{"name": "http", "image": image,
                              "command": ["sh", "-c", "mkdir -p /www; echo profiling >/www/index.html; exec httpd -f -p 8080 -h /www"],
                              "readinessProbe": {"httpGet": {"path": "/", "port": 8080}},
                              "resources": {"requests": {"cpu": "10m", "memory": "16Mi"}}}]}}}}
        service = {"apiVersion": "v1", "kind": "Service", "metadata": {"name": "profile-server"},
                   "spec": {"selector": {"app": "profile-server"}, "ports": [{"port": 8080, "targetPort": 8080}]}}
        self.kubectl("apply", "-f", "-", body={"apiVersion": "v1", "kind": "List", "items": [server, service]})
        self.kubectl("rollout", "status", "deployment/profile-server", "--timeout=180s", timeout=190, request_timeout="0")

    def start_client(self):
        interval = "0.05" if self.preset == "heavy" else "0.2"
        client = {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "profile-client"},
                  "spec": {"restartPolicy": "Never", "containers": [{"name": "traffic", "image": "busybox:1.37.0",
                      "command": ["sh", "-c", f"while true; do wget -q -O /dev/null http://profile-server:8080/ || exit 1; echo request-ok; sleep {interval}; done"],
                      "resources": {"requests": {"cpu": "10m", "memory": "16Mi"}}}]}}
        self.kubectl("create", "-f", "-", body=client)
        self.kubectl("wait", "pod/profile-client", "--for=condition=Ready", "--timeout=90s", timeout=100, request_timeout="0")

    def api_worker(self, worker):
        counter = 0
        while not self.stop.is_set():
            name = f"profile-config-{worker}"
            self.kubectl("create", "configmap", name, f"--from-literal=counter={counter}")
            self.kubectl("get", "configmaps", "-o", "name")
            self.kubectl("patch", "configmap", name, "--type=merge", "-p", json.dumps({"data": {"counter": str(counter + 1)}}))
            self.kubectl("delete", "configmap", name, "--wait=false")
            counter += 1
            self.stop.wait(0.1 if self.preset == "heavy" else 1)

    def scale_worker(self):
        extra = True
        while not self.stop.wait(5 if self.preset == "heavy" else 15):
            burst = 5 if self.preset == "heavy" else 1
            self.kubectl("scale", "deployment/profile-server", f"--replicas={self.replicas + burst * int(extra)}")
            if self.preset == "heavy":
                self.kubectl("rollout", "status", "deployment/profile-server", "--timeout=90s", timeout=100, request_timeout="0")
            extra = not extra

    def job_worker(self):
        """Exercise scheduling, container/emptyDir lifecycle, Job status and GC."""
        counter = 0
        while not self.stop.is_set():
            name = f"profile-job-{counter}"
            job = {"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": name},
                   "spec": {"backoffLimit": 0, "template": {"spec": {
                       "restartPolicy": "Never", "volumes": [{"name": "scratch", "emptyDir": {}}],
                       "containers": [{"name": "work", "image": "busybox:1.37.0",
                           "command": ["sh", "-ec", "dd if=/dev/zero of=/data/payload bs=1024 count=1024; sha256sum /data/payload"],
                           "volumeMounts": [{"name": "scratch", "mountPath": "/data"}],
                           "resources": {"requests": {"cpu": "10m", "memory": "16Mi"}}}]}}}}
            self.kubectl("create", "-f", "-", body=job)
            self.kubectl("wait", f"job/{name}", "--for=condition=Complete", "--timeout=90s", timeout=100, request_timeout="0")
            self.kubectl("delete", f"job/{name}", "--cascade=foreground", "--wait=true", "--timeout=60s", timeout=70)
            counter += 1
            self.stop.wait(1)

    def save(self):
        (self.output / "workload-operations.json").write_text(json.dumps(self.events, indent=2))


def capture(output, phase, duration, backend="notk8s", selected=COMPONENTS, whole=False, flamegraphs=True):
    phase_dir = output / phase
    phase_dir.mkdir()
    pids = processes(backend, selected, whole)
    baseline = {name: counters(pid) for name, pid in pids.items()}
    identity = {name: {"pid": pid, "exe": os.readlink(f"/proc/{pid}/exe"),
                       "start_ticks": baseline[name][3]} for name, pid in pids.items()}
    (phase_dir / "processes.json").write_text(json.dumps(identity, indent=2))
    captures = []
    env = dict(os.environ, PROFILE_EVENT="cpu-clock", PROFILE_FREQUENCY="49",
               PROFILE_CALL_GRAPH="fp", PROFILE_CAPTURE_ONLY="1", PROFILE_REQUIRE_PERF="1")
    try:
        for key, pid in pids.items() if flamegraphs else []:
            name = key if whole else key.split("@", 1)[0]
            log = (phase_dir / f"{name}-capture.log").open("w")
            proc = subprocess.Popen(["bash", str(ROOT / "deploy/profile-process.sh"), "--pid", str(pid),
                                     "--label", name, "--duration", str(duration),
                                     "--output", str(phase_dir / name)], env=env, stdout=log,
                                    stderr=subprocess.STDOUT, start_new_session=True)
            captures.append((proc, log))
        started = time.monotonic()
        previous = baseline.copy()
        previous_time = started
        with (phase_dir / "timeseries.csv").open("w") as stream:
            writer = csv.writer(stream)
            writer.writerow(["elapsed_seconds", "component", "pid", "cpu_seconds", "cpu_pct_one_core", "rss_kib", "pss_kib"])
            while time.monotonic() - started < duration:
                time.sleep(1)
                now = time.monotonic()
                for name, pid in pids.items():
                    current = counters(pid)
                    if current[3] != baseline[name][3]:
                        raise RuntimeError(f"{name} restarted during {phase}; sample invalid")
                    delta = (current[0] - previous[name][0]) / os.sysconf("SC_CLK_TCK")
                    total = (current[0] - baseline[name][0]) / os.sysconf("SC_CLK_TCK")
                    writer.writerow([now - started, name.split("@", 1)[0], pid, total, 100 * delta / (now - previous_time), current[1], current[2]])
                    previous[name] = current
                stream.flush()
                previous_time = now
        for proc, _ in captures:
            if proc.wait(timeout=30):
                raise RuntimeError(f"perf capture failed during {phase}; see capture logs")
    finally:
        for proc, log in captures:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGINT)
                try:
                    proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(proc.pid, signal.SIGKILL)
                    proc.wait()
            log.close()


def run(args):
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    workload = Workload(output, args.replicas, args.workers, args.workload)
    (output / "workload-config.json").write_text(json.dumps({
        "preset": args.workload, "replicas": args.replicas, "api_workers": args.workers,
        "seconds_per_phase": args.seconds, "job_churn": args.workload == "heavy",
        "http_interval_seconds": 0.05 if args.workload == "heavy" else 0.2,
    }, indent=2))
    watcher = None
    watch_log = None
    failure = None
    try:
        workload.setup()
        capture(output, "idle", args.seconds, args.backend, args.components, args.whole_stack, not args.metrics_only)
        workload.start_client()
        watch_log = (output / "watch-events.txt").open("w")
        watcher = subprocess.Popen(["kubectl", "-n", workload.namespace, "get", "configmaps", "--watch-only"],
                                   stdout=watch_log, stderr=subprocess.STDOUT)
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers + 2) as pool:
            futures = [pool.submit(workload.api_worker, i) for i in range(args.workers)]
            futures.append(pool.submit(workload.scale_worker))
            if args.workload == "heavy":
                futures.append(pool.submit(workload.job_worker))
            try:
                capture(output, "load", args.seconds, args.backend, args.components, args.whole_stack, not args.metrics_only)
            finally:
                workload.stop.set()
            for future in futures:
                future.result()
        if watcher.poll() is not None:
            raise RuntimeError("ConfigMap watch exited during load")
        traffic = workload.kubectl("logs", "profile-client")
        (output / "traffic.txt").write_text(traffic)
        pod = json.loads(workload.kubectl("get", "pod/profile-client", "-o", "json"))
        if "request-ok" not in traffic or pod.get("status", {}).get("phase") != "Running":
            raise RuntimeError("in-cluster HTTP traffic did not remain healthy")
        (output / "pods.json").write_text(workload.kubectl("get", "pods", "-o", "json"))
        (output / "workload.json").write_text(json.dumps({"namespace": workload.namespace,
            "backend": args.backend, "components": args.components, "whole_stack": args.whole_stack,
            "metrics_only": args.metrics_only, "preset": args.workload,
            "replicas": args.replicas, "api_workers": args.workers, "seconds_per_phase": args.seconds,
            "http_successes": traffic.count("request-ok")}, indent=2))
    except Exception as error:
        failure = error
        (output / "failure.txt").write_text(f"{type(error).__name__}: {error}\n")
        # Inspect before namespace deletion triggers GC and destroys the
        # evidence. Diagnostic errors must not replace the workload failure.
        for filename, command in (
            ("failure-pods.json", ("get", "pods", "-o", "json")),
            ("failure-events.json", ("get", "events", "-o", "json")),
            ("failure-client.txt", ("describe", "pod/profile-client")),
            ("failure-client-log.txt", ("logs", "profile-client")),
        ):
            try:
                diagnostic = workload.kubectl(*command, timeout=35)
            except Exception as diagnostic_error:
                diagnostic = f"diagnostic unavailable: {diagnostic_error}\n"
            (output / filename).write_text(diagnostic)
        raise
    finally:
        workload.stop.set()
        if watcher is not None:
            watcher.terminate()
            try:
                watcher.wait(timeout=10)
            except subprocess.TimeoutExpired:
                watcher.kill()
                watcher.wait()
        if watch_log is not None:
            watch_log.close()
        try:
            workload.kubectl("delete", "namespace", workload.namespace, "--ignore-not-found", "--wait=false")
        except Exception as cleanup_error:
            if failure is None:
                raise
            (output / "cleanup-error.txt").write_text(str(cleanup_error) + "\n")
        finally:
            workload.save()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True)
    parser.add_argument("--seconds", type=int, default=120)
    parser.add_argument("--workload", choices=("standard", "heavy"), default="standard")
    parser.add_argument("--replicas", type=int)
    parser.add_argument("--workers", type=int)
    parser.add_argument("--backend", choices=("notk8s", "k8s", "k3s"), default="notk8s")
    parser.add_argument("--components", default=",".join(COMPONENTS))
    parser.add_argument("--whole-stack", action="store_true")
    parser.add_argument("--metrics-only", action="store_true")
    args = parser.parse_args()
    args.replicas = args.replicas if args.replicas is not None else (10 if args.workload == "heavy" else 3)
    args.workers = args.workers if args.workers is not None else (4 if args.workload == "heavy" else 2)
    args.components = tuple(args.components.split(","))
    if not args.components or len(set(args.components)) != len(args.components) or set(args.components) - set(COMPONENTS):
        parser.error("components must be unique canonical not-k8s runtime component names")
    if args.backend == "k3s" and not args.whole_stack:
        parser.error("k3s embeds components; only whole-stack measurement is valid")
    if args.whole_stack and set(args.components) != set(COMPONENTS):
        parser.error("whole-stack measurement requires all runtime components")
    if not 30 <= args.seconds <= 600 or not 1 <= args.replicas <= 10 or not 1 <= args.workers <= 4:
        parser.error("seconds: 30..600; replicas: 1..10; workers: 1..4")
    run(args)


if __name__ == "__main__":
    main()
