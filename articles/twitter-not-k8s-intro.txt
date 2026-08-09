Replaced kubelet with nodelet, a 15MB event-driven Rust node agent. Profiled both idle, x86 and a Pixel 7:

x86 — 15MB/0.08s vs 81MB/0.85s
phone — 12MB/0.44s vs 68MB/8.03s

CPU gap widens 10.6x → 18.4x on the slower core. CSVs: https://github.com/centerionware/not-k8s
