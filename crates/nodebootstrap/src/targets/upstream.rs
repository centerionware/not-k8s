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

    let spec = target_spec(cfg);
    wait_for_nodestore(&spec.etcd_servers)?;
    install_apiserver(cfg, &spec, &bin_dir, None)?;

    wait_for_readyz(&spec);
    Ok(())
}

/// Nodelet creates its self-signed serving CA on first startup. Once that
/// file exists, replace the initial apiserver unit with the full kubelet
/// proxy configuration and restart it. This is the same two-phase handoff
/// as deploy/lib/control-plane.sh's `enable_kubelet_certificate_authority_trust`.
pub fn enable_nodelet_proxy(cfg: &Config) -> Result<()> {
    if !cfg.with_cri || cfg.skip_nodelet {
        return Ok(());
    }
    let cert_path = cfg.nodelet_server_ca_path();
    tracing::info!(path = %cert_path.display(), "waiting for nodelet's kubelet-style server CA");
    for _ in 0..15 {
        if std::fs::metadata(&cert_path).map(|m| m.is_file() && m.len() > 0).unwrap_or(false) {
            let bin_dir = cfg.toolchain_dir().join("bin");
            let bin = bin_dir.join("kube-apiserver");
            anyhow::ensure!(bin.exists(), "no kube-apiserver binary at {}", bin.display());
            let spec = target_spec(cfg);
            install_apiserver(cfg, &spec, &bin_dir, Some(&cert_path))?;
            wait_for_readyz(&spec);
            tracing::info!(path = %cert_path.display(), "kube-apiserver now trusts nodelet's kubelet-style server CA");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    anyhow::bail!(
        "nodelet never wrote {} within 30s -- kubelet proxying (exec/logs/attach/port-forward) cannot be configured",
        cert_path.display()
    )
}

/// The first apiserver start happens before CNI has created `cni0`, so
/// `target_spec()` has to fall back to loopback. That address is valid for
/// nodebootstrap's own kubeconfigs but invalid as the `default/kubernetes`
/// Service endpoint: the apiserver rejects loopback Endpoints, and pods then
/// cannot reach the ClusterIP used by CSI/DRA sidecars. Once nodelet has
/// started the bootstrap pods and flannel has written its subnet, `cni0`
/// exists and its address is the bridge gateway for every pod on this node.
/// Reinstall the unit once with that address so the apiserver's own
/// bootstrap-controller publishes a usable endpoint.
pub fn refresh_network_advertise_address(cfg: &Config) -> Result<()> {
    if !cfg.with_cri || cfg.skip_nodelet || cfg.cni_provider.as_deref() != Some("flannel") {
        return Ok(());
    }

    let advertise_address = wait_for_cni_address()?;
    let bin_dir = cfg.toolchain_dir().join("bin");
    let bin = bin_dir.join("kube-apiserver");
    anyhow::ensure!(bin.exists(), "no kube-apiserver binary at {}", bin.display());
    let cert_path = cfg.nodelet_server_ca_path();
    anyhow::ensure!(cert_path.is_file(), "nodelet server CA is missing at {}", cert_path.display());

    let mut spec = target_spec(cfg);
    spec.advertise_address = advertise_address.clone();
    install_apiserver(cfg, &spec, &bin_dir, Some(&cert_path))?;
    wait_for_readyz(&spec);
    crate::service_reconciler::reset_and_wait_for_reachable_endpoint(
        &cfg.kubeconfig_dir().join("admin.kubeconfig"),
    )?;
    tracing::info!(address = %advertise_address, "kube-apiserver now advertises the reachable CNI gateway");
    Ok(())
}

fn target_spec(cfg: &Config) -> TargetSpec {
    TargetSpec {
        pki_dir: cfg.pki_dir(),
        etcd_servers: nodestore_etcd_servers(),
        advertise_address: detect_advertise_address(),
        service_cidr: "10.43.0.0/16".to_string(),
        service_account_issuer: "https://kubernetes.default.svc.cluster.local".to_string(),
    }
}

fn install_apiserver(
    cfg: &Config,
    spec: &TargetSpec,
    bin_dir: &std::path::Path,
    nodelet_ca: Option<&std::path::Path>,
) -> Result<()> {
    let apiserver_exec = format!(
        "{} {}",
        bin_dir.join("kube-apiserver").display(),
        apiserver_args(spec, nodelet_ca).join(" ")
    );
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
    .context("installing kube-apiserver as a supervised service")
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

fn nodestore_client_pki_paths() -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let configured = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty()).map(std::path::PathBuf::from);
    (
        configured("NODEBOOTSTRAP_JOIN_CA_FILE")
            .or_else(|| configured("NODESTORE_CLIENT_CA_FILE"))
            .or_else(|| configured("NODESTORE_TRUSTED_CA_FILE"))
            .unwrap_or_else(|| nodestore_client_pki_dir().join("ca.crt")),
        configured("NODEBOOTSTRAP_JOIN_CERT_FILE")
            .or_else(|| configured("NODESTORE_CLIENT_CERT_FILE"))
            .unwrap_or_else(|| nodestore_client_pki_dir().join("client.crt")),
        configured("NODEBOOTSTRAP_JOIN_KEY_FILE")
            .or_else(|| configured("NODESTORE_CLIENT_KEY_FILE"))
            .unwrap_or_else(|| nodestore_client_pki_dir().join("client.key")),
    )
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
    detect_cni_address().unwrap_or_else(|| "127.0.0.1".to_string())
}

fn detect_cni_address() -> Option<String> {
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
}

fn wait_for_cni_address() -> Result<String> {
    tracing::info!("waiting for CNI bridge cni0 before publishing the apiserver Service endpoint...");
    for _ in 0..30 {
        if let Some(address) = detect_cni_address() {
            return Ok(address);
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    anyhow::bail!(
        "CNI bridge cni0 never appeared within 30s after flannel became ready -- the apiserver cannot publish a reachable kubernetes Service endpoint; check: ip addr show cni0; journalctl -u flanneld -n 100"
    )
}

struct TargetSpec {
    pki_dir: std::path::PathBuf,
    etcd_servers: String,
    advertise_address: String,
    service_cidr: String,
    service_account_issuer: String,
}

fn apiserver_args(spec: &TargetSpec, nodelet_ca: Option<&std::path::Path>) -> Vec<String> {
    let pki = |name: &str| spec.pki_dir.join(name).display().to_string();
    let (etcd_ca, etcd_cert, etcd_key) = nodestore_client_pki_paths();
    let mut args = vec![
        format!("--etcd-servers={}", spec.etcd_servers),
        format!("--etcd-cafile={}", etcd_ca.display()),
        format!("--etcd-certfile={}", etcd_cert.display()),
        format!("--etcd-keyfile={}", etcd_key.display()),
        "--secure-port=6443".to_string(),
        "--bind-address=0.0.0.0".to_string(),
        format!("--advertise-address={}", spec.advertise_address),
        format!("--tls-cert-file={}", pki("apiserver.crt")),
        format!("--tls-private-key-file={}", pki("apiserver.key")),
        // The CA that issued every client cert (admin/kube-controller-
        // manager/kube-scheduler/kube-apiserver) IS the client CA here.
        format!("--client-ca-file={}", pki("ca.crt")),
        format!("--kubelet-client-certificate={}", pki("kube-apiserver.crt")),
        format!("--kubelet-client-key={}", pki("kube-apiserver.key")),
        "--kubelet-preferred-address-types=InternalIP,Hostname,ExternalIP".to_string(),
        format!("--service-account-key-file={}", pki("sa.pub")),
        format!("--service-account-signing-key-file={}", pki("sa.key")),
        format!("--service-account-issuer={}", spec.service_account_issuer),
        format!("--service-cluster-ip-range={}", spec.service_cidr),
        "--authorization-mode=Node,RBAC".to_string(),
        "--enable-admission-plugins=NodeRestriction".to_string(),
        "--allow-privileged=true".to_string(),
        "--anonymous-auth=false".to_string(),
        // The reference DRA driver's admission policy relies on the node
        // name enrichment carried by projected ServiceAccount tokens. Device
        // plugin health is reported through PodStatus.allocatedResourcesStatus,
        // which is alpha and off by default in the v1.33 apiserver.
        "--feature-gates=ResourceHealthStatus=true,ServiceAccountTokenPodNodeInfo=true".to_string(),
        "--v=1".to_string(),
    ];
    if let Some(path) = nodelet_ca {
        args.push(format!("--kubelet-certificate-authority={}", path.display()));
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_spec() -> TargetSpec {
        TargetSpec {
            pki_dir: "/var/lib/nodebootstrap/pki".into(),
            etcd_servers: "https://127.0.0.1:2379".to_string(),
            advertise_address: "10.42.0.1".to_string(),
            service_cidr: "10.43.0.0/16".to_string(),
            service_account_issuer: "https://kubernetes.default.svc.cluster.local".to_string(),
        }
    }

    #[test]
    fn apiserver_args_reference_a_single_client_ca_not_a_two_ca_bundle() {
        let args = apiserver_args(&test_spec(), None);
        assert!(args.iter().any(|a| a == "--client-ca-file=/var/lib/nodebootstrap/pki/ca.crt"));
        assert!(args.iter().any(|a| a == "--authorization-mode=Node,RBAC"));
        assert!(!args.iter().any(|a| a.contains("bundle")));
    }

    #[test]
    fn apiserver_args_include_nodelet_proxy_identity_and_dra_token_enrichment() {
        let args = apiserver_args(&test_spec(), Some(std::path::Path::new("/var/lib/nodelet/pki/server-ca.pem")));
        assert!(args.iter().any(|a| a == "--kubelet-client-certificate=/var/lib/nodebootstrap/pki/kube-apiserver.crt"));
        assert!(args.iter().any(|a| a == "--kubelet-client-key=/var/lib/nodebootstrap/pki/kube-apiserver.key"));
        assert!(args.iter().any(|a| a == "--kubelet-certificate-authority=/var/lib/nodelet/pki/server-ca.pem"));
        assert!(args.iter().any(|a| a == "--kubelet-preferred-address-types=InternalIP,Hostname,ExternalIP"));
        assert!(args.iter().any(|a| a == "--feature-gates=ResourceHealthStatus=true,ServiceAccountTokenPodNodeInfo=true"));
    }
}
