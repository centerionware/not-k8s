Kubelet, the agent running on every Kubernetes node, idles at about 81MB of RAM and burns real CPU before your cluster does anything useful.

Kubelet's job is necessary: watch for pods, talk to the container runtime, report status back. But it also polls constantly, runs its own container-stats housekeeping, and keeps a pile of watch caches alive the whole time it's idle.

not-k8s replaces just that node agent with nodelet — a leaner, event-driven Rust binary. Everything else about the cluster stays the same; only the per-node agent changes.

Benchmarked, not just claimed:

- Installed nodelet, let it sit idle, measured real memory and CPU time
- Ran the identical test with a standalone upstream kubelet binary
- Same container runtime, only the node agent changed
- Six separate runs on six separate machines, so the numbers couldn't be biased by testing back to back on a warmed-up box

Completely idle, zero workload running:

- Memory — about 15MB for nodelet, about 81MB for kubelet (~5x lower)
- CPU time over a 2-minute idle window — about 0.08s vs about 0.85s (~10x lower)

That's the cost of having the node agent installed, before it's done anything — and both numbers trace back to the same root cause:

- CPU-seconds are a direct energy cost. Every polling loop (PLEG relisting every 1s, cAdvisor scraping every 10-15s, watch caches getting rewritten) burns real joules whether or not anything's running.
- The RAM difference is also a real energy cost, just not the way "more bytes resident" implies. DRAM refresh itself doesn't care about content or usage — idle capacity gets refreshed either way. What actually costs energy is active memory traffic: reads, writes, row activations. Active DRAM draws roughly 1-3W/GB versus single-digit milliwatts/GB in self-refresh — a 100-1000x gap. Kubelet's polling loops don't just burn CPU to run — they constantly scan and rewrite real memory to do it, which is exactly what keeps DRAM in that expensive active state instead of dropping into self-refresh.
- On top of that, RAM is also a capacity cost: memory kubelet ties up is memory you can't schedule other pods into, so you provision more of it to fit the same workload.

Priced at AWS Fargate's own published per-resource rate (the cleanest real $/GB-hr and $/vCPU-hr number available, since standard EC2 bundles memory into instance pricing — $0.00444/GB-hr, $0.04048/vCPU-hr, us-east-1), reclaiming just the idle overhead is worth about $0.40/node/month. Nothing on one node. At 1,000 nodes that's roughly $400/month (~$4,800/year) — on top of ~66GB of RAM freed up to actually run pods instead of sitting reserved for a node agent's own idle housekeeping.

For context: average Kubernetes clusters run at only ~20% memory utilization and ~8-10% CPU utilization industry-wide, and cloud spend on idle resources is projected at $27.1B in 2026. nodelet doesn't touch workload-level overprovisioning — that's a separate, much bigger problem — but it closes the one slice of that waste that's kubelet's own fault.

Open source, still early, raw data and charts included with every run:

Report: https://github.com/centerionware/not-k8s/blob/profiling-results/latest/README.md
Repo: https://github.com/centerionware/not-k8s

#Kubernetes #Rust #EdgeComputing
