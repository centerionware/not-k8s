# not-k8s: replaced kubelet with a 15MB Rust agent, profiled on x86 and a phone

**nodelet** — drop-in kubelet replacement, Rust, event-driven instead of polling. Same control plane, same kubectl, same CRDs.

Idle, no pods, 120s, 3 replicates each:

| | nodelet | kubelet | gap |
|---|---|---|---|
| **x86_64** | 15MB / 0.08s CPU | 81MB / 0.85s CPU | 5.4x / 10.6x |
| **Pixel 7** | 12MB / 0.44s CPU | 68MB / 8.03s CPU | 5.7x / 18.4x |

CSVs: [x86_64](https://github.com/centerionware/not-k8s/tree/profiling-results/latest) · [Pixel 7](https://github.com/centerionware/not-k8s/tree/profiling-results/history/2026-08-09_00-59-17-arm64-phone)

Alpha. 1,100 unit tests, 140 e2e. Repo: https://github.com/centerionware/not-k8s

Supports CNI, CSI, DRA (ex GPU's), and CRI. A kubernetes agent that uses much lower resources and doesn't use a garbage collector for it's memory management so it can run on low powered devices like Pi's etc more easily by consuming less resources is a good idea imho.

I wouldn't recommend replacing kubelet with it for anything "critical" (a family media stack, vaultwarden, the website that needs 99.999 uptime) — use it for things that don't matter to much. Feedback welcome, bug reports more so, collaboration if anybody is down. Eventually I'd want to replace every part of kubernetes while keeping it fully compatible. The control plane stack uses a lot of resources that I don't think it really needs to be using.

Claude Code wrote virtually all of it — code, unit tests, e2e tests, CI — at my direction, 30+ year dev. $20/mo subscription, would've been ~$2,000 on API tier, almost all sonnet5 on medium. Numbers are checkable either way, CSVs are right there.
