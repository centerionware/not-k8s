Kubelet, the agent running on every Kubernetes node, idles at about 81MB of RAM and burns real CPU before your cluster does anything useful.

Kubelet's job is necessary: watch for pods, talk to the container runtime, report status to the control plane. But it also polls constantly, runs its own container-stats housekeeping, and keeps a pile of watch caches alive the whole time it's idle.

not-k8s replaces just that node agent. A k3s control plane underneath — apiserver, scheduler, full kubectl and CRD compatibility — stays as-is; the node agent is swapped for a leaner, event-driven Rust binary called nodelet.

Benchmarked, not just claimed:

- Installed nodelet, let it sit idle, measured real memory and CPU time
- Ran the identical test with a standalone upstream kubelet binary
- Same control plane, same container runtime, only the node agent changed
- Six separate runs on six separate machines, so the numbers couldn't be biased by testing back to back on a warmed-up box

Completely idle, zero workload running:

- Memory — about 15MB for nodelet, about 81MB for kubelet
- CPU time over a 2-minute idle window — about 0.08s vs about 0.85s

That's the cost of having the node agent installed, before it's done anything.

It matters most on something small — a Pi, an edge box, a fleet of them. It doesn't stop mattering on a big server, it's just easier to lose in the noise there. Every idle cycle kubelet burns is a cycle your workload doesn't get, on every node you run.

Open source, still early, raw data and charts included with every run:

Report: https://github.com/centerionware/not-k8s/blob/profiling-results/latest/README.md
Repo: https://github.com/centerionware/not-k8s

If you're running Kubernetes on a Pi cluster, edge fleet, or anything resource-constrained — does 66MB and 0.8 CPU-seconds per node actually matter at your scale, or is that noise?

#Kubernetes #Rust #EdgeComputing
