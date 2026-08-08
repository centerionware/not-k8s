# not-k8s

**A drop-in kubelet replacement that doesn't melt your battery.**

`not-k8s` is **not** a from-scratch Kubernetes — it's a lightweight replacement for one component: kubelet, the node agent, backed by 1,100+ unit tests and ~140 e2e tests. Everything else stays a k3s control plane (apiserver, scheduler, full kubectl/CRD support). Only the node side gets rebuilt, as a lean event-driven Rust binary, because that's where the heaviest polling loops live.

The core idea: kubelet idles at 30–50% of a CPU core on an edge device, not from doing actual work, but from constant polling (PLEG re-lists every container every second, cAdvisor walks cgroups forever, leases renew every 2s, informers periodically re-list the world). `not-k8s` keeps the control-plane API surface and rebuilds the node side to be event-driven: no PLEG, no cAdvisor housekeeping, one process, one watch.

This exists for embedded/edge fleets where that idle cost is real money and real battery — think clusters of Pis, mini PCs, or (yes, really) farms of old Android phones repurposed as compute nodes. `not-k8s` aims to stay as close to upstream kubelet's behavior as possible, not to reinvent the node agent's contract.

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

The design prioritizes single-node edge deployments — that's where low idle CPU shines — but `not-k8s` is built to be a drop-in kubelet replacement usable in multi-node Kubernetes clusters too. For now, the quickest and easiest way to try it is on top of k3s. If you want to run it against a full upstream Kubernetes control plane, there's nothing stopping you — it's the same kubelet replacement protocol either way.

To be clear about what this is *not*: `not-k8s` doesn't touch the apiserver, scheduler, controller-manager, or etcd/kine — it only replaces the node agent. Feature-parity claims against kubelet aren't taken on faith from the design doc; they're verified by a real e2e suite against real containerd, real CSI/DRA drivers, and real clusters — not just unit tests against mocks. That suite runs as a required gate in the [release pipeline](https://github.com/centerionware/not-k8s/actions) before any release is cut, and it has to pass everything it can; check the Actions history for actual run results rather than taking that on faith either.

## Profiling

See live idle-CPU and RSS comparison numbers (nodelet vs. stock k3s kubelet) in the [`profiling-results`](https://github.com/centerionware/not-k8s/tree/profiling-results) branch — published automatically by CI on every release.

## Learn more

- **[`ARCHITECTURE.md`](docs/ARCHITECTURE.md)** — Design rationale and trade-offs.

## Support

If this project has been useful to you, consider supporting it: https://buymeacoffee.com/centerionww

## License

MIT OR Apache-2.0, at your option. See [`LICENSE-MIT`](LICENSE-MIT) and
[`LICENSE-APACHE`](LICENSE-APACHE).
