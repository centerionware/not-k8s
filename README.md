The scope of this project has changed to slowly become a full kubernetes distro but in rust. I'm one guy with some tools. this readme is pretty out of date. the only component that's not merged to the main branch is apiserver and it's being actively worked on. it's targeting upstream 1.34 plus DRA, and after all the parts are made and a new bootstrap method is made I'll begin to retarget it to the latest (1.36.3 at this time afaik). it can still operate as kubelet and Kube-proxy replacements with an upstream control plane, and if I have anything to say about it always will. 

I don't expect it to be bug free, or security problem free, because this is a friggin massive undertaking done by one guy and two/three ai agents. I do have 30+ years of experience coding though so methinks that gives me an edge when it comes to understanding what it's actually outputting. I try to review a lot of the code manually but it's so much, and I'm not being paid for this (yet?) so don't expect me to review everything.


# not-k8s

**A drop-in kubelet replacement small enough to run where kubelet won't fit.**

`not-k8s` is **not** a from-scratch Kubernetes — it's a lightweight replacement for the node side: `nodelet` (kubelet, the node agent) and `nodeproxy` (kube-proxy), backed by 1,100+ unit tests and ~140 e2e tests. Everything else stays a k3s control plane (apiserver, scheduler, full kubectl/CRD support). Only the node side gets rebuilt, in lean event-driven Rust, because that's where the heaviest polling loops live.

The core idea: kubelet's idle cost isn't from doing actual work, it's from constant polling (PLEG re-lists every container every second, cAdvisor walks cgroups forever, informers periodically re-list the world). `nodelet` rebuilds the node side to be event-driven — no PLEG, no cAdvisor housekeeping, one process, one watch. Measured idle, no pods scheduled, 120s window, 3 replicates per agent:

| | nodelet | upstream kubelet | gap |
|---|---|---|---|
| **x86_64** (CI) | ~15MB / ~0.08s CPU | ~81MB / ~0.85s CPU | 5.4x / 10.6x |
| **ARM phone** (Pixel 7, KVM) | 12.0MB / 0.436s CPU | 67.9MB / 8.031s CPU | 5.7x / **18.4x** |

The x86_64 CPU share is a fraction of a percent of a fast core, and easy to wave off. That's the wrong way to read it: the polling is a *fixed* amount of work, so the slower the core, the bigger the bite — and the gap widens from ~10.6x to ~18.4x going from a server core to a phone core, while the memory ratio stays flat. Raw per-second CSVs for both: [x86_64](https://github.com/centerionware/not-k8s/tree/profiling-results/latest), [ARM phone](https://github.com/centerionware/not-k8s/tree/profiling-results/history/2026-08-09_00-59-17-arm64-phone).

Worth knowing before you read too much into it: on that same phone the k3s control plane itself idles at ~34% of a core and ~350MB — more than either node agent. Swapping the agent is a real saving that doesn't touch the biggest line item.

Per node that saving is modest, and on a 64GB server it's a rounding error. The point isn't the megabytes — it's that the node agent's floor decides which hardware can be a Kubernetes node at all. Kubernetes' API solves a lot of what small-hardware fleets actually need (declarative rollouts, health checks, restart policy, config/secret distribution, real RBAC), but the orchestrator that already solved it assumes every node can spare hundreds of megabytes before running a container. The control plane can live on a server somewhere; only the node agent has to run on the constrained device. `not-k8s` aims to stay as close to upstream kubelet's behavior as possible, not to reinvent the node agent's contract.

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

By default, two binaries get installed, as two independent services: `nodelet` (the node agent — the kubelet replacement) and `nodeproxy` (Service/ClusterIP/NodePort routing via nftables — kube-proxy's job). They're separate for the same reason kubelet and kube-proxy are separate upstream: a node can use Cilium, a real kube-proxy, or nothing for service routing without changing the node agent. Pass `--proxy=none` if something else already owns that datapath on your node.

## Scope

The design prioritizes single-node edge deployments — that's where low idle CPU shines — but nothing about it is single-node-specific. For now, the quickest and easiest way to try it is on top of k3s; running it against a full upstream Kubernetes control plane works the same way.

Feature-parity claims against kubelet aren't taken on faith from the design doc; they're verified by an e2e suite that exercises containerd, CSI/DRA drivers, and full clusters end to end — not just unit tests against mocks. That suite runs as a required gate in the [release pipeline](https://github.com/centerionware/not-k8s/actions) before any release is cut, and it has to pass everything it can; check the Actions history for actual run results rather than taking that on faith either.

## Profiling

See live idle-CPU and RSS comparison numbers (nodelet vs. stock k3s kubelet) in the [`profiling-results`](https://github.com/centerionware/not-k8s/tree/profiling-results) branch.

## Learn more

- **[`ARCHITECTURE.md`](docs/ARCHITECTURE.md)** — Design rationale and trade-offs.

## Support

If this project has been useful to you, consider supporting it: https://buymeacoffee.com/centerionww

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md). Commit messages follow
[Conventional Commits](https://www.conventionalcommits.org/) and are checked in
CI on every PR.

## License

MIT OR Apache-2.0, at your option. See [`LICENSE-MIT`](LICENSE-MIT) and
[`LICENSE-APACHE`](LICENSE-APACHE).
