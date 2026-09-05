# Kubernetes stack comparison

Measured scope: distribution daemons.

CPU and memory counters only: no perf or flamegraph collection during this comparison.
Each stack runs on an independent hosted runner with the same bounded workload: a ready
HTTP Deployment, service traffic, ConfigMap API/watch churn and replica scaling.
This is one sample per stack, not a statistical claim. Compare hardware metadata and
workload errors/operation counts before interpreting differences. Components are measured
inside their respective full stacks, not swapped into an otherwise identical control plane.

Canonical chart labels map nodestore→etcd, nodeapiserver→kube-apiserver,
nodescheduler→kube-scheduler, nodecontroller→kube-controller-manager, nodelet→kubelet,
and nodeproxy→kube-proxy. k3s embeds these components: they are not separately attributable.

Whole-stack totals include these distribution daemons plus containerd, Flannel and CoreDNS
(k3s embeds Flannel). They exclude workload containers, runtime shims, the load generator
and unrelated host services. RSS sums can double-count shared pages; PSS apportions them.
Missing values are unavailable, never zero. Component-mode totals include only selected components.

## Source data

- [notk8s metadata](notk8s/metadata.txt), [workload](notk8s/workload.json), [idle CSV](notk8s/idle/timeseries.csv), [load CSV](notk8s/load/timeseries.csv)
- [k8s metadata](k8s/metadata.txt), [workload](k8s/workload.json), [idle CSV](k8s/idle/timeseries.csv), [load CSV](k8s/load/timeseries.csv)

## Component and combined graphs

### idle-combined-cpu_pct_one_core

![idle-combined-cpu_pct_one_core](charts/idle-combined-cpu_pct_one_core.png)

### idle-combined-pss_kib

![idle-combined-pss_kib](charts/idle-combined-pss_kib.png)

### idle-combined-rss_kib

![idle-combined-rss_kib](charts/idle-combined-rss_kib.png)

### idle-containerd-cpu_pct_one_core

![idle-containerd-cpu_pct_one_core](charts/idle-containerd-cpu_pct_one_core.png)

### idle-containerd-pss_kib

![idle-containerd-pss_kib](charts/idle-containerd-pss_kib.png)

### idle-containerd-rss_kib

![idle-containerd-rss_kib](charts/idle-containerd-rss_kib.png)

### idle-coredns-cpu_pct_one_core

![idle-coredns-cpu_pct_one_core](charts/idle-coredns-cpu_pct_one_core.png)

### idle-coredns-pss_kib

![idle-coredns-pss_kib](charts/idle-coredns-pss_kib.png)

### idle-coredns-rss_kib

![idle-coredns-rss_kib](charts/idle-coredns-rss_kib.png)

### idle-flanneld-cpu_pct_one_core

![idle-flanneld-cpu_pct_one_core](charts/idle-flanneld-cpu_pct_one_core.png)

### idle-flanneld-pss_kib

![idle-flanneld-pss_kib](charts/idle-flanneld-pss_kib.png)

### idle-flanneld-rss_kib

![idle-flanneld-rss_kib](charts/idle-flanneld-rss_kib.png)

### idle-nodeapiserver-cpu_pct_one_core

![idle-nodeapiserver-cpu_pct_one_core](charts/idle-nodeapiserver-cpu_pct_one_core.png)

### idle-nodeapiserver-pss_kib

![idle-nodeapiserver-pss_kib](charts/idle-nodeapiserver-pss_kib.png)

### idle-nodeapiserver-rss_kib

![idle-nodeapiserver-rss_kib](charts/idle-nodeapiserver-rss_kib.png)

### idle-nodecontroller-cpu_pct_one_core

![idle-nodecontroller-cpu_pct_one_core](charts/idle-nodecontroller-cpu_pct_one_core.png)

### idle-nodecontroller-pss_kib

![idle-nodecontroller-pss_kib](charts/idle-nodecontroller-pss_kib.png)

### idle-nodecontroller-rss_kib

![idle-nodecontroller-rss_kib](charts/idle-nodecontroller-rss_kib.png)

### idle-nodelet-cpu_pct_one_core

![idle-nodelet-cpu_pct_one_core](charts/idle-nodelet-cpu_pct_one_core.png)

### idle-nodelet-pss_kib

![idle-nodelet-pss_kib](charts/idle-nodelet-pss_kib.png)

### idle-nodelet-rss_kib

![idle-nodelet-rss_kib](charts/idle-nodelet-rss_kib.png)

### idle-nodeproxy-cpu_pct_one_core

![idle-nodeproxy-cpu_pct_one_core](charts/idle-nodeproxy-cpu_pct_one_core.png)

### idle-nodeproxy-pss_kib

![idle-nodeproxy-pss_kib](charts/idle-nodeproxy-pss_kib.png)

### idle-nodeproxy-rss_kib

![idle-nodeproxy-rss_kib](charts/idle-nodeproxy-rss_kib.png)

### idle-nodescheduler-cpu_pct_one_core

![idle-nodescheduler-cpu_pct_one_core](charts/idle-nodescheduler-cpu_pct_one_core.png)

### idle-nodescheduler-pss_kib

![idle-nodescheduler-pss_kib](charts/idle-nodescheduler-pss_kib.png)

### idle-nodescheduler-rss_kib

![idle-nodescheduler-rss_kib](charts/idle-nodescheduler-rss_kib.png)

### idle-nodestore-cpu_pct_one_core

![idle-nodestore-cpu_pct_one_core](charts/idle-nodestore-cpu_pct_one_core.png)

### idle-nodestore-pss_kib

![idle-nodestore-pss_kib](charts/idle-nodestore-pss_kib.png)

### idle-nodestore-rss_kib

![idle-nodestore-rss_kib](charts/idle-nodestore-rss_kib.png)

### idle-summary-cpu_pct_one_core

![idle-summary-cpu_pct_one_core](charts/idle-summary-cpu_pct_one_core.png)

### idle-summary-pss_kib

![idle-summary-pss_kib](charts/idle-summary-pss_kib.png)

### idle-summary-rss_kib

![idle-summary-rss_kib](charts/idle-summary-rss_kib.png)

### load-combined-cpu_pct_one_core

![load-combined-cpu_pct_one_core](charts/load-combined-cpu_pct_one_core.png)

### load-combined-pss_kib

![load-combined-pss_kib](charts/load-combined-pss_kib.png)

### load-combined-rss_kib

![load-combined-rss_kib](charts/load-combined-rss_kib.png)

### load-containerd-cpu_pct_one_core

![load-containerd-cpu_pct_one_core](charts/load-containerd-cpu_pct_one_core.png)

### load-containerd-pss_kib

![load-containerd-pss_kib](charts/load-containerd-pss_kib.png)

### load-containerd-rss_kib

![load-containerd-rss_kib](charts/load-containerd-rss_kib.png)

### load-coredns-cpu_pct_one_core

![load-coredns-cpu_pct_one_core](charts/load-coredns-cpu_pct_one_core.png)

### load-coredns-pss_kib

![load-coredns-pss_kib](charts/load-coredns-pss_kib.png)

### load-coredns-rss_kib

![load-coredns-rss_kib](charts/load-coredns-rss_kib.png)

### load-flanneld-cpu_pct_one_core

![load-flanneld-cpu_pct_one_core](charts/load-flanneld-cpu_pct_one_core.png)

### load-flanneld-pss_kib

![load-flanneld-pss_kib](charts/load-flanneld-pss_kib.png)

### load-flanneld-rss_kib

![load-flanneld-rss_kib](charts/load-flanneld-rss_kib.png)

### load-nodeapiserver-cpu_pct_one_core

![load-nodeapiserver-cpu_pct_one_core](charts/load-nodeapiserver-cpu_pct_one_core.png)

### load-nodeapiserver-pss_kib

![load-nodeapiserver-pss_kib](charts/load-nodeapiserver-pss_kib.png)

### load-nodeapiserver-rss_kib

![load-nodeapiserver-rss_kib](charts/load-nodeapiserver-rss_kib.png)

### load-nodecontroller-cpu_pct_one_core

![load-nodecontroller-cpu_pct_one_core](charts/load-nodecontroller-cpu_pct_one_core.png)

### load-nodecontroller-pss_kib

![load-nodecontroller-pss_kib](charts/load-nodecontroller-pss_kib.png)

### load-nodecontroller-rss_kib

![load-nodecontroller-rss_kib](charts/load-nodecontroller-rss_kib.png)

### load-nodelet-cpu_pct_one_core

![load-nodelet-cpu_pct_one_core](charts/load-nodelet-cpu_pct_one_core.png)

### load-nodelet-pss_kib

![load-nodelet-pss_kib](charts/load-nodelet-pss_kib.png)

### load-nodelet-rss_kib

![load-nodelet-rss_kib](charts/load-nodelet-rss_kib.png)

### load-nodeproxy-cpu_pct_one_core

![load-nodeproxy-cpu_pct_one_core](charts/load-nodeproxy-cpu_pct_one_core.png)

### load-nodeproxy-pss_kib

![load-nodeproxy-pss_kib](charts/load-nodeproxy-pss_kib.png)

### load-nodeproxy-rss_kib

![load-nodeproxy-rss_kib](charts/load-nodeproxy-rss_kib.png)

### load-nodescheduler-cpu_pct_one_core

![load-nodescheduler-cpu_pct_one_core](charts/load-nodescheduler-cpu_pct_one_core.png)

### load-nodescheduler-pss_kib

![load-nodescheduler-pss_kib](charts/load-nodescheduler-pss_kib.png)

### load-nodescheduler-rss_kib

![load-nodescheduler-rss_kib](charts/load-nodescheduler-rss_kib.png)

### load-nodestore-cpu_pct_one_core

![load-nodestore-cpu_pct_one_core](charts/load-nodestore-cpu_pct_one_core.png)

### load-nodestore-pss_kib

![load-nodestore-pss_kib](charts/load-nodestore-pss_kib.png)

### load-nodestore-rss_kib

![load-nodestore-rss_kib](charts/load-nodestore-rss_kib.png)

### load-summary-cpu_pct_one_core

![load-summary-cpu_pct_one_core](charts/load-summary-cpu_pct_one_core.png)

### load-summary-pss_kib

![load-summary-pss_kib](charts/load-summary-pss_kib.png)

### load-summary-rss_kib

![load-summary-rss_kib](charts/load-summary-rss_kib.png)

