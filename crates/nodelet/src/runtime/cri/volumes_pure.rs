use super::*;

/// A volume `resolve_volumes()` has resolved to something mountable —
/// either a host path nodelet materialized itself (every volume kind
/// before round 32: ConfigMap/Secret/emptyDir/downwardAPI/projected/PVC/
/// ephemeral, all bind-mounted from a real host directory), or an image
/// reference for `volumeSource.image` (round 32) — CRI's `Mount.image`
/// field handles those directly, with no host path involved at all
/// (`host_path` must stay empty per the proto's own contract).
#[derive(Clone, Debug)]
pub(crate) enum ResolvedVolume {
    HostPath(PathBuf),
    Image { image_ref: String },
}

/// `volumes[].hostPath.type`'s validate-and-maybe-create semantics
/// (round 65; found in a fresh gap re-audit) — matches real kubelet's own
/// hostPath type checking: an unset/empty type performs no check at all
/// (the pre-1.8 legacy behavior, still the default — the path is used
/// exactly as given, whatever it turns out to be); `DirectoryOrCreate`/
/// `FileOrCreate` create an empty directory (mode `0755`) / file (mode
/// `0644`) only if nothing exists there yet, performing no check if
/// something already does (matching upstream: existing content is never
/// second-guessed); every other named type requires something of that
/// exact kind to already exist, erroring otherwise rather than silently
/// mounting the wrong thing. `Err` means the volume must not be used —
/// the caller logs it and skips the volume rather than mounting it.
pub(crate) fn validate_host_path(path: &std::path::Path, type_: Option<&str>) -> Result<(), String> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let existing = std::fs::symlink_metadata(path).ok().map(|m| m.file_type());
    let missing = |kind: &str| format!("hostPath: {} does not exist (type: {kind} requires it to already)", path.display());
    let wrong_kind = |kind: &str| format!("hostPath: {} exists but is not {kind}", path.display());
    match type_.unwrap_or("") {
        "" => Ok(()),
        "DirectoryOrCreate" => match existing {
            Some(ft) if ft.is_dir() => Ok(()),
            Some(_) => Err(wrong_kind("a directory")),
            None => {
                std::fs::create_dir_all(path).map_err(|e| format!("hostPath: failed to create directory {}: {e}", path.display()))?;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
                Ok(())
            }
        },
        "Directory" => match existing {
            Some(ft) if ft.is_dir() => Ok(()),
            Some(_) => Err(wrong_kind("a directory")),
            None => Err(missing("Directory")),
        },
        "FileOrCreate" => match existing {
            Some(ft) if ft.is_file() => Ok(()),
            Some(_) => Err(wrong_kind("a regular file")),
            None => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| format!("hostPath: failed to create parent directory for {}: {e}", path.display()))?;
                }
                std::fs::File::create(path).map_err(|e| format!("hostPath: failed to create file {}: {e}", path.display()))?;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
                Ok(())
            }
        },
        "File" => match existing {
            Some(ft) if ft.is_file() => Ok(()),
            Some(_) => Err(wrong_kind("a regular file")),
            None => Err(missing("File")),
        },
        "Socket" => match existing {
            Some(ft) if ft.is_socket() => Ok(()),
            Some(_) => Err(wrong_kind("a socket")),
            None => Err(missing("Socket")),
        },
        "CharDevice" => match existing {
            Some(ft) if ft.is_char_device() => Ok(()),
            Some(_) => Err(wrong_kind("a character device")),
            None => Err(missing("CharDevice")),
        },
        "BlockDevice" => match existing {
            Some(ft) if ft.is_block_device() => Ok(()),
            Some(_) => Err(wrong_kind("a block device")),
            None => Err(missing("BlockDevice")),
        },
        other => Err(format!("hostPath: unrecognized type '{other}'")),
    }
}


/// Build CRI `Mount` entries for a container's volumeMounts against the
/// pod's already-resolved volume name -> mount source map (see
/// resolve_volumes()). A mount naming a volume that isn't in the map
/// (unsupported volume type, or the ConfigMap/Secret fetch failed) is
/// silently dropped — pulled out as a pure function specifically to make
/// that behavior, and subPath/readOnly handling, unit-testable without a
/// real CRI socket.
pub(crate) fn build_mounts(
    volume_mounts: &[k8s_openapi::api::core::v1::VolumeMount],
    volumes: &HashMap<String, ResolvedVolume>,
) -> Vec<Mount> {
    volume_mounts
        .iter()
        .filter_map(|vm| match volumes.get(&vm.name)? {
            ResolvedVolume::HostPath(host_dir) => {
                let host_path = match &vm.sub_path {
                    Some(sub) => host_dir.join(sub),
                    None => host_dir.clone(),
                };
                Some(Mount {
                    container_path: vm.mount_path.clone(),
                    host_path: host_path.to_string_lossy().into_owned(),
                    readonly: vm.read_only.unwrap_or(false),
                    ..Default::default()
                })
            }
            ResolvedVolume::Image { image_ref } => Some(Mount {
                container_path: vm.mount_path.clone(),
                // Must stay empty — CRI's Mount.image and Mount.host_path
                // are mutually exclusive by the proto's own contract.
                host_path: String::new(),
                readonly: true, // image volumes are always read-only, matching the KEP
                image: Some(ImageSpec { image: image_ref.clone(), ..Default::default() }),
                // The container's own volumeMounts[].subPath, same field
                // regular volumes already use to select a subdirectory —
                // for an image-backed volume it selects a path *within*
                // the mounted image instead (CRI's `image_sub_path`).
                image_sub_path: vm.sub_path.clone().unwrap_or_default(),
                ..Default::default()
            }),
        })
        .collect()
}


/// Parse a Kubernetes `Quantity` suffix (`Ki`/`Mi`/`Gi`/`Ti` binary, `k`/`M`/`G`/`T`
/// decimal, or bare). Uses f64 — imprecise at the very top of i64 range, which
/// doesn't matter for cpu/memory quantities on any real machine.
pub(crate) fn parse_quantity(s: &str) -> Option<f64> {
    const BINARY: [(&str, f64); 4] =
        [("Ki", 1024.0), ("Mi", 1024.0 * 1024.0), ("Gi", 1024.0 * 1024.0 * 1024.0), ("Ti", 1024.0 * 1024.0 * 1024.0 * 1024.0)];
    const DECIMAL: [(&str, f64); 4] = [("k", 1e3), ("M", 1e6), ("G", 1e9), ("T", 1e12)];
    let s = s.trim();
    for (suf, mult) in BINARY.into_iter().chain(DECIMAL) {
        if let Some(num) = s.strip_suffix(suf) {
            return num.parse::<f64>().ok().map(|n| n * mult);
        }
    }
    s.parse::<f64>().ok()
}


/// Whether a CSI driver requires an attach before Stage/Publish — pure
/// wrapper around `CSIDriver.spec.attachRequired` so the "assume yes
/// if the field/object is missing" default (matching upstream) is
/// unit-testable without a cluster. `None` means no `CSIDriver` object
/// exists for this driver name at all.
pub(crate) fn attach_required(driver: Option<&CSIDriver>) -> bool {
    driver.and_then(|d| d.spec.attach_required) != Some(false)
}


/// Find the `VolumeAttachment` (if any) describing `driver` attaching
/// `pv_name` to `node_name` — a pure search so the matching logic is
/// unit-testable without listing real cluster objects. `VolumeAttachment`
/// names are generated (hashed) by the attach/detach controller, not
/// derivable from `(driver, node, pv)`, so this has to scan rather than
/// `.get()` by name.
pub(crate) fn find_volume_attachment<'a>(
    attachments: &'a [VolumeAttachment],
    driver: &str,
    node_name: &str,
    pv_name: &str,
) -> Option<&'a VolumeAttachment> {
    attachments.iter().find(|a| {
        a.spec.attacher == driver
            && a.spec.node_name == node_name
            && a.spec.source.persistent_volume_name.as_deref() == Some(pv_name)
    })
}


/// Extract the `publish_context` for Stage/Publish from an attached
/// `VolumeAttachment` — `None` if it isn't attached yet
/// (`status.attached == false`, or no `status` at all: the
/// external-attacher hasn't finished `ControllerPublishVolume` yet).
pub(crate) fn attachment_publish_context(attachment: &VolumeAttachment) -> Option<HashMap<String, String>> {
    let status = attachment.status.as_ref()?;
    if !status.attached {
        return None;
    }
    Some(status.attachment_metadata.clone().unwrap_or_default().into_iter().collect())
}


/// The deterministic name a generic ephemeral volume's (`spec.volumes[].ephemeral`)
/// auto-created `PersistentVolumeClaim` gets — `<pod name>-<volume name>`,
/// exactly as documented on `EphemeralVolumeSource` itself (round 31).
/// Pure so the naming convention is unit-testable without a cluster.
pub(crate) fn ephemeral_pvc_name(pod_name: &str, volume_name: &str) -> String {
    format!("{pod_name}-{volume_name}")
}


/// Synthetic `volume_id` for a CSI *ephemeral inline* volume (round 46) —
/// there's no PV/PVC to derive a real one from, so nodelet mints its own,
/// scoped by pod UID (stable across reconciles, unique across pod
/// recreations even under the same name) and volume name (unique within
/// one pod).
pub(crate) fn csi_ephemeral_volume_handle(pod_uid: &str, volume_name: &str) -> String {
    format!("{pod_uid}-{volume_name}")
}


/// Whether `pvc` is genuinely owned by the pod with uid `pod_uid` — the
/// safety check real kubelet itself does before trusting a same-named
/// PVC for a generic ephemeral volume (round 31; see
/// `EphemeralVolumeSource`'s own doc comment: "An existing PVC with that
/// name that is not owned by the pod will *not* be used ... to avoid
/// using an unrelated volume by mistake"). Checked by UID, not just
/// name/kind — a stale or coincidentally-named PVC must never be
/// silently adopted.
pub(crate) fn pvc_owned_by_pod(pvc: &PersistentVolumeClaim, pod_uid: &str) -> bool {
    pvc.metadata.owner_references.as_deref().unwrap_or(&[]).iter().any(|o| o.uid == pod_uid)
}


/// Whether an `emptyDir` volume wants `medium: Memory` (tmpfs-backed,
/// round 30) rather than the default (unset, or explicitly `""`) —
/// regular disk. Pure so the decision is unit-testable without touching
/// the filesystem.
pub(crate) fn is_memory_medium_empty_dir(source: &k8s_openapi::api::core::v1::EmptyDirVolumeSource) -> bool {
    source.medium.as_deref() == Some("Memory")
}


/// Build `mount -t tmpfs [-o size=<bytes>] tmpfs <path>`'s arguments —
/// pure so the command construction is unit-testable without actually
/// mounting anything. No `sizeLimit` set means no `-o size=`, matching
/// tmpfs's own kernel default (half of physical RAM) rather than nodelet
/// inventing a cap upstream doesn't itself impose in that case.
pub(crate) fn tmpfs_mount_args(path: &std::path::Path, size_limit_bytes: Option<i64>) -> Vec<String> {
    let mut args = vec!["-t".to_string(), "tmpfs".to_string()];
    if let Some(bytes) = size_limit_bytes.filter(|b| *b > 0) {
        args.push("-o".to_string());
        args.push(format!("size={bytes}"));
    }
    args.push("tmpfs".to_string());
    args.push(path.to_string_lossy().into_owned());
    args
}


/// Mount a `Memory`-medium `emptyDir` volume's directory as tmpfs — the
/// same approach real kubelet itself uses (kubelet mounts tmpfs directly
/// on the host path it hands the container runtime as a bind-mount
/// source; this isn't a CRI-level concept, CRI's `Mount` struct only
/// binds an *existing* host path, it doesn't control the filesystem type
/// backing it). Shells out to `mount(8)` — same "use the host's own
/// tools rather than raw syscalls" approach `svc.rs` already takes for
/// `nft`. Best-effort: a failure here is logged and the (already-created,
/// plain-disk) directory is used as a fallback rather than failing the
/// whole pod — the same graceful-degradation posture used everywhere
/// else a host-level operation might not be available (e.g. no root, no
/// tmpfs support at all).
pub(crate) fn mount_tmpfs_empty_dir(dir: &std::path::Path, size_limit_bytes: Option<i64>) -> Result<()> {
    let status = std::process::Command::new("mount")
        .args(tmpfs_mount_args(dir, size_limit_bytes))
        .status()
        .context("running mount(8)")?;
    if !status.success() {
        anyhow::bail!("mount -t tmpfs exited with {status}");
    }
    Ok(())
}


/// Whether an `emptyDir` volume wants a `HugePages`/`HugePages-<size>`
/// medium (round 61; the last of round 58's 3 HugePages pieces) —
/// `HugePages` alone means "the kernel's default huge page size", while
/// `HugePages-<size>` (e.g. `HugePages-2Mi`) pins a specific size, mirroring
/// `resources.limits["hugepages-<size>"]`'s own naming (round 59/60). Pure,
/// same reasoning as `is_memory_medium_empty_dir()`.
pub(crate) fn is_hugepages_medium_empty_dir(source: &k8s_openapi::api::core::v1::EmptyDirVolumeSource) -> bool {
    source.medium.as_deref().is_some_and(|m| m == "HugePages" || m.starts_with("HugePages-"))
}


/// `HugePages-<size>`'s k8s binary-unit suffix (`Mi`/`Gi`/`Ki`) -> the unit
/// spelling `hugetlbfs`'s own `pagesize=` mount option expects. Linux's
/// kernel option parser (`memparse()`) reads a bare `K`/`M`/`G` suffix as
/// base-1024 already — exactly the same naming-convention-only
/// translation as round 59's `hugepage_cri_page_size()` (strip the
/// trailing `i`, no numeric rescaling of the value itself).
pub(crate) fn hugepages_medium_pagesize_option(medium: &str) -> Option<String> {
    medium.strip_prefix("HugePages-")?.strip_suffix('i').map(|s| format!("pagesize={s}"))
}


/// Build `mount -t hugetlbfs [-o pagesize=<unit>[,size=<bytes>]] none
/// <path>`'s arguments — pure, mirroring `tmpfs_mount_args()`. `medium ==
/// "HugePages"` (no specific size) omits `pagesize=` entirely, letting the
/// kernel use its own default huge page size, same "don't invent a cap
/// upstream doesn't impose" reasoning as tmpfs's unset `sizeLimit`.
pub(crate) fn hugetlbfs_mount_args(path: &std::path::Path, medium: &str, size_limit_bytes: Option<i64>) -> Vec<String> {
    let mut args = vec!["-t".to_string(), "hugetlbfs".to_string()];
    let mut opts: Vec<String> = hugepages_medium_pagesize_option(medium).into_iter().collect();
    if let Some(bytes) = size_limit_bytes.filter(|b| *b > 0) {
        opts.push(format!("size={bytes}"));
    }
    if !opts.is_empty() {
        args.push("-o".to_string());
        args.push(opts.join(","));
    }
    args.push("none".to_string());
    args.push(path.to_string_lossy().into_owned());
    args
}


/// Mount a `HugePages`/`HugePages-<size>`-medium `emptyDir` volume's
/// directory as `hugetlbfs` — same host-mount approach as
/// `mount_tmpfs_empty_dir()` and for the same reason: this isn't a CRI-level
/// concept, CRI's `Mount` only binds an *existing* host path. Best-effort:
/// logged and falls back to the already-created plain-disk directory on
/// failure (e.g. no hugepages reserved on this node, or the hugetlbfs
/// filesystem isn't available), same graceful-degradation posture as tmpfs.
pub(crate) fn mount_hugetlbfs_empty_dir(dir: &std::path::Path, medium: &str, size_limit_bytes: Option<i64>) -> Result<()> {
    let status = std::process::Command::new("mount")
        .args(hugetlbfs_mount_args(dir, medium, size_limit_bytes))
        .status()
        .context("running mount(8)")?;
    if !status.success() {
        anyhow::bail!("mount -t hugetlbfs exited with {status}");
    }
    Ok(())
}


/// Unmount every `Memory`- or `HugePages`/`HugePages-<size>`-medium
/// `emptyDir` this pod declared — called on pod teardown (`remove_pod()`)
/// since both are real reserved memory that must be given back, unlike a
/// plain-disk `emptyDir` directory (left in place today regardless of
/// medium — a pre-existing simplification, see `docs/GAP_CLOSURE.md`).
/// Re-derives volume names/paths from the Pod object rather than tracking
/// mount state separately, the same approach `unmount_csi_volumes()`
/// already takes. Best-effort per volume — one already-gone mount (e.g. the
/// pod directory was already cleaned up some other way) must not stop the
/// rest of teardown.
pub(crate) fn unmount_special_medium_empty_dirs(pod: &Pod, id: &PodId) {
    let Some(volumes) = pod.spec.as_ref().and_then(|s| s.volumes.as_ref()) else { return };
    let pod_dir = PathBuf::from(VOLUME_ROOT).join(&id.uid).join("volumes");
    for v in volumes {
        let Some(source) = &v.empty_dir else { continue };
        if !is_memory_medium_empty_dir(source) && !is_hugepages_medium_empty_dir(source) {
            continue;
        }
        let vol_dir = pod_dir.join(&v.name);
        if let Err(e) = std::process::Command::new("umount").arg(&vol_dir).status() {
            warn!(volume = %v.name, path = %vol_dir.display(), error = ?e, "failed to run umount for a Memory/HugePages-medium emptyDir volume");
        }
    }
}


/// Write a ConfigMap/Secret's keys out as individual files under `dir`
/// (creating it if needed) — text values from `.data`/`.stringData`, binary
/// values from `.binaryData`/`.data` (Secret's `.data` is base64 in the wire
/// format but k8s_openapi's `ByteString` decodes it automatically, so by the
/// time it gets here it's already raw bytes).
pub(crate) fn write_volume_dir(
    dir: &std::path::Path,
    text: Option<std::collections::BTreeMap<String, String>>,
    binary: Option<std::collections::BTreeMap<String, Vec<u8>>>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (k, v) in text.into_iter().flatten() {
        std::fs::write(dir.join(k), v)?;
    }
    for (k, v) in binary.into_iter().flatten() {
        std::fs::write(dir.join(k), v)?;
    }
    Ok(())
}


/// Write a downwardAPI volume's files (`fieldRef` only — `resourceFieldRef`,
/// e.g. a container's actual assigned CPU/memory limit, isn't supported: it
/// needs the resolved container spec, not just the Pod object). An item's
/// `path` may contain subdirectories, which is valid Kubernetes downwardAPI
/// syntax.
pub(crate) fn write_downward_api_volume(
    dir: &std::path::Path,
    pod: &Pod,
    items: &[k8s_openapi::api::core::v1::DownwardAPIVolumeFile],
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for item in items {
        let Some(field_ref) = &item.field_ref else { continue };
        let Some(value) = pod_field_value(pod, &field_ref.field_path) else { continue };
        let target = dir.join(&item.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, value)?;
    }
    Ok(())
}


/// Write one projected-volume source's contribution into `dir`. Mirrors
/// `write_volume_dir()`'s "every key becomes a file" default, but a
/// projected source additionally supports `items` (`KeyToPath`): select
/// specific keys and rename them to a specific path within the volume —
/// real Kubernetes semantics that plain top-level configMap/secret volumes
/// also have but nodelet doesn't apply there yet (see docs/GAP_CLOSURE.md).
pub(crate) fn write_projected_keys(
    dir: &std::path::Path,
    text: Option<std::collections::BTreeMap<String, String>>,
    binary: Option<std::collections::BTreeMap<String, Vec<u8>>>,
    items: Option<&[k8s_openapi::api::core::v1::KeyToPath]>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let write_one = |path: &str, bytes: &[u8]| -> std::io::Result<()> {
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, bytes)
    };
    match items {
        Some(items) => {
            for item in items {
                if let Some(v) = text.as_ref().and_then(|m| m.get(&item.key)) {
                    write_one(&item.path, v.as_bytes())?;
                } else if let Some(v) = binary.as_ref().and_then(|m| m.get(&item.key)) {
                    write_one(&item.path, v)?;
                }
            }
        }
        None => {
            for (k, v) in text.into_iter().flatten() {
                write_one(&k, v.as_bytes())?;
            }
            for (k, v) in binary.into_iter().flatten() {
                write_one(&k, &v)?;
            }
        }
    }
    Ok(())
}


/// Build a pod's `/etc/hosts` contents from `hostAliases` — kubelet's own
/// approach: it doesn't tell the container runtime about extra hosts
/// entries (CRI has no such field), it generates the file itself and bind
/// mounts it over `/etc/hosts`.
pub(crate) fn write_etc_hosts(path: &std::path::Path, aliases: &[k8s_openapi::api::core::v1::HostAlias]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut content = String::from("127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n");
    for alias in aliases {
        let hostnames = alias.hostnames.clone().unwrap_or_default().join(" ");
        if !hostnames.is_empty() {
            content.push_str(&format!("{}\t{hostnames}\n", alias.ip));
        }
    }
    std::fs::write(path, content)
}


/// Set the group ownership of `path` without touching its user owner
/// (`(uid_t)-1` is POSIX for "leave unchanged").
pub(crate) fn chown_gid(path: &std::path::Path, gid: u32) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let rc = unsafe { libc::chown(c_path.as_ptr(), u32::MAX as libc::uid_t, gid as libc::gid_t) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}


/// Set the setgid bit on a directory so files later written into it by the
/// container process inherit `fsGroup` too, matching real kubelet's
/// volume-ownership behavior.
pub(crate) fn set_setgid(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(dir)?;
    let mut perm = meta.permissions();
    perm.set_mode(perm.mode() | 0o2000);
    std::fs::set_permissions(dir, perm)
}


/// Recursively chown a materialized volume directory to `fsGroup`. Only
/// touches directories nodelet itself created (ConfigMap/Secret/emptyDir/
/// downwardAPI/projected materializations) — there's no real PV/hostPath
/// support yet for this to reach beyond that (see docs/GAP_CLOSURE.md).
pub(crate) fn apply_fs_group(dir: &std::path::Path, gid: u32) -> std::io::Result<()> {
    chown_gid(dir, gid)?;
    set_setgid(dir)?;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            apply_fs_group(&path, gid)?;
        } else {
            chown_gid(&path, gid)?;
        }
    }
    Ok(())
}


