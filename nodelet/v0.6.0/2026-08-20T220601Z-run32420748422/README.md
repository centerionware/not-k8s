# nodelet flame graph — v0.6.0

Workload lifecycle: create 10 namespace(s),
one plain `nginx:1.27-alpine` pod each (no probes), wait for all
`Ready`, idle 15s with all of them
running, delete every namespace, wait for all of them gone.
`deploy/profile-process.sh` attached to the real nodelet PID
(`perf record -F 99 --call-graph dwarf`, real debug binary —
nodelet-0.6.0-linux-x86_64-debug) for exactly that window —
stopped the moment every namespace was confirmed gone
(`--stop-file`), not a pre-guessed fixed duration, so nothing here
is diluted by idle-nodelet-with-nothing-happening frames.
180s was the safety-cap ceiling, not the
actual sample length.

- `flamegraph.svg` — open directly in a browser.
- `perf-report.txt` / `perf-self-report.txt` — textual top-functions
  breakdown (inclusive / self time).
- `top-functions.txt` — the same, pre-sorted top 20.
- `nodelet-workload-lifecycle-journal.txt` — nodelet's own log lines for the sample window.
- `process.txt` — PID/executable/cmdline identity.

Captured from: https://github.com/centerionware/not-k8s/actions/runs/32420748422
Repo commit: e680c95

Related: #133
