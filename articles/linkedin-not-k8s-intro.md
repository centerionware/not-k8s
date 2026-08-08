Kubelet, the agent running on every Kubernetes node, idles at about 81MB of RAM and burns real CPU before your cluster does anything useful.

Kubelet's job is necessary: watch for pods, talk to the container runtime, report status back. But it also polls constantly, runs its own container-stats housekeeping, and keeps a pile of watch caches alive the whole time it's idle.

not-k8s replaces just that node agent with nodelet — a leaner, event-driven Rust binary. Everything else about the cluster stays the same; only the per-node agent changes.

Benchmarked, not just claimed:

- Installed nodelet, let it sit idle, measured real memory and CPU time
- Ran the identical test with a standalone upstream kubelet binary
- Same container runtime, only the node agent changed
- Six separate runs on six separate machines, so the numbers couldn't be biased by testing back to back on a warmed-up box

Completely idle, zero workload running:

- Memory — about 15MB for nodelet, about 81MB for kubelet (~5x lower)
- CPU time over a 2-minute idle window — about 0.08s vs about 0.85s (~10x lower)

That's the cost of having the node agent installed, before it's done anything. Per node, it's already small — but idle cost scales with fleet size, not workload:

- 100 nodes — ~1.5GB vs ~8.1GB of RAM burned just sitting idle
- 1,000 nodes — ~15GB vs ~81GB of RAM burned just sitting idle

Same cluster, same workload, nothing running yet. Every idle cycle kubelet burns is a cycle your workload doesn't get, multiplied by every node you run.

Open source, still early, raw data and charts included with every run:

Report: https://github.com/centerionware/not-k8s/blob/profiling-results/latest/README.md
Repo: https://github.com/centerionware/not-k8s

If you're running Kubernetes on a Pi cluster, edge fleet, or anything at scale — does that gap actually matter to you, or is it noise?

#Kubernetes #Rust #EdgeComputing
