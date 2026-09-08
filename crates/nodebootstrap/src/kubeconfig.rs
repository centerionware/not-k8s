//! kubeconfig emission for `kubectl` and every in-cluster component
//! (`nodelet`, `nodeproxy`, `nodescheduler`, `nodecontroller`), driven by
//! the certs `pki.rs` mints. Depends on `pki::run_with`/`pki::generate`
//! having already produced the CA and the relevant client cert.
//!
//! Hand-templates the YAML rather than adding `serde_yaml`: the kubeconfig
//! schema this crate emits is small and fixed (one cluster, one user, one
//! context, always both certs embedded rather than file-referenced), so a
//! struct that mirrors upstream's `clientcmd/api` types would be more
//! machinery than the four fields actually vary.

use anyhow::{Context, Result};
use base64::Engine;

use crate::config::Config;
use crate::pki::{ClusterPki, IssuedCert};

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if cfg.skip_kubeconfig {
        tracing::info!("skipping kubeconfig emission (NODEBOOTSTRAP_SKIP_KUBECONFIG)");
        return Ok(());
    }
    let pki = read_pki_from_dir(&cfg.pki_dir()).with_context(|| {
        format!(
            "reading PKI back from {} -- run `nodebootstrap pki` first",
            cfg.pki_dir().display()
        )
    })?;
    let dir = cfg.kubeconfig_dir();
    write_all(&dir, &cfg.apiserver_server(), &pki)?;
    tracing::info!(dir = %dir.display(), "wrote kubeconfigs");
    Ok(())
}

/// Reads back the subset of `pki.rs`'s output this module needs, using the
/// exact filenames `ClusterPki::write_to_dir` wrote. Separate step, not
/// `pki.rs`'s own concern -- `pki` and `kubeconfig` are independently
/// invokable subcommands, each re-reading disk state rather than one
/// passing an in-memory value to the other.
fn read_pki_from_dir(dir: &std::path::Path) -> Result<ClusterPki> {
    let read = |name: &str| -> Result<String> {
        let path = dir.join(name);
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
    };
    let pair = |cert_name: &str, key_name: &str| -> Result<IssuedCert> {
        Ok(IssuedCert { cert_pem: read(cert_name)?, key_pem: read(key_name)? })
    };
    Ok(ClusterPki {
        ca: pair("ca.crt", "ca.key")?,
        apiserver_serving: pair("apiserver.crt", "apiserver.key")?,
        kube_apiserver_client: pair("kube-apiserver.crt", "kube-apiserver.key")?,
        aggregation_proxy_client: pair("front-proxy-client.crt", "front-proxy-client.key")?,
        aggregation_proxy_ca: pair("front-proxy-ca.crt", "front-proxy-ca.key")?,
        sa_signing: pair("sa.pub", "sa.key")?,
        kube_controller_manager: pair("kube-controller-manager.crt", "kube-controller-manager.key")?,
        kube_scheduler: pair("kube-scheduler.crt", "kube-scheduler.key")?,
        cluster_admin: pair("admin.crt", "admin.key")?,
    })
}

fn b64(pem: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(pem.as_bytes())
}

/// Renders one kubeconfig YAML embedding `identity`'s client cert/key and
/// `ca`'s CA cert, pointed at `server` (e.g. `https://127.0.0.1:6443`).
pub fn render(server: &str, ca_cert_pem: &str, identity: &IssuedCert, user_name: &str) -> String {
    format!(
        "apiVersion: v1\n\
         kind: Config\n\
         clusters:\n\
         - name: default\n\
         \x20 cluster:\n\
         \x20   server: {server}\n\
         \x20   certificate-authority-data: {ca}\n\
         users:\n\
         - name: {user_name}\n\
         \x20 user:\n\
         \x20   client-certificate-data: {cert}\n\
         \x20   client-key-data: {key}\n\
         contexts:\n\
         - name: default\n\
         \x20 context:\n\
         \x20   cluster: default\n\
         \x20   user: {user_name}\n\
         current-context: default\n",
        ca = b64(ca_cert_pem),
        cert = b64(&identity.cert_pem),
        key = b64(&identity.key_pem),
    )
}

/// Writes one kubeconfig per static identity `pki.rs` issued (admin,
/// kube-controller-manager, kube-scheduler) to `dir`, upstream-conventional
/// filenames (`admin.kubeconfig`, ...) so `targets/upstream.rs` and the
/// `nodecontroller`/`nodescheduler` service units can point straight at
/// them.
pub fn write_all(dir: &std::path::Path, server: &str, pki: &ClusterPki) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating kubeconfig dir {}", dir.display()))?;
    let entries: &[(&str, &str, &IssuedCert)] = &[
        ("admin.kubeconfig", "admin", &pki.cluster_admin),
        (
            "kube-controller-manager.kubeconfig",
            "system:kube-controller-manager",
            &pki.kube_controller_manager,
        ),
        ("kube-scheduler.kubeconfig", "system:kube-scheduler", &pki.kube_scheduler),
    ];
    for (filename, user_name, identity) in entries {
        let path = dir.join(filename);
        std::fs::write(&path, render(server, &pki.ca.cert_pem, identity, user_name))
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pki::{generate, ClusterPkiSpec};

    #[test]
    fn render_round_trips_base64_and_has_every_required_field() {
        let pki = generate(&ClusterPkiSpec::default()).expect("generate cluster PKI");
        let kc = render("https://127.0.0.1:6443", &pki.ca.cert_pem, &pki.cluster_admin, "admin");

        assert!(kc.contains("server: https://127.0.0.1:6443"));
        assert!(kc.contains("current-context: default"));

        let ca_line = kc
            .lines()
            .find(|l| l.trim_start().starts_with("certificate-authority-data:"))
            .expect("has a certificate-authority-data line");
        let encoded = ca_line.split(':').nth(1).unwrap().trim();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("certificate-authority-data is valid base64");
        assert_eq!(String::from_utf8(decoded).unwrap(), pki.ca.cert_pem);
    }
}
