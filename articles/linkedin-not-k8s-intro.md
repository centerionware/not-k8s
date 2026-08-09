Kubelet, the agent running on every Kubernetes node, idles at about 81MB of RAM and burns real CPU before your cluster does anything useful. It polls constantly, runs its own container-stats housekeeping, and keeps a pile of watch caches alive the whole time it's idle.

not-k8s replaces just that node agent with nodelet, an event-driven Rust binary. Everything else stays the same.

I profiled both on x86_64 CI runners and on a Google Pixel 7 running Debian in a VM. Idle, no pods scheduled, 120s window, 3 replicates per agent.

x86_64:
- nodelet ~15MB / ~0.08s CPU
- kubelet ~81MB / ~0.85s CPU

Pixel 7:
- nodelet 12.0MB / 0.436s CPU
- kubelet 67.9MB / 8.031s CPU

The CPU gap widens from ~10.6x to ~18.4x on the slower core. Memory stays flat at ~5.5x. Idle overhead doesn't stay proportional as hardware gets weaker — it gets worse, which is the opposite of how it's usually dismissed.

On that same phone the k3s control plane idles at ~34% of a core and ~350MB, more than either node agent. Replacing the agent doesn't touch that.

Raw per-second CSVs and methodology for both platforms:

x86_64: https://github.com/centerionware/not-k8s/tree/profiling-results/latest
Pixel 7: https://github.com/centerionware/not-k8s/tree/profiling-results/history/2026-08-09_00-59-17-arm64-phone
Repo: https://github.com/centerionware/not-k8s

Still alpha: 1,100 unit tests and 140 e2e tests against real containerd and real CSI/DRA drivers, gated before any release ships.

#Kubernetes #Rust #EdgeComputing
