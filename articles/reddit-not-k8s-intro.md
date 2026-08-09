# I replaced kubelet with a 15MB Rust agent and profiled it on a phone

**nodelet** is a drop-in replacement for kubelet — the Kubernetes node agent — written in Rust, event-driven instead of polling. Same control plane, same `kubectl`, same CRDs; only the per-node agent changes.

Measured idle, no pods scheduled, 120s window, 3 replicates per agent, on two different platforms:

| | nodelet | upstream kubelet | gap |
|---|---|---|---|
| **x86_64** (CI runners) | ~15MB / ~0.08s CPU | ~81MB / ~0.85s CPU | 5.4x / 10.6x |
| **ARM phone** (Pixel 7, Debian VM) | 12.0MB / 0.436s CPU | 67.9MB / 8.031s CPU | 5.7x / **18.4x** |

The interesting part isn't either row, it's the difference between them. **The CPU gap widens from ~10.6x on a server core to ~18.4x on a phone core, while the memory ratio stays basically flat.** Normalized against their own x86 baselines, nodelet is ~5.5x slower on the phone core and kubelet is ~9.5x slower — so something superlinear penalizes kubelet specifically as the hardware gets weaker. That's the whole thesis in one measurement: the polling work is fixed, so a slower core spends proportionally more of itself on it.

Three things could explain that — cache pressure (kubelet's working set is ~6x bigger), the Cortex-A55 being in-order so it can't hide memory stalls, or Go's GC walking the heap against low phone memory bandwidth. I can't tell which: the PMU isn't exposed to the KVM guest, so `perf` reports every hardware counter as `<not supported>` even as root. If anyone has bare-metal ARM with working perf counters, that would actually settle it.

**The caveat I'd rather say myself than have someone else say for me:** on that same phone, the k3s control plane idles at ~34% of a core and ~350MB — considerably more than either node agent. Swapping the agent is a real saving that does not touch the biggest line item. That's the next thing worth attacking, not something this already solves.

Raw per-second CSVs and methodology for both platforms (the ARM report leads with its own limitations — sequential legs instead of parallel, thermal throttling, virtualization):

- x86_64: https://github.com/centerionware/not-k8s/tree/profiling-results/latest
- ARM phone: https://github.com/centerionware/not-k8s/tree/profiling-results/history/2026-08-09_00-59-17-arm64-phone

Still alpha — hasn't been proven under heavy real-world workloads yet — but it's passed 1,100 unit tests and 140 e2e tests against real containerd and real CSI/DRA reference drivers, gated before any release ships, so it's in a good spot to start toying with.

Repo: https://github.com/centerionware/not-k8s

It's designed to support CNI, CSI, DRA (ex GPU's), and CRI. A kubernetes agent that uses much lower resources and doesn't use a garbage collector for its memory management, so it can run on low powered devices like Pi's etc more easily, is a good idea imho. It's been an extremely fun and insightful project.

I wouldn't recommend replacing kubelet with it for anything "critical" (like a media stack used by the family, or vaultwarden, or the website that needs 99.999 uptime) — use it for things that don't matter too much. Feedback is welcome, bug reports even more so (via GitHub issues), and I'd invite collaboration if anybody is down. Eventually I'd want to replace every part of kubernetes while keeping it fully compatible; the control plane numbers above are exactly why.

Claude Code was used to research and generate virtually everything in this project, from the code to the unit tests and e2e tests, at my direction — I'm a developer of 30+ years. It was all done on a $20/mo subscription and would have cost around $2,000 on API tier, almost exclusively Sonnet on medium reasoning. The measurements above are checkable regardless of what you think of that; the CSVs are right there.
