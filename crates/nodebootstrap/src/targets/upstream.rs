//! Installs real upstream `kube-apiserver` + `kube-controller-manager` +
//! `kube-scheduler` against `nodestore`, wired up with the PKI `pki.rs`
//! minted (not k3s's) -- `main`'s default target.
//!
//! Replaces `deploy/lib/upstream-kube-apiserver.sh` /
//! `upstream-kube-controller-manager.sh` / `upstream-kube-scheduler.sh` --
//! but where those scripts deliberately *borrow* k3s's already-generated
//! PKI (see `upstream-kube-apiserver.sh`'s header comment: nothing else
//! minted one), this target starts from `pki.rs`'s own output instead, so
//! there is no k3s in the loop at all.
//!
//! **Scope cut, deliberate:** fetches the three binaries and builds their
//! real flag sets (both real logic, unit-tested below). Does **not** yet
//! generate and start a supervised service for any of them -- same
//! `service-mgr.rs` gap `containerd.rs`/`cni.rs` already defer to. `run_with`
//! fetches the binaries, prints the flags it would run each with, and
//! bails rather than silently pretending the cluster is up.

use anyhow::{Context, Result};

use crate::config::Config;

/// Pinned to the same `v1.33` line the vendored CoreDNS manifest and this
/// workspace's `k8s-openapi` feature are pinned to (`vendor/README.md`).
/// Unlike `upstream-kube-apiserver.sh`, this is **not** derived from a
/// running k3s's own version at all -- there is no k3s to ask.
const K8S_VERSION: &str = "v1.33.13";

pub fn run_with(cfg: &Config) -> Result<()> {
    let arch = k8s_dl_arch(&cfg.arch()).with_context(|| format!("unsupported arch for upstream binaries: {}", cfg.arch()))?;
    let bin_dir = cfg.toolchain_dir().join("bin");
    std::fs::create_dir_all(&bin_dir).context("creating toolchain bin dir")?;

    for bin in ["kube-apiserver", "kube-controller-manager", "kube-scheduler"] {
        fetch_binary(bin, arch, &bin_dir)?;
    }

    let advertise_address = detect_advertise_address();
    let spec = TargetSpec {
        pki_dir: cfg.pki_dir(),
        kubeconfig_dir: cfg.kubeconfig_dir(),
        etcd_pki_dir: nodestore_client_pki_dir(),
        etcd_servers: nodestore_etcd_servers(),
        advertise_address,
        service_cidr: "10.43.0.0/16".to_string(),
        service_account_issuer: "https://kubernetes.default.svc.cluster.local".to_string(),
    };
    let apiserver_args = apiserver_args(&spec);
    let cm_args = controller_manager_args(&spec);
    let sched_args = scheduler_args(&spec);

    anyhow::bail!(
        "nodebootstrap::targets::upstream fetched the binaries into {} but does not yet start \
         them as supervised services (service-mgr.rs gap -- see this module's doc comment). \
         Flags that would be used:\nkube-apiserver {}\nkube-controller-manager {}\n\
         kube-scheduler {}",
        bin_dir.display(),
        apiserver_args.join(" "),
        cm_args.join(" "),
        sched_args.join(" "),
    )
}

fn k8s_dl_arch(arch: &str) -> Option<&'static str> {
    Some(match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "armv7l" => "arm",
        "ppc64le" => "ppc64le",
        "s390x" => "s390x",
        _ => return None,
    })
}

/// `dl.k8s.io` publishes each binary at a fixed, predictable path per
/// version/arch -- no release-asset matching needed, unlike
/// `fetch.rs`'s not-yet-ported `Source::Release` path for this project's
/// own components.
fn fetch_binary(name: &str, arch: &str, bin_dir: &std::path::Path) -> Result<()> {
    let dest = bin_dir.join(name);
    if dest.exists() {
        tracing::info!(name, "already fetched");
        return Ok(());
    }
    let url = format!("https://dl.k8s.io/release/{K8S_VERSION}/bin/linux/{arch}/{name}");
    tracing::info!(name, url, "fetching upstream binary");
    crate::pkg::fetch_url(&url, &dest).with_context(|| format!("fetching {name}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("making {name} executable"))?;
    }
    Ok(())
}

fn nodestore_client_pki_dir() -> std::path::PathBuf {
    let data_dir = std::env::var("NODESTORE_DATA_DIR").unwrap_or_else(|_| "/var/lib/nodestore".to_string());
    std::path::PathBuf::from(data_dir).join("pki/client")
}

fn nodestore_etcd_servers() -> String {
    std::env::var("NODEBOOTSTRAP_ETCD_SERVERS").unwrap_or_else(|_| "https://127.0.0.1:2379".to_string())
}

/// Same reasoning as `upstream-kube-apiserver.sh`'s `detect_advertise_address`:
/// the CNI bridge's gateway address is what's reachable from inside a pod's
/// network namespace, which loopback and the auto-detected external
/// address both are not (see that script's comment for the two ways this
/// was confirmed live). Falls back to loopback when no `cni0` exists yet
/// (e.g. `--cni=none`).
fn detect_advertise_address() -> String {
    std::process::Command::new("ip")
        .args(["-4", "-o", "addr", "show", "cni0"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .nth(3)
                .and_then(|cidr| cidr.split('/').next())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

struct TargetSpec {
    pki_dir: std::path::PathBuf,
    /// Where `kubeconfig::write_all` wrote the static kubeconfigs -- a
    /// separate dir from `pki_dir` (see `Config::kubeconfig_dir`), so
    /// `kube-controller-manager`/`kube-scheduler`'s `--kubeconfig` flags
    /// must be built from this, not `pki_dir`.
    kubeconfig_dir: std::path::PathBuf,
    etcd_pki_dir: std::path::PathBuf,
    etcd_servers: String,
    advertise_address: String,
    service_cidr: String,
    service_account_issuer: String,
}

fn apiserver_args(spec: &TargetSpec) -> Vec<String> {
    let pki = |name: &str| spec.pki_dir.join(name).display().to_string();
    let etcd = |name: &str| spec.etcd_pki_dir.join(name).display().to_string();
    vec![
        format!("--etcd-servers={}", spec.etcd_servers),
        format!("--etcd-cafile={}", etcd("ca.crt")),
        format!("--etcd-certfile={}", etcd("client.crt")),
        format!("--etcd-keyfile={}", etcd("client.key")),
        "--secure-port=6443".to_string(),
        "--bind-address=0.0.0.0".to_string(),
        format!("--advertise-address={}", spec.advertise_address),
        format!("--tls-cert-file={}", pki("apiserver.crt")),
        format!("--tls-private-key-file={}", pki("apiserver.key")),
        // The CA that issued every client cert (admin/kube-controller-
        // manager/kube-scheduler) IS the client CA here -- unlike
        // upstream-kube-apiserver.sh's k3s-borrowed setup, pki.rs issues
        // every client cert off one CA, so there's no two-CA bundle to
        // build (see that script's comment on why it needed one).
        format!("--client-ca-file={}", pki("ca.crt")),
        format!("--service-account-key-file={}", pki("sa.pub")),
        format!("--service-account-signing-key-file={}", pki("sa.key")),
        format!("--service-account-issuer={}", spec.service_account_issuer),
        format!("--service-cluster-ip-range={}", spec.service_cidr),
        "--authorization-mode=Node,RBAC".to_string(),
        "--enable-admission-plugins=NodeRestriction".to_string(),
        "--allow-privileged=true".to_string(),
        "--anonymous-auth=false".to_string(),
        "--v=1".to_string(),
    ]
}

fn controller_manager_args(spec: &TargetSpec) -> Vec<String> {
    let pki = |name: &str| spec.pki_dir.join(name).display().to_string();
    vec![
        format!("--kubeconfig={}", spec.kubeconfig_dir.join("kube-controller-manager.kubeconfig").display()),
        format!("--service-account-private-key-file={}", pki("sa.key")),
        format!("--cluster-signing-cert-file={}", pki("ca.crt")),
        format!("--cluster-signing-key-file={}", pki("ca.key")),
        format!("--root-ca-file={}", pki("ca.crt")),
        format!("--service-cluster-ip-range={}", spec.service_cidr),
        "--use-service-account-credentials=true".to_string(),
        "--allocate-node-cidrs=true".to_string(),
        "--cluster-cidr=10.42.0.0/16".to_string(),
        "--v=1".to_string(),
    ]
}

fn scheduler_args(spec: &TargetSpec) -> Vec<String> {
    vec![format!("--kubeconfig={}", spec.kubeconfig_dir.join("kube-scheduler.kubeconfig").display()), "--v=1".to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_spec() -> TargetSpec {
        TargetSpec {
            pki_dir: "/var/lib/nodebootstrap/pki".into(),
            kubeconfig_dir: "/etc/nodebootstrap".into(),
            etcd_pki_dir: "/var/lib/nodestore/pki/client".into(),
            etcd_servers: "https://127.0.0.1:2379".to_string(),
            advertise_address: "10.42.0.1".to_string(),
            service_cidr: "10.43.0.0/16".to_string(),
            service_account_issuer: "https://kubernetes.default.svc.cluster.local".to_string(),
        }
    }

    #[test]
    fn apiserver_args_reference_a_single_client_ca_not_a_two_ca_bundle() {
        let args = apiserver_args(&test_spec());
        assert!(args.iter().any(|a| a == "--client-ca-file=/var/lib/nodebootstrap/pki/ca.crt"));
        assert!(args.iter().any(|a| a == "--authorization-mode=Node,RBAC"));
        assert!(!args.iter().any(|a| a.contains("bundle")));
    }

    #[test]
    fn controller_manager_and_scheduler_point_at_their_own_kubeconfigs() {
        let cm = controller_manager_args(&test_spec());
        assert!(cm.iter().any(|a| a == "--kubeconfig=/etc/nodebootstrap/kube-controller-manager.kubeconfig"));
        let sched = scheduler_args(&test_spec());
        assert!(sched.iter().any(|a| a == "--kubeconfig=/etc/nodebootstrap/kube-scheduler.kubeconfig"));
    }
}
