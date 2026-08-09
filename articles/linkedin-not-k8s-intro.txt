Kubelet idles at ~81MB and burns real CPU before your cluster does anything. It polls constantly and keeps a pile of watch caches alive the whole time.

not-k8s swaps just that node agent for nodelet, an event-driven Rust binary. Everything else stays the same.

Profiled both on x86_64 CI runners and a Pixel 7 running Debian in a VM. Idle, no pods, 120s, 3 replicates each:

x86_64 — nodelet 15MB / 0.08s CPU, kubelet 81MB / 0.85s CPU
Pixel 7 — nodelet 12MB / 0.44s CPU, kubelet 68MB / 8.03s CPU

CPU gap widens from ~10.6x to ~18.4x on the slower core. Memory doesn't move.

CSVs and methodology:
x86_64: https://github.com/centerionware/not-k8s/tree/profiling-results/latest
Pixel 7: https://github.com/centerionware/not-k8s/tree/profiling-results/history/2026-08-09_00-59-17-arm64-phone
Repo: https://github.com/centerionware/not-k8s

Alpha. 1,100 unit tests, 140 e2e against real containerd and CSI/DRA drivers.

#Kubernetes #Rust #EdgeComputing
