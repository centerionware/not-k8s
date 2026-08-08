Kubelet, the agent running on every Kubernetes node, idles at about 81MB of RAM and burns real CPU before your cluster does anything useful.

I built a leaner replacement and benchmarked it properly to see if the gap was real.

Kubelet's job is necessary: watch for pods, talk to the container runtime, report status to the control plane. But it also polls constantly, runs its own container-stats housekeeping, and keeps a pile of watch caches alive the whole time it's idle.

So I built not-k8s. Real Kubernetes control plane underneath — real apiserver, real scheduler, full kubectl and CRD compatibility. The node agent is swapped out for a leaner, event-driven Rust binary called nodelet.

I didn't trust my own claim until I tested it:

- Installed nodelet, let it sit idle, measured real memory and CPU time
- Ran the identical test with a genuine, unmodified, standalone kubelet binary
- Same control plane, same container runtime, only the node agent changed
- Six separate runs on six separate machines, so I couldn't bias the numbers by testing back to back on a warmed-up box

Completely idle, zero workload running:

- Memory — about 15MB for nodelet, about 81MB for kubelet
- CPU time over a 2-minute idle window — about 0.08s vs about 0.85s

That's the cost of having the node agent installed, before it's done anything.

It matters most on something small — a Pi, an edge box, a fleet of them. It doesn't stop mattering on a big server, it's just easier to lose in the noise there. Every idle cycle kubelet burns is a cycle your workload doesn't get, on every node you run.

Open source, still early, raw data and charts included with every run:

Report: https://github.com/centerionware/not-k8s/blob/profiling-results/latest/README.md
Repo: https://github.com/centerionware/not-k8s

Genuinely curious — if you're running Kubernetes on a Pi cluster, edge fleet, or anything resource-constrained, would 66MB and 0.8 CPU-seconds per node actually matter to you, or is that noise at your scale?

#Kubernetes #Rust #EdgeComputing
