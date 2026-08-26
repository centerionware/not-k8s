# Vendored manifests

Real upstream manifests, fetched verbatim (not hand-reconstructed) — same
discipline `APISERVER_PLAN.md` finding 5's OpenAPI spec vendoring and
`deploy/lib/e2e-full-setup.sh`'s CSI/DRA driver vendoring already use in
this project.

## `coredns.yaml`

Source: `k3s-io/k3s` repo's own `manifests/coredns.yaml`, at tag
**`v1.33.13+k3s2`** (the latest v1.33 patch release as of 2026-08-22 —
matches this workspace's `k8s-openapi` `v1_33` feature pin).

```bash
gh api "repos/k3s-io/k3s/contents/manifests/coredns.yaml?ref=v1.33.13%2Bk3s2" \
  -H 'Accept: application/vnd.github.raw' > crates/nodebootstrap/vendor/coredns.yaml
```

Fetched from k3s's own repo rather than `coredns/deployment` upstream
because k3s's copy is the one this project's existing (k3s-based) deploy has
actually run and proven against real e2e — same reasoning as reusing k3s's
PKI in `deploy/lib/upstream-kube-apiserver.sh` before this crate existed,
minus the PKI-borrowing part `docs/NODEBOOTSTRAP_PLAN.md` deliberately drops.
Confirmed it is a genuine, complete, ready-to-apply manifest (ServiceAccount
+ ClusterRole + ClusterRoleBinding + ConfigMap + Deployment + Service), not
a k3s-specific fork of the upstream one.

**Template placeholders** (`%{...}%`, k3s's own Go-template syntax, filled
in by `manifests.rs::render_coredns()`):

| Placeholder | Filled with |
|---|---|
| `%{CLUSTER_DOMAIN}%` | `cluster.local` (this project's fixed default — nothing in `nodebootstrap` makes this configurable yet) |
| `%{CLUSTER_DNS}%` | the cluster DNS ClusterIP (`Config`'s DNS IP, default `10.43.0.10`) |
| `%{CLUSTER_DNS_LIST}%` | `[<CLUSTER_DNS>]` (YAML list syntax) |
| `%{CLUSTER_DNS_IPFAMILYPOLICY}%` | `SingleStack` (dual-stack not yet supported by this crate) |
| `%{SYSTEM_DEFAULT_REGISTRY}%rancher/mirrored-coredns-coredns:1.14.6` | the whole image reference is substituted wholesale with `registry.k8s.io/coredns/coredns:v1.14.6` -- once k3s (and its Rancher-mirrored images) is gone, there is no reason to keep pulling through Rancher's mirror instead of the canonical upstream image. |

Refresh by re-running the `gh api` command above against a newer `k3s-io/k3s`
tag, recording the new tag in this file, and re-checking the placeholder
table and image-substitution line still match (both have changed across k3s
releases before).

## Flannel service setup

The flannel service wrapper is no longer vendored. `cni.rs`'s Rust
`flanneld` service applet rewrites `/etc/kube-flannel/net-conf.json`, waits for
the node PodCIDR, selects the default interface, and then starts flanneld on
every supervised process start.
