use super::context::E2eContext;
use super::skip_test;
use anyhow::Result;
use kube::discovery;

pub(super) async fn resource_api_group_is_enabled(context: &E2eContext) -> Result<()> {
    let group = match discovery::group(&context.client, "resource.k8s.io").await {
        Ok(group) => group,
        Err(error) => {
            return Err(skip_test(format!(
                "resource.k8s.io/resourceclaims is not registered: {error}"
            )))
        }
    };
    if !group
        .recommended_resources()
        .iter()
        .any(|(resource, _)| resource.plural == "resourceclaims")
    {
        return Err(skip_test(
            "resource.k8s.io/resourceclaims is not registered; the apiserver DRA feature gate is unavailable on this deployment",
        ));
    }
    Ok(())
}
