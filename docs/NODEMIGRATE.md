# nodemigrate

`nodemigrate` is a standalone operator binary. It is a workspace crate, but it
is not linked into the combined `notk8s` binary. Its release uses the latest
regular project release's version and does not update `VERSION`.

The migration replaces a K3s installation on this host with the not-k8s stack
while carrying the cluster's Kubernetes API state to the destination. It
supports single-node and multi-node source clusters, bundled Flannel or an
externally managed CNI such as Cilium, and a new local destination or joining
an existing nodestore cluster.

```sh
sudo nodemigrate inspect
sudo nodemigrate to=nodestore from=k3s plan-only=true
sudo nodemigrate to=nodestore from=k3s
```

Set `NODEBOOTSTRAP_JOIN_ENDPOINT` and `NODEBOOTSTRAP_PEER_URL` (and the
nodebootstrap join CA/certificate/key settings) when the replacement should
join an existing nodestore cluster. `nodemigrate` then starts nodebootstrap in
control-plane join mode. The source K3s `node-name`, cluster domain, service
and pod CIDRs are carried into the replacement configuration.

The utility exports API resources, including custom resources and system
add-ons such as Cilium, through the source API, then applies them through the
destination API. Kubernetes-managed transient resources are recreated by the
destination controllers. Owner references and persistent-volume claim UIDs
are remapped to the destination UIDs. Kine and etcd database files are not
copied between distributions.

If the source uses an external CNI, nodebootstrap is started with CNI setup
disabled so it leaves the provider's host configuration in place. Add-on API
objects are restored after the destination is ready. A CNI provider remains
responsible for its own host binaries, interfaces, routes, and kernel state.

PersistentVolume and PersistentVolumeClaim objects are migrated. For local
and hostPath PersistentVolumes, nodemigrate snapshots the referenced host path
into the protected export before changing services. If
`uninstall-after-migrate=true` invokes the K3s uninstall script, the host path
is restored afterward. Network and CSI backed volume payloads stay with their
storage provider.

The source service is stopped and disabled after the protected export is
written. The K3s installation remains available for recovery by default. The
export, including Secrets and local volume snapshots, is stored under
`/var/lib/nodemigrate/exports` with restrictive permissions. If bootstrap
fails before the destination API becomes ready, nodemigrate restores the
source service's previous state. If destination bootstrap or import fails
after cutover, the source remains stopped to avoid competing API servers; the
export path is reported for recovery.

```sh
sudo nodemigrate to=nodestore from=k3s uninstall-after-migrate=true
```

This is an API-level cluster migration, not a byte-for-byte K3s datastore
restore. The export is retained after success so operators can inspect it or
recover individual objects and local volume data.
