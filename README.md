# not-k8s

**A single-device Kubernetes that doesn't melt your battery.**

`not-k8s` is a lightweight Kubernetes node agent that replaces kubelet with a lean, event-driven Rust binary. Keep a real (stripped) Kubernetes control plane for 1:1 `kubectl` and CRD compatibility, but slash the idle CPU and RAM overhead by rebuilding only the node side — because that's where the heaviest polling loops live.

The core idea: kubelet idles at 30–50% of a CPU core on an edge device, not from doing actual work, but from constant polling (PLEG re-lists every container every second, cAdvisor walks cgroups forever, leases renew every 2s, informers periodically re-list the world). `not-k8s` keeps the control-plane API surface and rebuilds the node side to be event-driven: no PLEG, no cAdvisor housekeeping, one process, one watch.

## Get started

**No clone needed** — fetches a prebuilt binary for your architecture and installs everything else around it:

```bash
curl -fsSL https://raw.githubusercontent.com/centerionware/not-k8s/install-scripts/install.sh | bash -s -- --with-cri
```

**From a prebuilt release, if you'd rather have the repo on disk too:**

```bash
git clone https://github.com/centerionware/not-k8s && cd not-k8s && \
  ./deploy/bootstrap-release.sh --with-cri
```

**From source** (if you want to modify or iterate):

```bash
git clone https://github.com/centerionware/not-k8s && cd not-k8s && \
  ./deploy/bootstrap-source.sh --with-cri
```

All three are self-contained: they detect your distro and CPU architecture and install everything else needed (containerd, k3s, CNI); the two prebuilt-binary paths need no Rust toolchain at all, the from-source path builds and then cleans up its build tools afterward. Pin the one-liner to a specific release instead of always-latest with `install-v<version>.sh` in place of `install.sh`.

## Scope

The design prioritizes single-node edge deployments — that's where low idle CPU shines — but `not-k8s` is built to be a genuine drop-in kubelet replacement usable in multi-node Kubernetes clusters too. For now, the quickest and easiest way to try it is on top of k3s. If you want to run it against a full upstream Kubernetes control plane, there's nothing stopping you — it's the same kubelet replacement protocol either way.

## Profiling

See live idle-CPU and RSS comparison numbers (nodelet vs. stock k3s kubelet) in the [`profiling-results`](https://github.com/centerionware/not-k8s/tree/profiling-results) branch — published automatically by CI on every release.

## Learn more

- **[`DEPLOYMENT_GUIDE.md`](docs/DEPLOYMENT_GUIDE.md)** — Full setup, running, testing, and configuration guide.
- **[`ARCHITECTURE.md`](docs/ARCHITECTURE.md)** — Design rationale and trade-offs.
- **[`GAP_CLOSURE.md`](docs/GAP_CLOSURE.md)** — Detailed feature parity checklist against upstream kubelet.

## Support

If this project has been useful to you, consider supporting it: https://buymeacoffee.com/centerionww

## License

MIT OR Apache-2.0, at your option. See [`LICENSE-MIT`](LICENSE-MIT) and
[`LICENSE-APACHE`](LICENSE-APACHE).
