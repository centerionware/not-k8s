# not-k8s: replaced kubelet with a 15MB Rust agent

Been building this for a while — a k3s control plane (apiserver, scheduler, full kubectl/CRD support) with kubelet swapped out for **nodelet**, a leaner event-driven agent written in Rust.

Benchmarked it against upstream kubelet, both completely idle, same control plane, six separate runs so I couldn't bias the numbers:

- **Memory** — ~15MB nodelet vs ~81MB kubelet
- **CPU (2min idle)** — ~0.08s nodelet vs ~0.85s kubelet

Full raw data + methodology: https://github.com/centerionware/not-k8s/blob/profiling-results/latest/README.md

Still alpha — hasn't been proven under heavy real-world workloads yet — but it's passed 1,100 unit tests and 140 e2e tests, so it's in a good spot to start toying with.

Repo: https://github.com/centerionware/not-k8s

Per this repo rules: Claude Code was used to research and generate virtually everything in this project from the code to the unit tests and e2e tests. It built it various times for testing in various places and created the GitHub actions workflows all at the directions of me, a 30+ year software developer. It was all done on a $20/mo claude subscription and would have cost around $2,000usd on API tier. It was built almost exclusively with sonnet5 on medium.

It's relevant to r/homelab because many if not most people use containers to run things, and while docker/podman is great and all some like to run kubernetes. A kubernetes agent that uses much lower resources and doesn't use a garbage collector for it's memory management so it can run on low powered devices like Pi's etc more easily by consuming less resources is a good idea imho.

It's designed to support CNI, CSI, DRA (ex GPU's), and CRI. The e2e tests prove it mostly works before any release is built. It's been an extremely fun and insightful project. 

I wouldn't recommend replacing kubelet with it for anything "critical" (like a media stack used by the family, or vaultwarden), it can be used for things that don't matter to much. Feedback is welcome, bug reports even more so (via GitHub issues), and I'd invite collaboration if anybody is down. Eventually I'd want to replace every part of kubernetes while keeping it fully compatible. The control plane stack uses a lot of resources that I don't think it really needs to be using. 
