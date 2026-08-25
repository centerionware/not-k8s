use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};

/// This check is selected by the external-CNI workflow mode. A normal
/// single-node run intentionally skips it because flannel is expected there.
pub(super) async fn external_cni_mode_disables_flannel(context: &E2eContext) -> Result<()> {
    let cfg = crate::config::Config::from_env()?;
    if !cfg.without_flannel && cfg.cni_provider.as_deref() == Some("flannel") {
        return Err(skip_test(
            "flannel is enabled for this bootstrap; run the external-cni workflow mode to exercise --without-flannel",
        ));
    }
    anyhow::ensure!(
        cfg.cni_provider.is_none(),
        "external-CNI mode must not select an internally managed provider: {:?}",
        cfg.cni_provider
    );
    anyhow::ensure!(
        cfg.without_flannel,
        "external-CNI mode must persist the --without-flannel preference"
    );
    let nodes = kube::api::Api::<k8s_openapi::api::core::v1::Node>::all(context.client.clone())
        .list(&kube::api::ListParams::default())
        .await
        .context("checking that the external-CNI bootstrap still registered a node")?;
    anyhow::ensure!(!nodes.items.is_empty(), "external-CNI bootstrap registered no nodes");

    anyhow::ensure!(
        std::process::Command::new("systemctl")
            .args(["is-active", "--quiet", "nodelet"])
            .status()
            .is_ok_and(|status| status.success()),
        "nodelet is not active after the external-CNI bootstrap"
    );
    if std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", "flanneld"])
        .status()
        .is_ok_and(|status| status.success())
    {
        anyhow::bail!("flanneld is active after --without-flannel");
    }
    if std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", "nodeproxy"])
        .status()
        .is_ok_and(|status| status.success())
    {
        anyhow::bail!("nodeproxy is active after --proxy=none");
    }
    Ok(())
}

pub(super) async fn graceful_node_shutdown_manual_note(_context: &E2eContext) -> Result<()> {
    Err(skip_test(
        "graceful node shutdown requires a real systemd-logind PrepareForShutdown signal; manual verification is documented in the archived graceful_shutdown case",
    ))
}
