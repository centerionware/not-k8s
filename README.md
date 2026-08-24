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
./bootstrap
```

Common commands after downloading the release binary:

```bash
./bootstrap                         # install or update the stack (CRI is the default)
./bootstrap --without-cri           # opt out of containerd/CRI and use mock runtime
./bootstrap --release              # update from the latest published assets
./bootstrap --without-flannel      # use an external CNI and remember it on updates
./bootstrap --without-flannel --proxy=none
                                     # full single-node CP+worker for Cilium-style setups
./bootstrap --worker --kubeconfig=/path/to/cluster.kubeconfig --proxy=none
                                     # nodelet; no local control plane, flannel, or proxy
./bootstrap --control-plane --join=https://cp-1:2379 --peer-url=https://cp-2:2380
                                     # add a nodestore control-plane member
./bootstrap --remove-control-plane --join=https://cp-1:2379 --member-id=123
                                     # remove the member, then remove local CP services
./bootstrap --e2e                   # run bootstrap-native checks against the cluster
./bootstrap --e2e --only=node       # run one check by name substring
./bootstrap --e2e --shard=1/5       # run one CI shard (normally set by GitHub Actions)
```

`--e2e` does not install or restart anything. It uses `$KUBECONFIG` when set,
otherwise it discovers the admin kubeconfig written by nodebootstrap at
`/etc/nodebootstrap/admin.kubeconfig` (or `NODEBOOTSTRAP_KUBECONFIG_DIR`).
The bootstrap applet is the only repository e2e entrypoint: it uses the Rust
Kubernetes client directly and currently checks API resource serving, node
readiness, and the default Kubernetes Service endpoint. The former installer
and shell e2e tree is preserved on the
[`archive-shell-scripts-0.7.1`](https://github.com/centerionware/not-k8s/tree/archive-shell-scripts-0.7.1)
branch while the remaining checks move into the applet under
[issue #242](https://github.com/centerionware/not-k8s/issues/242).

The no-role invocation is the single-node convenience path: it installs both
the control plane and node services. --worker is the multi-node path and
requires an existing cluster kubeconfig; it installs only nodelet and
nodeproxy. Pass --proxy=none when a CNI such as Cilium also replaces
kube-proxy. Workers do not install flannel by default. --without-flannel
does the same for a single-node bootstrap and persists that choice so later
updates do not resurrect flannel.

--control-plane is an explicit nodestore-only control-plane join. The joining
host must have the existing cluster PKI in its configured PKI directory and
shared nodestore client credentials in NODEBOOTSTRAP_JOIN_CA_FILE,
NODEBOOTSTRAP_JOIN_CERT_FILE, and NODEBOOTSTRAP_JOIN_KEY_FILE (or the
corresponding NODESTORE_* variables). --join points at an existing nodestore
endpoint; --peer-url is the new host's reachable https://HOST:2380 peer
address. The join is added as a learner before the local service starts. The
local member also needs NODESTORE_CERT_FILE/KEY_FILE/TRUSTED_CA_FILE and the
corresponding NODESTORE_PEER_* triple for its server and raft peer identities.
Removal requires --member-id and a different reachable member as --join; local
data and PKI are retained after service removal.

There is no shell installer or shell e2e command in the 0.7.1 tree. Download
the combined `notk8s` binary, symlink it to `bootstrap`, and use the commands
above. Performance-only helpers remain temporarily for the 0.7.4 profiling
migration.

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
