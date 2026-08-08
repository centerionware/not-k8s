1/
kubelet is the reason your Raspberry Pi k8s cluster feels sluggish before you've even deployed anything.

so I ripped it out and replaced it with a 15MB Rust binary. real control plane, real kubectl, just a leaner node agent.

2/
kept the real k3s control plane (apiserver, scheduler, full CRD support). swapped kubelet for nodelet — event-driven instead of polling-driven.

same containerd, same everything else. just the node agent changed.

3/
ran a real benchmark, not vibes: nodelet vs a genuine unmodified upstream kubelet, both completely idle, 6 runs on separate machines so I couldn't accidentally bias it.

results, idle, zero workload:
RAM: ~15MB vs ~81MB
CPU time (2min idle): ~0.08s vs ~0.85s

4/
biggest win on a Pi, a mini PC, or a fleet of small edge devices. but a beefy server isn't exempt either — every idle cycle kubelet burns there is one your workload doesn't get, across every node in the fleet.

5/
open source, still early, raw data and charts both included.

report + real charts: https://github.com/centerionware/not-k8s/blob/profiling-results/latest/README.md
repo: https://github.com/centerionware/not-k8s
