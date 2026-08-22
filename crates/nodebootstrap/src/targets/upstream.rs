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
//! Fetches the three binaries, builds their real flag sets, and starts each
//! as a `service_mgr.rs`-supervised service (systemd -> OpenRC -> fallback
//! loop, same as `containerd.rs`). `kube-controller-manager` and
//! `kube-scheduler` are ordered `After=`/`depend()` on `kube-apiserver`
//! (matching real Kubernetes' own startup order expectation: both dial the
//! apiserver on their own retry loop, so this is a readiness nicety, not a
//! hard requirement, same as upstream's own docs describe).

use anyhow::{Context, Result};

use crate::config::Config;
use crate::service_mgr::{self, SupervisedService};

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
    let apiserver_exec = format!("{} {}", bin_dir.join("kube-apiserver").display(), apiserver_args(&spec).join(" "));
    service_mgr::install(
        cfg,
        &SupervisedService {
            name: "kube-apiserver",
            description: "Real upstream kube-apiserver (not-k8s, no k3s)",
            exec_cmd: &apiserver_exec,
            after: Some("nodestore.service"),
            env: &[],
        },
    )
    .context("installing kube-apiserver as a supervised service")?;

    wait_for_readyz(&spec);

    let cm_exec = format!(
        "{} {}",
        bin_dir.join("kube-controller-manager").display(),
        controller_manager_args(&spec).join(" ")
    );
    service_mgr::install(
        cfg,
        &SupervisedService {
            name: "kube-controller-manager",
            description: "Real upstream kube-controller-manager (not-k8s, no k3s)",
            exec_cmd: &cm_exec,
            after: Some("kube-apiserver.service"),
            env: &[],
        },
    )
    .context("installing kube-controller-manager as a supervised service")?;

    let sched_exec = format!("{} {}", bin_dir.join("kube-scheduler").display(), scheduler_args(&spec).join(" "));
    service_mgr::install(
        cfg,
        &SupervisedService {
            name: "kube-scheduler",
            description: "Real upstream kube-scheduler (not-k8s, no k3s)",
            exec_cmd: &sched_exec,
            after: Some("kube-apiserver.service"),
            env: &[],
        },
    )
    .context("installing kube-scheduler as a supervised service")?;

    Ok(())
}

/// Best-effort: polls `/readyz` with the admin client cert (`--anonymous-
/// auth=false` above means an anonymous request gets a real 401, not a
/// "not ready yet" -- same reasoning `upstream-kube-apiserver.sh`'s own
/// wait loop documents). Not a hard failure on timeout -- `kube-controller-
/// manager`/`kube-scheduler` retry connecting to the apiserver on their own
/// regardless, so this is a readiness nicety for clearer logs, not a
/// correctness requirement.
/// A real Rust TLS client (`ureq` + a `rustls::ClientConfig` trusting only
/// `pki.rs`'s own CA), not a `curl` subprocess. Deliberately checks "did
/// the apiserver answer at all" rather than "did `/readyz` return 200":
/// `--anonymous-auth=false` (set in `apiserver_args`) means an
/// unauthenticated request gets a real `401`, not a connection failure --
/// which is already proof the apiserver is up and terminating TLS
/// correctly. Presenting the admin client cert to get an authenticated
/// `200` instead would need `rustls::ClientConfig::with_client_auth_cert`
/// on top of this; not worth the extra complexity for a best-effort,
/// non-fatal readiness nicety (see this function's caller).
fn wait_for_readyz(spec: &TargetSpec) {
    let agent = match trusting_agent(&spec.pki_dir.join("ca.crt")) {
        Ok(agent) => agent,
        Err(e) => {
            tracing::warn!(error = ?e, "couldn't build a TLS client trusting the cluster CA -- skipping the readyz wait");
            return;
        }
    };
    tracing::info!("waiting for kube-apiserver to answer /readyz...");
    for _ in 0..30 {
        match agent.get("https://127.0.0.1:6443/readyz").call() {
            // Any real HTTP response -- including the 401 an anonymous
            // request gets -- means the apiserver is up. `ureq::Error::Status`
            // covers non-2xx responses; `Ok` covers 2xx.
            Ok(_) | Err(ureq::Error::Status(_, _)) => {
                tracing::info!("kube-apiserver is answering requests");
                return;
            }
            Err(_) => {} // connection-level failure -- keep waiting
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    tracing::warn!(
        "kube-apiserver never answered /readyz within 60s -- check: journalctl -u kube-apiserver \
         -n 50 (or the fallback tier's log under Config::log_dir)"
    );
}

/// Builds a `ureq::Agent` whose TLS trust root is exactly `ca_pem_path` --
/// nothing else, not the system CA store -- since the only thing this
/// agent ever talks to is our own freshly-minted apiserver on loopback.
fn trusting_agent(ca_pem_path: &std::path::Path) -> Result<ureq::Agent> {
    let ca_pem = std::fs::read(ca_pem_path).with_context(|| format!("reading {}", ca_pem_path.display()))?;
    let mut root_store = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut ca_pem.as_slice()) {
        root_store.add(cert.context("parsing CA cert PEM")?).context("adding CA cert to the trust root")?;
    }
    let config = rustls::ClientConfig::builder().with_root_certificates(root_store).with_no_client_auth();
    Ok(ureq::AgentBuilder::new().tls_config(std::sync::Arc::new(config)).build())
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
