# not-k8s: replaced kubelet with a 15MB Rust agent

Been building this for a while — a real Kubernetes control plane (real apiserver, real scheduler, full kubectl/CRD support) with kubelet swapped out for **nodelet**, a leaner event-driven agent written in Rust.

Benchmarked it against upstream kubelet, both completely idle, same control plane, six separate runs so I couldn't bias the numbers:

- **Memory** — ~15MB nodelet vs ~81MB kubelet
- **CPU (2min idle)** — ~0.08s nodelet vs ~0.85s kubelet

![RSS over time, nodelet vs upstream kubelet](https://raw.githubusercontent.com/centerionware/not-k8s/profiling-results/latest/rss-over-time.png)

![CPU % over time, nodelet vs upstream kubelet](https://raw.githubusercontent.com/centerionware/not-k8s/profiling-results/latest/cpu-over-time.png)

Full raw data + methodology: https://github.com/centerionware/not-k8s/blob/profiling-results/latest/README.md

Still alpha — hasn't been proven under heavy real-world workloads yet — but it's passed 1,100 unit tests and 140 e2e tests, so it's in a good spot to start toying with.

Repo: https://github.com/centerionware/not-k8s
