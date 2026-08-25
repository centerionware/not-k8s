use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use std::process::Command;

fn kubectl_api_resources() -> Result<String> {
    let output = Command::new("kubectl")
        .args(["api-resources", "--api-group=resource.k8s.io", "--no-headers"])
        .output()
        .context("running kubectl api-resources for DRA")?;
    anyhow::ensure!(
        output.status.success(),
        "kubectl api-resources for DRA failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(super) async fn resource_api_group_is_enabled(_context: &E2eContext) -> Result<()> {
    let resources = kubectl_api_resources()?;
    if !resources
        .lines()
        .any(|line| line.split_whitespace().next() == Some("resourceclaims"))
    {
        return Err(skip_test(
            "resource.k8s.io/resourceclaims is not registered; the apiserver DRA feature gate is unavailable on this deployment",
        ));
    }
    Ok(())
}
