# From a bare hostPath demo Pod to a CSI-backed Deployment (2026-08-27)

Working notes from diagnosing why the `hostpath-demo` sample pod
(`/home/droid/something-useful/k8s-demo/manifest.yaml` on the test box, not
checked into this repo) showed `Evicted`, the fix applied, and the plan for
migrating it off a raw `hostPath` volume onto the reference
`kubernetes-csi/csi-driver-host-path` driver this project's own e2e suite
already relies on. Companion to `docs/E2E_FINDINGS.md` (bugs found by
running the real e2e suite) and `docs/GAP_CLOSURE.md` (feature-by-feature
parity) — this is a single end-to-end walkthrough of one real cluster
incident and its resolution, not a tracked checklist.

## 1. Why the pod was `Evicted`

```
$ kubectl describe pod hostpath-demo
Status:   Failed
Reason:   Evicted
Message:  The node was low on resource: DiskPressure.
```

Root cause, in order:

1. **Disk usage briefly crossed the eviction-hard threshold.** `nodelet`
   monitors the filesystem under `NODELET_DISK_PATH` (default
   `/var/lib/nodelet`) and sets the node's `DiskPressure` condition `True`
   once available space drops below `NODELET_DISK_PRESSURE_PERCENT`
   (default **10%** — see `crates/nodelet/src/config.rs`). On this box `df`
   showed **12G avail / 103G total ≈ 11.6%** at the time — close enough to
   the line that a transient dip (an image pull, a build) plausibly pushed
   it under 10% for a moment.
2. **nodelet's eviction manager reacted like real kubelet does**: under
   `DiskPressure`, BestEffort pods are evicted first. `hostpath-demo` had no
   resource requests/limits, so it was BestEffort. nodelet stopped its
   containers and set `status.reason=Evicted` /
   `status.message="The node was low on resource: DiskPressure."` — see the
   eviction loop in `crates/nodelet/src/app.rs`.
3. **The pod's own `disk-pressure` tolerations did not save it.** The
   manifest already carried
   ```yaml
   tolerations:
     - key: node.kubernetes.io/disk-pressure
       operator: Exists
       effect: NoSchedule
     - key: node.kubernetes.io/disk-pressure
       operator: Exists
       effect: NoExecute
   ```
   added for an earlier reboot-survival test. This is a common
   misunderstanding, not a nodelet bug: taint tolerations only govern
   *scheduling* and the separate taint-based `NoExecute` eviction path
   (driven by the node lifecycle controller tainting a node and evicting
   non-tolerating pods after `tolerationSeconds`). Real kubelet's own
   resource-pressure eviction manager — and nodelet's, matching it — evicts
   BestEffort pods under actual `DiskPressure` regardless of toleration.
   There is no toleration that opts a pod out of it; the only way to reduce
   eviction risk is to give the pod CPU/memory *and* `ephemeral-storage`
   requests so it is Burstable/Guaranteed QoS instead of BestEffort.
4. **It stayed `Evicted` forever because it was a bare `Pod`, not a
   Deployment/ReplicaSet.** `kubectl get pod hostpath-demo -o
   jsonpath='{.metadata.ownerReferences}'` was empty. Real kubelet's
   behavior (nodelet deliberately matches it) is to leave an evicted pod's
   object around for cleanup rather than deleting or reviving it — logged
   repeatedly as `"evicted pod (containers stopped; object left for
   cleanup, matching real kubelet)"` in `journalctl -u nodelet`. Nothing
   owned the pod, so nothing ever recreated it.

## 2. Fix applied: wrap it in a Deployment

`manifest.yaml` was changed from a bare `Pod` to a `Deployment` (1 replica)
wrapping the identical pod spec — same tolerations, same `nginx:alpine`
container, same `hostPath` volume for now (see §3 for replacing that part).
The full manifest as applied is [`hostpath-demo-deployment.yaml`](./hostpath-demo-deployment.yaml)
in this directory. The old dead `Pod` object was deleted and the
`Deployment` applied in its place:

```bash
kubectl delete pod hostpath-demo
kubectl apply -f hostpath-demo-deployment.yaml
```

Verified live afterward, not just applied and assumed correct:

```
$ kubectl get deploy,rs,pod -n default -l app=hostpath-demo -o wide
NAME                            READY   UP-TO-DATE   AVAILABLE   AGE
deployment.apps/hostpath-demo   1/1     1            1           5m14s

NAME                                             DESIRED   CURRENT   READY   AGE
replicaset.apps/hostpath-demo-f5964e2b19e4d762   1         1         1       5m13s

NAME                                       READY   STATUS    RESTARTS   AGE
pod/hostpath-demo-f5964e2b19e4d762-nml8m   1/1     Running   0          5m13s

$ curl -s http://127.0.0.1:8080/
<!doctype html>
<html><head><title>k8s survives reboot</title></head>
<body style="font-family:sans-serif">
<h1>Hello from hostPath volume</h1>
...
```

The pod picked up the tolerations, the `hostPath` mount, and the `hostPort`
binding correctly, and is actually serving the real file content from
`/home/droid/something-useful/k8s-demo/www/index.html` — confirming the
Pod→Deployment translation, not just that the object got created.

This does **not** stop the underlying eviction from happening again if disk
usage crosses the 10%-available line — the toleration limitation from §1
still applies. What it buys is self-healing: the ReplicaSet notices the pod
disappear and recreates it automatically, instead of leaving a dead `Pod`
object that requires a manual `kubectl delete && kubectl apply` every time.

## 3. Planned follow-up: move off raw `hostPath` onto the reference CSI driver

The demo's `hostPath` volume type is itself part of why a stray
`DiskPressure` blip can wreck it — `hostPath` bypasses any of Kubernetes'
volume lifecycle/capacity accounting entirely; the pod just reads a bind
mount. The plan is to back the same content
(`/home/droid/something-useful/k8s-demo/www`) with a PVC dynamically
provisioned by `kubernetes-csi/csi-driver-host-path`, the same reference CSI
driver this repo's own e2e suite exercises against nodelet — so the volume
path goes through nodelet's real CSI plugin-registration and
mount/stage/publish code instead of a bare bind mount.

### How this repo installs that driver (reused, not reinvented)

As of the `feat(nodebootstrap): retire non-performance shell paths` commit,
`deploy/lib/e2e-full-setup.sh` no longer lives on `main` — it was moved to
the `archive-shell-scripts-0.7.1` branch and is fetched and re-run as raw
bash by CI (`.github/workflows/e2e.yml` and `release.yml`, step "Install
reference CSI and DRA drivers"). There is no Rust-native installer
subcommand yet — `crates/nodebootstrap/src/e2e/*` only contains the *test
assertions* ported from the old shell suite; they assume the driver is
already installed and read its StorageClass/driver name from `TEST_CSI_*`
env vars.

To install it by hand against a live cluster, reproduce (or literally
re-run) that archived script:

```bash
git fetch --no-tags --depth=1 origin archive-shell-scripts-0.7.1
git show FETCH_HEAD:deploy/lib/e2e-full-setup.sh > /tmp/e2e-full-setup.sh
chmod +x /tmp/e2e-full-setup.sh
KUBECONFIG=<admin kubeconfig> NODELET_DATA_DIR=/var/lib/nodelet \
  sudo -E bash /tmp/e2e-full-setup.sh
```

It is written to be idempotent (`helm upgrade -i`, `kubectl apply`), so
re-running it against an already-set-up cluster is safe. What it does:

1. Installs `helm` v3.16.4 if missing.
2. Applies the `snapshot.storage.k8s.io` CRDs from
   `kubernetes-csi/external-snapshotter` `v8.6.0` (a prerequisite of
   upstream's own deploy script).
3. `git clone --depth 1 https://github.com/kubernetes-csi/csi-driver-host-path.git`
   (tip of the default branch — no pinned tag).
4. Applies three narrow `sed` patches to the cloned
   `deploy/kubernetes-latest/hostpath/csi-hostpath-plugin.yaml` and
   `deploy/kubernetes-latest/deploy.sh` (disables
   `KUBE_FEATURE_WatchListClient` for the v6.3 `csi-provisioner` sidecar
   against this project's apiserver; fixes a deprecated kustomize
   `commonLabels` key; renames a duplicate `healthz` port).
5. Runs upstream's own `deploy.sh` with
   `KUBELET_DATA_DIR="$NODELET_DATA_DIR"` so the driver's
   `node-driver-registrar` sidecar registers against **nodelet's** plugin
   directory instead of real kubelet's conventional
   `/var/lib/kubelet/plugins_registry` — nodelet deliberately does not use
   that path (see `crates/nodelet/src/config.rs` — `NODELET_PLUGIN_REGISTRY_PATH`
   defaults to `/var/lib/nodelet/plugins_registry`,
   `NODELET_PLUGIN_REGISTRY_SYNC_SECS` defaults to a 10s poll).
6. Applies three inline `StorageClass` manifests (not fetched from
   upstream), all `provisioner: hostpath.csi.k8s.io`:
   - `csi-hostpath-sc` — `volumeBindingMode: Immediate` (the one to use here)
   - `csi-hostpath-block-sc` — `Immediate`, `volumeMode: Block`
   - `csi-hostpath-sc-wait` — `WaitForFirstConsumer`
7. Waits for a readiness PVC to bind before returning.

### The "use the same folder" wrinkle

`csi-driver-host-path`'s reference implementation provisions each volume as
a fresh, empty directory under its own data root (inside
`$NODELET_DATA_DIR`, i.e. `/var/lib/nodelet/...` here) — it has no notion of
binding a new PVC to a specific *pre-existing* host directory the way a raw
`hostPath` volume does. There are two honest ways to reuse
`k8s-demo/www`'s content once the PVC is bound, and this doc does not yet
pick one — that's the open decision before actually cutting the migration
over:

- **Copy-in after first bind**: create the PVC, let the driver provision an
  empty volume, start the pod once so the volume gets staged/published on
  the node, then `cp` `k8s-demo/www/*` into the now-mounted path and restart
  the pod. Simple, but the content lives in two places (repo checkout +
  provisioned volume) and has to be re-copied if the PVC is ever recreated.
- **Bind the provisioned directory back via a symlink**: after first
  provisioning, find the driver's per-volume directory under
  `/var/lib/nodelet/...` and replace it with a symlink to
  `/home/droid/something-useful/k8s-demo/www`. Keeps a single source of
  truth, but depends on an implementation detail of the reference driver's
  on-disk layout that isn't a stable public contract.

Neither has been applied yet — the `Deployment` fix in §2 is what's live on
the test cluster today; this section is the plan for the follow-up PVC
migration, to be done as its own change once the copy-vs-symlink choice is
made deliberately rather than under an eviction diagnosis's time pressure.

## References

- `crates/nodelet/src/config.rs` — `disk_pressure_percent`, `disk_path`,
  `NODELET_PLUGIN_REGISTRY_PATH`, `NODELET_PLUGIN_REGISTRY_SYNC_SECS`,
  `NODELET_CSI_DRIVERS`
- `crates/nodelet/src/app.rs` — the eviction loop
- `crates/nodelet/src/plugin_registry.rs` — CSI/device-plugin/DRA
  registration protocol
- `crates/nodebootstrap/src/e2e/tests/storage.rs`, `csi.rs`,
  `scheduler.rs`, `generic_ephemeral_volume.rs` — the Rust e2e assertions
  that assume this driver is already installed
- `.github/workflows/e2e.yml`, `.github/workflows/release.yml` — where the
  archived install script is fetched and run in CI
- `deploy/lib/e2e-full-setup.sh` on branch `archive-shell-scripts-0.7.1` —
  the install script itself (not present on `main`)
