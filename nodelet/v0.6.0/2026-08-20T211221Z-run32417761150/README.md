# nodelet flame graph — v0.6.0

Single-pod lifecycle: create a plain `nginx:1.27-alpine` pod (no
probes), wait for `Ready`, idle 15s, delete
it, wait for it to be gone. `deploy/profile-process.sh` attached to
the real nodelet PID (`perf record -F 99 --call-graph dwarf`, real
debug binary — nodelet-0.6.0-linux-x86_64-debug) for exactly that
window — stopped the moment the pod was confirmed gone
(`--stop-file`), not a pre-guessed fixed duration, so nothing here
is diluted by idle-nodelet-with-nothing-happening frames.
180s was the safety-cap ceiling, not the
actual sample length.

- `flamegraph.svg` — open directly in a browser.
- `perf-report.txt` / `perf-self-report.txt` — textual top-functions
  breakdown (inclusive / self time).
- `top-functions.txt` — the same, pre-sorted top 20.
- `nodelet-single-pod-lifecycle-journal.txt` — nodelet's own log lines for the sample window.
- `process.txt` — PID/executable/cmdline identity.

Captured from: https://github.com/centerionware/not-k8s/actions/runs/32417761150
Repo commit: 31c7329

Related: #133
