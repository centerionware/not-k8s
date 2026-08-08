# not-k8s: replaced kubelet with a 15MB Rust agent

**nodelet** is a drop-in replacement for kubelet — the Kubernetes node agent — written in Rust, event-driven instead of polling.

Benchmarked against upstream kubelet, both completely idle, six separate runs so the numbers couldn't be biased by testing back to back on an already-warmed-up box:

- **Memory** — ~15MB nodelet vs ~81MB kubelet (~5x lower)
- **CPU (2min idle)** — ~0.08s nodelet vs ~0.85s kubelet (~10x lower)

Those numbers look small per node, but idle cost scales with fleet size, not workload:

- **100 nodes** — ~1.5GB vs ~8.1GB RAM burned just sitting idle
- **1,000 nodes** — ~15GB vs ~81GB RAM burned just sitting idle

Same cluster, same workload, nothing running yet — that gap is pure idle overhead, and it only gets bigger as the fleet does.

Full raw data + methodology: https://github.com/centerionware/not-k8s/blob/profiling-results/latest/README.md

Still alpha — hasn't been proven under heavy real-world workloads yet — but it's passed 1,100 unit tests and 140 e2e tests, so it's in a good spot to start toying with.

Repo: https://github.com/centerionware/not-k8s

Claude Code was used to research and generate virtually everything in this project from the code to the unit tests and e2e tests. It built it various times for testing in various places and created the GitHub actions workflows all at the directions of me, a 30+ year software developer. It was all done on a $20/mo claude subscription and would have cost around $2,000usd on API tier. It was built almost exclusively with sonnet5 on medium.

A kubernetes agent that uses much lower resources and doesn't use a garbage collector for it's memory management so it can run on low powered devices like Pi's etc more easily by consuming less resources is a good idea imho.

It's designed to support CNI, CSI, DRA (ex GPU's), and CRI. The e2e tests prove it mostly works before any release is built. It's been an extremely fun and insightful project. 

I wouldn't recommend replacing kubelet with it for anything "critical" (like a media stack used by the family, or vaultwarden, or the website that needs 99.999 uptime), it can be used for things that don't matter to much. Feedback is welcome, bug reports even more so (via GitHub issues), and I'd invite collaboration if anybody is down. Eventually I'd want to replace every part of kubernetes while keeping it fully compatible. The control plane stack uses a lot of resources that I don't think it really needs to be using. 
