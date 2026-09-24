# nodemigrate

`nodemigrate` is a standalone operator binary. It is a workspace crate but is
not linked into the combined `notk8s` binary. Its independent release tag uses
the version of the latest regular project release and does not update `VERSION`.

## Current migration path

The implemented migration is a single-node K3s server using systemd or OpenRC
and K3s's bundled flannel VXLAN CNI to a single-node nodestore cluster:

```sh
sudo nodemigrate inspect
sudo nodemigrate to=nodestore from=k3s plan-only=true
sudo nodemigrate to=nodestore from=k3s
```

The inspect report identifies the service manager, K3s config files, cluster
CIDRs/domain/DNS, and whether the datastore is Kine or etcd. The migration reads
Kubernetes API objects from the live source API, then bootstraps nodestore using
the installed `notk8s` or `nodebootstrap` binary and restores those objects
through the destination API. Kine and etcd database files are never copied into
nodestore.

Schedule a maintenance window and stop clients from changing cluster objects
before starting the export. The source API remains available while the export
is read; nodemigrate cannot fence writes from other cluster clients.

The source K3s service is stopped and disabled after the protected object export
is written. It remains installed by default. The export contains Secrets and is
stored under `/var/lib/nodemigrate/exports` with restrictive directory and file
permissions; keep it until the destination is accepted. If bootstrap fails
before the destination API is available, nodemigrate restores the previous
K3s service state. If the destination API is running but import fails, K3s
remains stopped to avoid a port conflict; the export path is reported for
recovery.

`uninstall-after-migrate=true` is the only option that invokes
`/usr/local/bin/k3s-uninstall.sh`. Afterward, nodemigrate reruns nodebootstrap
to restore destination-owned host files that the K3s uninstall script may have
removed, then checks destination readiness again.

## Safety limits

The tool currently refuses to migrate clusters with PersistentVolumes or
PersistentVolumeClaims because it does not copy volume data. It also refuses
multi-node clusters, non-default or disabled K3s CNI, non-VXLAN flannel,
non-default cluster DNS, CIDR combinations nodebootstrap cannot represent, and
service managers other than systemd or OpenRC. The reverse nodestore-to-K3s
path and generic upstream Kubernetes source detection are not executable yet;
they are rejected before the source service changes.

This is an API object migration, not a K3s control-plane database restore. It
does not transfer API audit logs, etcd membership/snapshots, node-local files,
or objects created by K3s add-ons in the excluded `kube-system` namespace.
Owner references are removed because their source UIDs do not exist in the new
cluster; API controllers recreate their managed children, but custom ownership
relationships may need repair.
