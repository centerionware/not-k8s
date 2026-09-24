# nodemigrate

`nodemigrate` is a standalone operator binary. It is a workspace crate, but it
is not linked into the combined `notk8s` binary. Its release uses the latest
regular project release's version and does not update `VERSION`.

The migration moves a Kubernetes installation between K3s, upstream
Kubernetes, and the not-k8s stack while carrying Kubernetes API state to the
destination. The utility detects local installations and service managers;
the source stays installed but stopped and disabled after migration unless
`uninstall-after-migrate=true` is requested.

```sh
sudo nodemigrate inspect
sudo nodemigrate to=nodestore from=k3s plan-only=true
sudo nodemigrate to=nodestore from=k3s
sudo nodemigrate to=k3s from=nodestore
sudo nodemigrate to=kubernetes from=nodestore
sudo nodemigrate to=nodestore from=kubernetes
```

For a nodestore target, join settings come directly from nodebootstrap's
environment configuration. Set `NODEBOOTSTRAP_JOIN_ENDPOINT` and
`NODEBOOTSTRAP_PEER_URL` (and the nodebootstrap join CA, certificate, key, and
member settings as needed) when migrating a control-plane node into an
existing cluster. For a worker source, nodemigrate uses nodebootstrap worker
mode against the destination kubeconfig. Set
`NODEMIGRATE_REPLACE_NODE=true` only when an existing same-name destination
Node should be replaced; nodemigrate removes that stale Node after stopping
the source service and waits for the new worker registration to become Ready.
`NODEMIGRATE_NODE_NAME` overrides the detected node name. Cluster-wide API
objects are transferred at the control-plane stage, not re-imported from each
worker. The utility derives service and pod CIDRs, cluster domain, DNS
addresses, and node name from the source installation where available.

The supported directions are K3s or upstream Kubernetes to nodestore, and
nodestore to a retained local K3s or upstream Kubernetes installation. The
return path starts the retained target service and imports the nodestore API
state. The original source stays installed for rollback by default, which
provides the retained local target for a return migration. `KUBECONFIG` may
select an alternate source config, `NODEMIGRATE_SOURCE_KUBECONFIG` explicitly
overrides it, and `NODEMIGRATE_DESTINATION_KUBECONFIG` overrides the target
config.

The utility exports API resources, including custom resources and system
add-ons such as Cilium, through the source API, then applies them through the
destination API. Kubernetes-managed transient resources are recreated by the
destination controllers. Owner references and persistent-volume claim UIDs
are remapped to the destination UIDs. Kine and etcd database files are not
copied between distributions.

If the source uses an external CNI such as Cilium, nodebootstrap is started
with CNI setup disabled so it preserves the provider's host configuration.
This also applies when joining a cluster whose CNI is managed outside
nodebootstrap. Add-on API objects, including Cilium resources, are restored
after the destination is ready. A CNI provider remains responsible for its
host binaries, interfaces, routes, and kernel state.

PersistentVolume and PersistentVolumeClaim objects are migrated during the
control-plane stage. For local and hostPath PersistentVolumes, nodemigrate
snapshots referenced host paths into the protected export before changing
control-plane services. Per-worker volume snapshot and rollback handling is
not yet verified. If
`uninstall-after-migrate=true` invokes the K3s uninstall script, the host path
is restored afterward. Network and CSI backed volume payloads stay with their
storage provider.

The source services are stopped and disabled after the protected export is
written. The source installation remains available for recovery by default.
The export, including Secrets and local volume snapshots, is stored under
`/var/lib/nodemigrate/exports` with restrictive permissions. If bootstrap
fails before the destination API becomes ready, nodemigrate restores the
source service's previous state. If destination bootstrap or import fails
after cutover, the source remains stopped to avoid competing API servers; the
export path is reported for recovery.

```sh
sudo nodemigrate to=nodestore from=k3s uninstall-after-migrate=true
sudo nodemigrate to=k3s from=nodestore uninstall-after-migrate=true
```

`uninstall-after-migrate=true` invokes the source distribution's uninstall
path after the target node is ready and its API objects have been imported.
K3s uses its uninstall script, kubeadm uses `kubeadm reset --force`, and
nodestore uses nodebootstrap's uninstall mode. Without this flag, no source
uninstall is run. This is an API-level cluster migration, not a byte-for-byte
datastore restore. The export is retained after success so operators can
inspect it or recover individual objects and local volume data.

See [NODEMIGRATION_GOAL.md](NODEMIGRATION_GOAL.md) for the full scope and the
task-specific validation rules. [NODEMIGRATE_STATUS.md](NODEMIGRATE_STATUS.md)
is the dashboard for separate living migration, CI, and release status records.
