I replaced kubelet with nodelet, a 15MB event-driven Rust node agent, and profiled both on x86_64 AND on a Pixel 7.

Idle, 120s, 3 replicates:
· x86: 15MB/0.08s vs 81MB/0.85s
· phone: 12MB/0.436s vs 68MB/8.03s

The CPU gap widens 10.6x → 18.4x on the weaker core. Raw CSVs: https://github.com/centerionware/not-k8s
