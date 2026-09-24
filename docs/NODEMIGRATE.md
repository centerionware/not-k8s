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
the source service and waits for the new node registration to become Ready.
If no destination Node exists, nodemigrate carries labels, taints, and the
unschedulable setting from the source Node to the new registration. When a
destination Node is replaced, those scheduling fields from that destination
Node are restored after the new registration becomes Ready.
For control-plane migration in either direction, the same flag is required
when the destination cluster already contains the local node name. If a
reverse control-plane migration cannot query the retained API before starting
it, set the flag to force a fresh registration check. Nodemigrate removes a
stale Node before checking readiness and restores its labels, taints, and
unschedulable setting after registration. When migrating into a new cluster,
it carries those scheduling fields from the source Node. A staged reverse
control-plane operation can return before the destination API is ready; the
multi-node coordinator must verify fresh Node registration after quorum
returns.
`NODEMIGRATE_NODE_NAME` overrides the detected node name. Cluster-wide API
objects are transferred at the control-plane stage, not re-imported from each
worker. For an ordered multi-control-plane migration, migrate the first
control-plane without `skip-api-import`; after its API state is imported and
the destination is ready, migrate each later control-plane with
`skip-api-import=true` while setting the normal nodestore join environment.
Each later control plane still writes a protected per-node export and snapshots
local host paths, but does not re-apply cluster-wide objects. If the source
API has lost quorum, copy the first control plane's protected export to each
later node using a private directory, then pass
`source-export=/path/to/private/export` with `skip-api-import=true`. Export
format v2 preserves all source Nodes' scheduling metadata, and each node keeps
its local storage and CNI recovery files in node-specific subdirectories. The
source export is required to be from a forward migration, and its presence
requires skipping a repeated cluster-wide import. Workers also accept these
options when joining the existing cluster; they never import cluster-wide
objects.

For an upstream three-control-plane return, stage the first retained
control-plane with `stage-target=true`. It exports nodestore API state, stops
its local nodestore services, starts the retained upstream control plane, and
returns without waiting for upstream API quorum. Migrate the second
control-plane normally; its source export remains available while two
nodestore members are active, and starting its retained etcd member should
restore upstream quorum so the export can be applied. Return the last
control-plane with `skip-api-export=true` only after that import succeeds.
This final operation requires `NODEMIGRATE_DESTINATION_KUBECONFIG` to reach
the ready upstream API and takes its local-volume recovery snapshot from the
destination PV inventory, since the source API may no longer have quorum.
Review the plan output before cutover. This staged operator protocol has not
yet been validated in the multi-node runtime lane.

On the return path, a detected not-k8s worker can start its retained
K3s-agent or kubelet service against the existing target cluster and wait for
a fresh Ready registration without importing cluster-wide objects again. Use
`NODEMIGRATE_DESTINATION_KUBECONFIG` for a cluster-admin kubeconfig that can
read PVs and replace an existing Node. The utility derives service and pod
CIDRs, cluster domain, DNS addresses, and node name from the source
installation where available.

The supported directions are K3s or upstream Kubernetes to nodestore, and
nodestore to a retained local K3s or upstream Kubernetes installation. The
return path starts the retained target service and imports the nodestore API
state. The original source stays installed for rollback by default, which
provides the retained local target for a return migration. `KUBECONFIG` may
select an alternate source config, `NODEMIGRATE_SOURCE_KUBECONFIG` explicitly
overrides it, and `NODEMIGRATE_DESTINATION_KUBECONFIG` overrides the target
config.

Before an interactive migration, `nodemigrate` prints a high-risk warning that
calls out the probability of data loss and asks for external backups. Continue
only by typing the exact lowercase response `yes`; any other response cancels
the operation. Noninteractive migration commands print the same warning to
their logs and continue without a prompt. Inspection, help, and plan-only
commands do not start a migration and do not request confirmation.

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
For K3s with Flannel disabled, nodemigrate reads the active containerd CNI
config and binary directories, passes those paths to nodebootstrap, and updates
containerd to keep using them. If an explicit K3s uninstall would remove either
directory from K3s's data tree, nodemigrate saves its contents to the protected
recovery export and restores them at the configured paths before reconciling
the not-k8s services. This protection has focused fixtures; the real K3s
uninstall and Cilium networking path still require the authorized integration
run.

PersistentVolume and PersistentVolumeClaim objects are migrated during the
control-plane stage. For local and hostPath PersistentVolumes, nodemigrate
reads the local Kubernetes Node labels and snapshots only host paths whose
required PV node affinity matches that node. If neither API exposes the local
Node, it logs a warning and backs up all safe PV paths found on the host so
unknown affinity cannot omit local data. A worker replacement also reads the
target PV list and retains its local hostPath/local-volume snapshot without
re-importing API objects; source uninstall restores that snapshot afterward.
The runtime
preservation and rollback behavior still needs a live multi-node check. If
`uninstall-after-migrate=true` invokes the K3s uninstall script, the host path
is restored afterward. Network and CSI backed volume payloads stay with their
storage provider.

The source services are stopped and disabled after the protected export is
written. On an upstream control-plane source, kubelet is stopped and its
file-managed static-pod sandboxes are removed through CRI so they release API
server and etcd ports; their manifests remain installed for a return
migration. `NODEMIGRATE_CRI_ENDPOINT` overrides the detected CRI endpoint
(`crictl` is required for this step). The source installation remains
available for recovery by default.
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
