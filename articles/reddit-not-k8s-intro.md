# not-k8s: replaced kubelet with a 15MB Rust agent, profiled on x86 and on a phone

**nodelet** is a drop-in replacement for kubelet, written in Rust, event-driven instead of polling. Same control plane, same kubectl, same CRDs.

Idle, no pods scheduled, 120s window, 3 replicates per agent:

| | nodelet | kubelet | gap |
|---|---|---|---|
| **x86_64** | ~15MB / ~0.08s CPU | ~81MB / ~0.85s CPU | 5.4x / 10.6x |
| **Pixel 7** | 12.0MB / 0.436s CPU | 67.9MB / 8.031s CPU | 5.7x / 18.4x |

The CPU gap widens on the slower core; memory stays flat. Could be cache, could be the in-order A55, could be Go's GC — can't tell, the PMU isn't exposed to the VM so perf gives no hardware counters.

On that phone the k3s control plane idles at ~34% of a core and ~350MB, more than either agent.

Raw CSVs + methodology:

- x86_64: https://github.com/centerionware/not-k8s/tree/profiling-results/latest
- Pixel 7: https://github.com/centerionware/not-k8s/tree/profiling-results/history/2026-08-09_00-59-17-arm64-phone

Still alpha — hasn't been proven under heavy real-world workloads yet — but it's passed 1,100 unit tests and 140 e2e tests, so it's in a good spot to start toying with.

Repo: https://github.com/centerionware/not-k8s

It's designed to support CNI, CSI, DRA (ex GPU's), and CRI. A kubernetes agent that uses much lower resources and doesn't use a garbage collector for it's memory management so it can run on low powered devices like Pi's etc more easily by consuming less resources is a good idea imho. It's been an extremely fun and insightful project.

I wouldn't recommend replacing kubelet with it for anything "critical" (like a media stack used by the family, or vaultwarden, or the website that needs 99.999 uptime), it can be used for things that don't matter to much. Feedback is welcome, bug reports even more so (via GitHub issues), and I'd invite collaboration if anybody is down. Eventually I'd want to replace every part of kubernetes while keeping it fully compatible. The control plane stack uses a lot of resources that I don't think it really needs to be using.

Claude Code was used to research and generate virtually everything in this project from the code to the unit tests and e2e tests. It built it various times for testing in various places and created the GitHub actions workflows all at the directions of me, a 30+ year software developer. It was all done on a $20/mo claude subscription and would have cost around $2,000usd on API tier. It was built almost exclusively with sonnet5 on medium. The numbers above are checkable either way, the CSVs are right there.
