use super::context::E2eContext;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{ConfigMap, Namespace};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, PostParams};
use std::time::Duration;

pub(super) async fn namespace_controller_deletes_contents_before_finalizing(
    context: &E2eContext,
) -> Result<()> {
    let name = format!("nodebootstrap-e2e-namespace-{}", std::process::id());
    let namespaces: Api<Namespace> = Api::all(context.client.clone());
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &name);
    namespaces
        .create(
            &PostParams::default(),
            &Namespace {
                metadata: ObjectMeta {
                    name: Some(name.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .context("creating namespace-controller test Namespace")?;
    configmaps
        .create(
            &PostParams::default(),
            &ConfigMap {
                metadata: ObjectMeta {
                    name: Some("must-be-cleaned".to_string()),
                    ..Default::default()
                },
                data: Some([(String::from("proof"), String::from("namespace-controller"))]
                    .into_iter()
                    .collect()),
                ..Default::default()
            },
        )
        .await
        .context("creating namespace-controller test ConfigMap")?;
    context
        .wait_until("namespace contents exist before deletion", Duration::from_secs(30), || {
            let configmaps = configmaps.clone();
            async move { Ok(configmaps.get_opt("must-be-cleaned").await?.is_some()) }
        })
        .await?;
    namespaces
        .delete(&name, &DeleteParams::default())
        .await
        .context("deleting namespace-controller test Namespace")?;
    context
        .wait_until(
            "namespace-controller removes the namespaced object",
            Duration::from_secs(120),
            || {
                let configmaps = configmaps.clone();
                async move { Ok(configmaps.get_opt("must-be-cleaned").await?.is_none()) }
            },
        )
        .await?;
    context
        .wait_until("namespace-controller removes the Namespace", Duration::from_secs(120), || {
            let namespaces = namespaces.clone();
            async move { Ok(namespaces.get_opt(&name).await?.is_none()) }
        })
        .await
}
