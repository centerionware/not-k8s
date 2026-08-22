//! Installs real upstream `kube-apiserver` against `nodestore`, wired up
//! with the PKI `pki.rs` minted (not k3s's) -- `main`'s default target for
//! the one piece not yet replaced by `nodeapiserver`.
//!
//! Replaces `deploy/lib/upstream-kube-apiserver.sh` -- but where that
//! script deliberately *borrows* k3s's already-generated PKI (see its
//! header comment: nothing else minted one), this target starts from
//! `pki.rs`'s own output instead, so there is no k3s in the loop at all.
//!
//! **`kube-controller-manager`/`kube-scheduler` are deliberately NOT
//! installed here** (decided 2026-08-22, user direction -- overriding this
//! module's earlier "measure against real upstream" framing, which
//! `upstream-kube-controller-manager.sh`/`upstream-kube-scheduler.sh`
//! still do for that purpose). `nodecontroller`/`nodescheduler` already
//! exist and are already built on `main` -- there is no reason to run the
//! upstream binaries they exist to replace. `services.rs`'s
//! `ensure_nodecontroller`/`ensure_nodescheduler` fill that role instead,
//! wired into `run_all()` right after this module's `kube-apiserver`
//! install, using the `kube-controller-manager.kubeconfig`/`kube-
//! scheduler.kubeconfig` `pki.rs`/`kubeconfig.rs` already mint for exactly
//! those two identities.

use anyhow::{Context, Result};

use crate::config::Config;
use crate::service_mgr::{self, SupervisedService};

/// **Bumped off `v1.33` to `v1.34` (2026-08-22), found live**:
/// `nodescheduler`'s DRA cache (`crates/nodescheduler/src/cache/dra.rs`)
/// hardcodes `resource.k8s.io/v1` -- the GA path DRA only reached in
/// Kubernetes 1.34 (same finding `APISERVER_PLAN.md`'s k8s-openapi
/// `v1_33` -> `v1_34` bump note records: "resource.k8s.io/v1 now exists
/// and is typed"). On real `v1.33.13`, that group/version genuinely isn't
/// served at all -- no `--feature-gates`/`--runtime-config` combination
/// makes a GA API exist in a release that predates it; that's a
/// build-time compiled fact of the binary, not a config toggle. `1.34`
/// makes `nodescheduler`'s already-hardcoded expectation correct instead
/// of trying to make an older server pretend otherwise.
/// `vendor/README.md`'s CoreDNS pin note (matching this workspace's
/// `k8s-openapi` `v1_33` feature) is now cosmetically stale -- CoreDNS
/// itself has no real version-compatibility requirement here, so left
/// alone rather than re-vendored for a purely cosmetic mismatch. Unlike
/// `upstream-kube-apiserver.sh`, this is **not** derived from a running
/// k3s's own version at all -- there is no k3s to ask.
const K8S_VERSION: &str = "v1.34.11";

pub fn run_with(cfg: &Config) -> Result<()> {
    let arch = k8s_dl_arch(&cfg.arch()).with_context(|| format!("unsupported arch for upstream binaries: {}", cfg.arch()))?;
    let bin_dir = cfg.toolchain_dir().join("bin");
    std::fs::create_dir_all(&bin_dir).context("creating toolchain bin dir")?;
    fetch_binary("kube-apiserver", arch, &bin_dir)?;

    let advertise_address = detect_advertise_address();
    let spec = TargetSpec {
        pki_dir: cfg.pki_dir(),
        etcd_pki_dir: nodestore_client_pki_dir(),
        etcd_servers: nodestore_etcd_servers(),
        advertise_address,
        service_cidr: "10.43.0.0/16".to_string(),
        service_account_issuer: "https://kubernetes.default.svc.cluster.local".to_string(),
    };
    wait_for_nodestore(&spec.etcd_servers)?;
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
    Ok(())
}

/// A **hard** wait, not best-effort like `wait_for_readyz` below: unlike a
/// slow apiserver (which retries its own etcd connection internally and
/// recovers), kube-apiserver's `rbac/bootstrap-roles` PostStartHook does
/// not retry on its own initial failure. Found live, not guessed: a real
/// e2e run failed with `poststarthook/rbac/bootstrap-roles failed: reason
/// withheld` on `/readyz`, and `journalctl -u kube-apiserver` showed
/// exactly why --
///
/// ```text
/// grpc: addrConn.createTransport failed to connect to {Addr: "127.0.0.1:2379"...}
/// Err: connection error: desc = "transport: authentication handshake failed: context canceled"
/// ```
///
/// `service_mgr.rs`'s `after: Some("nodestore.service")` (systemd
/// ordering) only guarantees nodestore's *unit* started before
/// kube-apiserver's does -- not that its gRPC/TLS listener is actually
/// accepting connections yet. A plain TCP connect is enough to prove that
/// (it doesn't need to complete a real etcd v3 handshake, just prove the
/// listener is bound and accepting) and is far cheaper than parsing
/// nodestore's own readiness signal out of its logs.
fn wait_for_nodestore(etcd_servers: &str) -> Result<()> {
    let addr = etcd_servers.trim_start_matches("https://").trim_start_matches("http://");
    tracing::info!(addr, "waiting for nodestore to accept connections...");
    for _ in 0..30 {
        if std::net::TcpStream::connect(addr).is_ok() {
            tracing::info!("nodestore is accepting connections");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    anyhow::bail!("nodestore never started accepting connections at {addr} within 30s -- check: journalctl -u nodestore")
}

/// A real Rust TLS client (`ureq` + a `rustls::ClientConfig` trusting only
/// `pki.rs`'s own CA), not a `curl` subprocess. Deliberately checks "did
/// the apiserver answer at all" rather than "did `/readyz` return 200":
/// `--anonymous-auth=false` (set in `apiserver_args`) means an
/// unauthenticated request gets a real `401`, not a connection failure --
/// which is already proof the apiserver is up and terminating TLS
/// correctly. Not a hard failure on timeout -- `nodecontroller`/
/// `nodescheduler` (started right after this, by `run_all()`) retry
/// connecting to the apiserver on their own regardless, so this is a
/// readiness nicety for clearer logs, not a correctness requirement.
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
        // nodescheduler (this project's own, already built on main) watches
        // resource.k8s.io/v1 (DRA) unconditionally on startup -- GA as of
        // K8S_VERSION's 1.34 (see that constant's own comment for the full
        // story: 1.33 doesn't serve this group/version at all, no flag
        // fixes that), so no extra flag is needed here at all: a GA API is
        // served by default. `--runtime-config=api/all=true` was tried
        // here first and removed (found live): it turns on every alpha/
        // experimental API group unconditionally, and one of them broke
        // the `rbac/bootstrap-roles` PostStartHook `rbac.rs` depends on
        // entirely -- readyz went from "ready" to permanently failing
        // that hook. Enabling only what's actually needed, not "everything
        // just in case", is the real lesson.
        "--v=1".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_spec() -> TargetSpec {
        TargetSpec {
            pki_dir: "/var/lib/nodebootstrap/pki".into(),
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
}
