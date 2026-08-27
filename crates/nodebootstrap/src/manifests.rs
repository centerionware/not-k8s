//! CoreDNS manifest, applied via the kubeconfig `kubeconfig.rs` wrote once
//! the target apiserver is reachable. The other half of Group O besides
//! PKI/RBAC/kubeconfig/Service-reconciler. See `vendor/README.md` for where
//! `vendor/coredns.yaml` came from and what its template placeholders mean.
//!
//! flannel is deliberately **not** here: unlike CoreDNS, this project's
//! flannel is a host-level `flanneld` daemon (`deploy/lib/cni.sh`), not a
//! Kubernetes-deployed DaemonSet manifest -- `cni.rs` owns it, not this
//! module. `docs/NODEBOOTSTRAP_PLAN.md`'s scope tree's mention of "flannel
//! manifests" here was aspirational and wrong; corrected as of this
//! finding.

use anyhow::{Context, Result};

use crate::config::Config;

/// `k3s-io/k3s`'s own `manifests/coredns.yaml` at `v1.33.13+k3s2` --
/// verbatim, see `vendor/README.md`.
const COREDNS_TEMPLATE: &str = include_str!("../vendor/coredns.yaml");

pub fn run() -> Result<()> {
    run_with(&Config::from_env()?)
}

pub fn run_with(cfg: &Config) -> Result<()> {
    if cfg.disable_dns {
        tracing::info!("NODEBOOTSTRAP_DISABLE_DNS=1 -- skipping CoreDNS manifest");
        return Ok(());
    }
    if cfg.skip_manifests {
        tracing::info!("skipping manifest apply (NODEBOOTSTRAP_SKIP_MANIFESTS)");
        return Ok(());
    }
    let manifest = render_coredns(&cfg.cluster_dns_ip(), &cfg.cluster_domain());
    apply(&cfg.kubeconfig_dir().join("admin.kubeconfig"), &manifest)
}

/// Fills in `vendor/coredns.yaml`'s `%{...}%` placeholders. See
/// `vendor/README.md`'s placeholder table for what each one means and why
/// the image reference is substituted wholesale rather than just its
/// registry prefix.
pub fn render_coredns(cluster_dns_ip: &str, cluster_domain: &str) -> String {
    COREDNS_TEMPLATE
        .replace("%{CLUSTER_DOMAIN}%", cluster_domain)
        .replace("%{CLUSTER_DNS}%", cluster_dns_ip)
        .replace("%{CLUSTER_DNS_LIST}%", &format!("[{cluster_dns_ip}]"))
        .replace("%{CLUSTER_DNS_IPFAMILYPOLICY}%", "SingleStack")
        .replace(
            "%{SYSTEM_DEFAULT_REGISTRY}%rancher/mirrored-coredns-coredns:1.14.6",
            "registry.k8s.io/coredns/coredns:v1.14.6",
        )
}

/// `kubectl apply -f -`, same subprocess-call posture `rbac.rs` uses and
/// explains (no `kube`/tokio stack for a one-shot CLI whose other checks
/// are all synchronous).
fn apply(kubeconfig: &std::path::Path, manifest: &str) -> Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("kubectl")
        .args(["--kubeconfig", &kubeconfig.to_string_lossy(), "apply", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("spawning kubectl apply")?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(manifest.as_bytes())
        .context("writing manifest to kubectl's stdin")?;
    let status = child.wait().context("waiting for kubectl apply")?;
    if !status.success() {
        anyhow::bail!("kubectl apply -f - (CoreDNS manifest) exited {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_coredns_leaves_no_placeholders_and_drops_the_rancher_mirror() {
        let rendered = render_coredns("10.43.0.10", "cluster.local");
        assert!(!rendered.contains("%{"), "unfilled placeholder remains:\n{rendered}");
        assert!(!rendered.contains("rancher/mirrored-coredns"));
        assert!(rendered.contains("registry.k8s.io/coredns/coredns:v1.14.6"));
        assert!(rendered.contains("clusterIP: 10.43.0.10"));
        assert!(rendered.contains("clusterIPs: [10.43.0.10]"));
        assert!(rendered.contains("kubernetes cluster.local in-addr.arpa ip6.arpa"));
    }

    #[test]
    fn render_coredns_uses_the_selected_cluster_domain() {
        let rendered = render_coredns("10.43.0.10", "cluster.example");
        assert!(rendered.contains("kubernetes cluster.example in-addr.arpa ip6.arpa"));
    }
}
