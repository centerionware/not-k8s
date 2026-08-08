Kubelet, the agent that runs on every Kubernetes node, is heavier than it needs to be. It polls constantly, runs its own container-stats housekeeping, keeps a pile of watch caches alive — all before your cluster has done a single useful thing. On a Raspberry Pi or an edge box, that idle cost can rival the actual workload you're trying to run. On a big server it's the same cost, just easier to lose in the noise.

So I built not-k8s. Real Kubernetes control plane underneath — real apiserver, real scheduler, full kubectl and CRD compatibility — with the node agent swapped out for a leaner Rust binary called nodelet. Event-driven instead of polling-driven.

Saying it's more efficient isn't worth much without proof, so I built a real benchmark. Install nodelet, let it sit idle, measure real memory and CPU time. Then run the identical test with a genuine, unmodified, standalone upstream kubelet binary — same control plane, same runtime, nothing else changed. Six separate runs across separate machines so I couldn't accidentally bias the numbers by testing back to back on a warmed-up box.

Completely idle, no workload running at all:

Memory — about 15MB for nodelet, about 81MB for kubelet.
CPU time over a 2-minute idle window — about 0.08s vs about 0.85s.

That's just the cost of having the node agent installed, before it's done anything.

It matters most on something small — a Pi, an edge box, a fleet of them. It doesn't stop mattering on a big server, it's just easier to ignore there. Every idle cycle kubelet burns is a cycle your actual workload doesn't get, on every node you run.

It's open source, still early, and every benchmark run publishes its raw data and charts alongside the summary:

Report: https://github.com/centerionware/not-k8s/blob/profiling-results/latest/README.md
Repo: https://github.com/centerionware/not-k8s

If you're running Kubernetes anywhere — a Pi cluster or a fleet of servers — I'd like to know if this would actually help you.

#Kubernetes #Rust #EdgeComputing #OpenSource
