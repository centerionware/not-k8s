Spent the last few months on a dumb question: what if you just didn't run kubelet?

Kubelet is the Kubernetes node agent — it's on every node, watching for pods, talking to the container runtime, reporting status up to the control plane. Necessary, but also kind of heavy. It polls a lot, runs its own container-stats housekeeping, keeps a bunch of watch caches going. Most noticeable on small hardware, like a Raspberry Pi or an edge device, where the node agent's own idle cost can rival whatever workload you're actually trying to run — but a big server pays the same cost too, it's just easier to lose in the noise.

So I built something called not-k8s. It keeps a real Kubernetes control plane — real apiserver, real scheduler, full kubectl and CRD compatibility — and just replaces the node agent with a leaner Rust binary called nodelet. Event-driven instead of polling-driven.

Claiming it's more efficient doesn't mean much on its own, so I built a real benchmark to back it up. Install nodelet, let it sit idle, measure real memory and CPU time. Then run the same test with a genuine, unmodified, standalone upstream kubelet binary — same control plane, same runtime, nothing else changed. Six separate runs across separate machines so I couldn't accidentally bias it by testing back to back on a warmed-up box.

Completely idle, no workload running at all:

Memory — about 15MB for nodelet, about 81MB for kubelet.
CPU time over a 2-minute idle window — about 0.08s vs about 0.85s.

Just the cost of having the node agent installed, before it's done a single useful thing.

Matters most on something small — a Pi, an edge box, a fleet of them. But it doesn't stop mattering on a big server, it's just easier to ignore there. Every idle cycle kubelet burns is a cycle your actual workload doesn't get, on every node you run.

It's open source, still early, and every benchmark run publishes its raw data and charts alongside the summary:

Report: https://github.com/centerionware/not-k8s/blob/profiling-results/latest/README.md
Repo: https://github.com/centerionware/not-k8s

If you're running Kubernetes anywhere — a Pi cluster or a fleet of servers — curious whether this would actually help you.

#Kubernetes #Rust #EdgeComputing #OpenSource
