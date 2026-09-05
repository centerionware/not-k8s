# Stack CPU profile

- Source: `6c2d5ce59577d36aea5d6c22ea8940c69384d16c`
- Run: https://github.com/centerionware/not-k8s/actions/runs/33940170035
- Build: profiling; capture result: capture=success,render=success
- Workload: heavy (see workload-config.json for parameters)
- Complete compressed bundle: 111361775 bytes; parts are below GitHub's per-file limit.

This is one single-node diagnostic sample, not conformance, a release performance
ratio, or a statistical benchmark. Six runtime PIDs are sampled together. The
bootstrap applet is captured separately. The load generator and perf share the
host. Inspect workload errors and restart checks before interpreting CPU numbers.

The archive includes raw perf data, decoded stacks, per-process CPU/RSS/PSS series,
workload operations, symbolized executable, build identity, and diagnostics.
An empty folded-stack file is reported as no samples, not zero CPU usage.
Exact min/mean/max values are in [charts/summary.csv](charts/summary.csv).
Memory units are MiB; CPU is percent of one logical CPU. Chart whiskers show
the observed range, not a confidence interval.

Download all parts and SHA256SUMS into an empty directory, then:

```sh
sha256sum -c SHA256SUMS
cat profile.tar.gz.part-* | tar -xz
```

Use the included matching executable with `perf report --symfs` if re-analyzing
on another host. The bundle retains the original absolute executable layout under
`symfs/`; rendered SVGs and text reports need no symbol setup.

## Browseable files

- [bootstrap/flamegraph.svg](bootstrap/flamegraph.svg)
- [idle/nodeapiserver/SUMMARY.txt](idle/nodeapiserver/SUMMARY.txt)
- [idle/nodeapiserver/flamegraph.svg](idle/nodeapiserver/flamegraph.svg)
- [idle/nodecontroller/SUMMARY.txt](idle/nodecontroller/SUMMARY.txt)
- [idle/nodecontroller/flamegraph.svg](idle/nodecontroller/flamegraph.svg)
- [idle/nodelet/SUMMARY.txt](idle/nodelet/SUMMARY.txt)
- [idle/nodelet/flamegraph.svg](idle/nodelet/flamegraph.svg)
- [idle/nodeproxy/SUMMARY.txt](idle/nodeproxy/SUMMARY.txt)
- [idle/nodeproxy/no-samples.txt](idle/nodeproxy/no-samples.txt)
- [idle/nodescheduler/SUMMARY.txt](idle/nodescheduler/SUMMARY.txt)
- [idle/nodescheduler/no-samples.txt](idle/nodescheduler/no-samples.txt)
- [idle/nodestore/SUMMARY.txt](idle/nodestore/SUMMARY.txt)
- [idle/nodestore/flamegraph.svg](idle/nodestore/flamegraph.svg)
- [idle/timeseries.csv](idle/timeseries.csv)
- [load/nodeapiserver/SUMMARY.txt](load/nodeapiserver/SUMMARY.txt)
- [load/nodeapiserver/flamegraph.svg](load/nodeapiserver/flamegraph.svg)
- [load/nodecontroller/SUMMARY.txt](load/nodecontroller/SUMMARY.txt)
- [load/nodecontroller/flamegraph.svg](load/nodecontroller/flamegraph.svg)
- [load/nodelet/SUMMARY.txt](load/nodelet/SUMMARY.txt)
- [load/nodelet/flamegraph.svg](load/nodelet/flamegraph.svg)
- [load/nodeproxy/SUMMARY.txt](load/nodeproxy/SUMMARY.txt)
- [load/nodeproxy/no-samples.txt](load/nodeproxy/no-samples.txt)
- [load/nodescheduler/SUMMARY.txt](load/nodescheduler/SUMMARY.txt)
- [load/nodescheduler/flamegraph.svg](load/nodescheduler/flamegraph.svg)
- [load/nodestore/SUMMARY.txt](load/nodestore/SUMMARY.txt)
- [load/nodestore/flamegraph.svg](load/nodestore/flamegraph.svg)
- [load/timeseries.csv](load/timeseries.csv)

## Charts and flame graphs

### bootstrap/flamegraph.svg

![bootstrap/flamegraph.svg](bootstrap/flamegraph.svg)

### charts/idle-combined-cpu_pct_one_core.png

![charts/idle-combined-cpu_pct_one_core.png](charts/idle-combined-cpu_pct_one_core.png)

### charts/idle-combined-pss_kib.png

![charts/idle-combined-pss_kib.png](charts/idle-combined-pss_kib.png)

### charts/idle-combined-rss_kib.png

![charts/idle-combined-rss_kib.png](charts/idle-combined-rss_kib.png)

### charts/idle-nodeapiserver-cpu_pct_one_core.png

![charts/idle-nodeapiserver-cpu_pct_one_core.png](charts/idle-nodeapiserver-cpu_pct_one_core.png)

### charts/idle-nodeapiserver-pss_kib.png

![charts/idle-nodeapiserver-pss_kib.png](charts/idle-nodeapiserver-pss_kib.png)

### charts/idle-nodeapiserver-rss_kib.png

![charts/idle-nodeapiserver-rss_kib.png](charts/idle-nodeapiserver-rss_kib.png)

### charts/idle-nodecontroller-cpu_pct_one_core.png

![charts/idle-nodecontroller-cpu_pct_one_core.png](charts/idle-nodecontroller-cpu_pct_one_core.png)

### charts/idle-nodecontroller-pss_kib.png

![charts/idle-nodecontroller-pss_kib.png](charts/idle-nodecontroller-pss_kib.png)

### charts/idle-nodecontroller-rss_kib.png

![charts/idle-nodecontroller-rss_kib.png](charts/idle-nodecontroller-rss_kib.png)

### charts/idle-nodelet-cpu_pct_one_core.png

![charts/idle-nodelet-cpu_pct_one_core.png](charts/idle-nodelet-cpu_pct_one_core.png)

### charts/idle-nodelet-pss_kib.png

![charts/idle-nodelet-pss_kib.png](charts/idle-nodelet-pss_kib.png)

### charts/idle-nodelet-rss_kib.png

![charts/idle-nodelet-rss_kib.png](charts/idle-nodelet-rss_kib.png)

### charts/idle-nodeproxy-cpu_pct_one_core.png

![charts/idle-nodeproxy-cpu_pct_one_core.png](charts/idle-nodeproxy-cpu_pct_one_core.png)

### charts/idle-nodeproxy-pss_kib.png

![charts/idle-nodeproxy-pss_kib.png](charts/idle-nodeproxy-pss_kib.png)

### charts/idle-nodeproxy-rss_kib.png

![charts/idle-nodeproxy-rss_kib.png](charts/idle-nodeproxy-rss_kib.png)

### charts/idle-nodescheduler-cpu_pct_one_core.png

![charts/idle-nodescheduler-cpu_pct_one_core.png](charts/idle-nodescheduler-cpu_pct_one_core.png)

### charts/idle-nodescheduler-pss_kib.png

![charts/idle-nodescheduler-pss_kib.png](charts/idle-nodescheduler-pss_kib.png)

### charts/idle-nodescheduler-rss_kib.png

![charts/idle-nodescheduler-rss_kib.png](charts/idle-nodescheduler-rss_kib.png)

### charts/idle-nodestore-cpu_pct_one_core.png

![charts/idle-nodestore-cpu_pct_one_core.png](charts/idle-nodestore-cpu_pct_one_core.png)

### charts/idle-nodestore-pss_kib.png

![charts/idle-nodestore-pss_kib.png](charts/idle-nodestore-pss_kib.png)

### charts/idle-nodestore-rss_kib.png

![charts/idle-nodestore-rss_kib.png](charts/idle-nodestore-rss_kib.png)

### charts/idle-summary-cpu_pct_one_core.png

![charts/idle-summary-cpu_pct_one_core.png](charts/idle-summary-cpu_pct_one_core.png)

### charts/idle-summary-pss_kib.png

![charts/idle-summary-pss_kib.png](charts/idle-summary-pss_kib.png)

### charts/idle-summary-rss_kib.png

![charts/idle-summary-rss_kib.png](charts/idle-summary-rss_kib.png)

### charts/load-combined-cpu_pct_one_core.png

![charts/load-combined-cpu_pct_one_core.png](charts/load-combined-cpu_pct_one_core.png)

### charts/load-combined-pss_kib.png

![charts/load-combined-pss_kib.png](charts/load-combined-pss_kib.png)

### charts/load-combined-rss_kib.png

![charts/load-combined-rss_kib.png](charts/load-combined-rss_kib.png)

### charts/load-nodeapiserver-cpu_pct_one_core.png

![charts/load-nodeapiserver-cpu_pct_one_core.png](charts/load-nodeapiserver-cpu_pct_one_core.png)

### charts/load-nodeapiserver-pss_kib.png

![charts/load-nodeapiserver-pss_kib.png](charts/load-nodeapiserver-pss_kib.png)

### charts/load-nodeapiserver-rss_kib.png

![charts/load-nodeapiserver-rss_kib.png](charts/load-nodeapiserver-rss_kib.png)

### charts/load-nodecontroller-cpu_pct_one_core.png

![charts/load-nodecontroller-cpu_pct_one_core.png](charts/load-nodecontroller-cpu_pct_one_core.png)

### charts/load-nodecontroller-pss_kib.png

![charts/load-nodecontroller-pss_kib.png](charts/load-nodecontroller-pss_kib.png)

### charts/load-nodecontroller-rss_kib.png

![charts/load-nodecontroller-rss_kib.png](charts/load-nodecontroller-rss_kib.png)

### charts/load-nodelet-cpu_pct_one_core.png

![charts/load-nodelet-cpu_pct_one_core.png](charts/load-nodelet-cpu_pct_one_core.png)

### charts/load-nodelet-pss_kib.png

![charts/load-nodelet-pss_kib.png](charts/load-nodelet-pss_kib.png)

### charts/load-nodelet-rss_kib.png

![charts/load-nodelet-rss_kib.png](charts/load-nodelet-rss_kib.png)

### charts/load-nodeproxy-cpu_pct_one_core.png

![charts/load-nodeproxy-cpu_pct_one_core.png](charts/load-nodeproxy-cpu_pct_one_core.png)

### charts/load-nodeproxy-pss_kib.png

![charts/load-nodeproxy-pss_kib.png](charts/load-nodeproxy-pss_kib.png)

### charts/load-nodeproxy-rss_kib.png

![charts/load-nodeproxy-rss_kib.png](charts/load-nodeproxy-rss_kib.png)

### charts/load-nodescheduler-cpu_pct_one_core.png

![charts/load-nodescheduler-cpu_pct_one_core.png](charts/load-nodescheduler-cpu_pct_one_core.png)

### charts/load-nodescheduler-pss_kib.png

![charts/load-nodescheduler-pss_kib.png](charts/load-nodescheduler-pss_kib.png)

### charts/load-nodescheduler-rss_kib.png

![charts/load-nodescheduler-rss_kib.png](charts/load-nodescheduler-rss_kib.png)

### charts/load-nodestore-cpu_pct_one_core.png

![charts/load-nodestore-cpu_pct_one_core.png](charts/load-nodestore-cpu_pct_one_core.png)

### charts/load-nodestore-pss_kib.png

![charts/load-nodestore-pss_kib.png](charts/load-nodestore-pss_kib.png)

### charts/load-nodestore-rss_kib.png

![charts/load-nodestore-rss_kib.png](charts/load-nodestore-rss_kib.png)

### charts/load-summary-cpu_pct_one_core.png

![charts/load-summary-cpu_pct_one_core.png](charts/load-summary-cpu_pct_one_core.png)

### charts/load-summary-pss_kib.png

![charts/load-summary-pss_kib.png](charts/load-summary-pss_kib.png)

### charts/load-summary-rss_kib.png

![charts/load-summary-rss_kib.png](charts/load-summary-rss_kib.png)

### idle/nodeapiserver/flamegraph.svg

![idle/nodeapiserver/flamegraph.svg](idle/nodeapiserver/flamegraph.svg)

### idle/nodecontroller/flamegraph.svg

![idle/nodecontroller/flamegraph.svg](idle/nodecontroller/flamegraph.svg)

### idle/nodelet/flamegraph.svg

![idle/nodelet/flamegraph.svg](idle/nodelet/flamegraph.svg)

### idle/nodestore/flamegraph.svg

![idle/nodestore/flamegraph.svg](idle/nodestore/flamegraph.svg)

### load/nodeapiserver/flamegraph.svg

![load/nodeapiserver/flamegraph.svg](load/nodeapiserver/flamegraph.svg)

### load/nodecontroller/flamegraph.svg

![load/nodecontroller/flamegraph.svg](load/nodecontroller/flamegraph.svg)

### load/nodelet/flamegraph.svg

![load/nodelet/flamegraph.svg](load/nodelet/flamegraph.svg)

### load/nodescheduler/flamegraph.svg

![load/nodescheduler/flamegraph.svg](load/nodescheduler/flamegraph.svg)

### load/nodestore/flamegraph.svg

![load/nodestore/flamegraph.svg](load/nodestore/flamegraph.svg)

