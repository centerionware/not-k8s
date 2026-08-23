Started because I wanted to run a dev cluster on my phone without destroying the battery.

# not-k8s

**A drop-in kubelet replacement small enough to run where kubelet won't fit.**
**Another Rust-based Kubernetes clone.**

`not-k8s` is **becoming** a Kubernetes distro.

Older Measured idle, no pods scheduled, 120s window, 3 replicates per agent:

| | nodelet | upstream kubelet | gap |
|---|---|---|---|
| **x86_64** (CI) | ~15MB / ~0.08s CPU | ~81MB / ~0.85s CPU | 5.4x / 10.6x |
| **ARM phone** (Pixel 7, KVM) | 12.0MB / 0.436s CPU | 67.9MB / 8.031s CPU | 5.7x / **18.4x** |

Some profiling results:
both: [x86_64](https://github.com/centerionware/not-k8s/tree/profiling-results/latest),
[ARM phone](https://github.com/centerionware/not-k8s/tree/profiling-results/history/2026-08-09_00-59-17-arm64-phone).

## Get started

It's going to be something like

```
wget https://github.com/centerionware/not-k8s/releases/download/v0.7.0/notk8s-0.7.0-linux-aarch64-release
chmod +x notk8s-0.7.0-linux-aarch64-release
ln -s ./notk8s-0.7.0-linux-aarch64-release bootstrap
./bootstrap --with-cri
```

## Scope

most everything except apiserver (apiserver in progress)

## Testing

A ridiculous amount of unit regression testing and a good amount of e2e testing, it could use more.

## Profiling

[`profiling-results`](https://github.com/centerionware/not-k8s/tree/profiling-results)

## Learn more

- **[`ARCHITECTURE.md`](docs/ARCHITECTURE.md)** — Semi Deprecated
- **A lot of the docs suck**

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md). Commit messages follow
[Conventional Commits](https://www.conventionalcommits.org/) and are checked in
CI on every PR.

## License

MIT OR Apache-2.0, at your option. See [`LICENSE-MIT`](LICENSE-MIT) and
[`LICENSE-APACHE`](LICENSE-APACHE).
