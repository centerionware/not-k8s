# not-k8s: replaced kubelet with a 15MB Rust agent

**nodelet** is a drop-in replacement for kubelet — the Kubernetes node agent — written in Rust, event-driven instead of polling.

Benchmarked against upstream kubelet, both completely idle, six separate runs so the numbers couldn't be biased by testing back to back on an already-warmed-up box:

- **Memory** — ~15MB nodelet vs ~81MB kubelet (~5x lower)
- **CPU (2min idle)** — ~0.08s nodelet vs ~0.85s kubelet (~10x lower)

Both numbers trace back to the same root cause, and both are real costs:

- **The CPU-seconds are a direct energy cost** — every polling loop (PLEG relisting every 1s, cAdvisor scraping every 10-15s, watch caches getting rewritten) burns real joules whether or not anything's running.
- **The RAM difference is also a real energy cost, just not the way "more bytes resident" implies.** DRAM refresh itself doesn't care about content or usage — a 0 and a 1 cost the same, and idle capacity gets refreshed regardless. What actually costs energy is *active* memory traffic: reads, writes, row activations. Active DRAM draws roughly 1-3W/GB; the same memory sitting in self-refresh draws single-digit milliwatts/GB — a 100-1000x gap. Kubelet's polling loops don't just burn CPU to run — they constantly scan and rewrite real memory to do it, which is exactly what keeps DRAM in that expensive active state instead of dropping into self-refresh. nodelet touches memory far less often for the same reason it burns less CPU: it's not polling.
- **On top of the energy cost, RAM is also a capacity cost** — memory kubelet ties up is memory you can't schedule other pods into, so you provision more of it to fit the same workload.

Priced at AWS Fargate's own published per-resource on-demand rate (the cleanest real $/GB-hr and $/vCPU-hr number that exists, since normal EC2 bundles memory into instance pricing — $0.00444/GB-hr, $0.04048/vCPU-hr, us-east-1), reclaiming just the idle CPU+RAM overhead works out to about **$0.40/node/month**. Nothing on one node. At **1,000 nodes that's ~$400/month (~$4,800/year)** — on top of ~66GB of RAM freed up to actually run pods instead of sitting reserved for a node agent's own idle housekeeping.

For scale: average Kubernetes clusters run at only ~20% memory utilization and ~8-10% CPU utilization industry-wide, and cloud spend on idle resources is projected at $27.1B in 2026. nodelet doesn't touch workload-level overprovisioning — that's a separate, much bigger problem — but it does close the one slice of that waste that's kubelet's own fault, not anything you're running.

Full raw data + methodology: https://github.com/centerionware/not-k8s/blob/profiling-results/latest/README.md

Still alpha — hasn't been proven under heavy real-world workloads yet — but it's passed 1,100 unit tests and 140 e2e tests, so it's in a good spot to start toying with.

Repo: https://github.com/centerionware/not-k8s

Claude Code was used to research and generate virtually everything in this project from the code to the unit tests and e2e tests. It built it various times for testing in various places and created the GitHub actions workflows all at the directions of me, a 30+ year software developer. It was all done on a $20/mo claude subscription and would have cost around $2,000usd on API tier. It was built almost exclusively with sonnet5 on medium.

A kubernetes agent that uses much lower resources and doesn't use a garbage collector for it's memory management so it can run on low powered devices like Pi's etc more easily by consuming less resources is a good idea imho.

It's designed to support CNI, CSI, DRA (ex GPU's), and CRI. The e2e tests prove it mostly works before any release is built. It's been an extremely fun and insightful project. 

I wouldn't recommend replacing kubelet with it for anything "critical" (like a media stack used by the family, or vaultwarden, or the website that needs 99.999 uptime), it can be used for things that don't matter to much. Feedback is welcome, bug reports even more so (via GitHub issues), and I'd invite collaboration if anybody is down. Eventually I'd want to replace every part of kubernetes while keeping it fully compatible. The control plane stack uses a lot of resources that I don't think it really needs to be using. 
