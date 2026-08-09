Kubelet, the agent running on every Kubernetes node, idles at about 81MB of RAM and burns real CPU before your cluster does anything useful.

Its job is necessary: watch for pods, talk to the container runtime, report status back. But it also polls constantly, runs its own container-stats housekeeping, and keeps a pile of watch caches alive the whole time it's idle.

not-k8s replaces just that node agent with nodelet — a leaner, event-driven Rust binary. Everything else about the cluster stays the same.

I profiled both on two very different machines: x86_64 CI runners, and a Google Pixel 7 running Debian in a VM. Idle, no pods scheduled, 120s window, 3 replicates per agent.

x86_64:
- nodelet ~15MB / ~0.08s CPU
- kubelet ~81MB / ~0.85s CPU

ARM phone:
- nodelet 12.0MB / 0.436s CPU
- kubelet 67.9MB / 8.031s CPU

The result I didn't expect: the CPU gap widens from ~10.6x on the server core to ~18.4x on the phone core, while the memory ratio stays essentially flat. Normalized against their own x86 baselines, nodelet runs ~5.5x slower on the phone and kubelet ~9.5x slower — something superlinear penalizes kubelet specifically as hardware gets weaker.

That matters because it inverts the usual intuition. Idle overhead is easy to dismiss at datacenter scale, where it's a fraction of a percent of a fast core. On constrained hardware it doesn't stay proportional — it gets worse. The node agent's resource floor is what decides which hardware can be a Kubernetes node at all, and that floor is the part nobody optimizes because most people measure it on servers.

The caveat I'd rather state myself than have someone else state for me: on that same phone, the k3s control plane idles at ~34% of a core and ~350MB — more than either node agent. Replacing the agent is a real saving that doesn't touch the biggest line item. That's the next problem, not one this solves.

Raw per-second CSVs and full methodology for both platforms are published, and the ARM report leads with its own limitations (sequential rather than parallel legs, thermal throttling, virtualization) rather than burying them:

x86_64: https://github.com/centerionware/not-k8s/tree/profiling-results/latest
ARM phone: https://github.com/centerionware/not-k8s/tree/profiling-results/history/2026-08-09_00-59-17-arm64-phone
Repo: https://github.com/centerionware/not-k8s

Still alpha, and honest about it: 1,100 unit tests and 140 e2e tests against real containerd and real CSI/DRA drivers, gated before any release ships.

#Kubernetes #Rust #EdgeComputing
