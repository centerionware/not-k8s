use super::context::E2eContext;
use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use kube::api::{Api, DeleteParams, PostParams};
use serde_json::json;
use std::time::Duration;

pub(super) async fn disruption_controller_computes_pdb_status(context: &E2eContext) -> Result<()> {
    let deployment_name = "pdb-test";
    let pdb_name = "pdb-test-budget";
    let deployments: Api<Deployment> = Api::namespaced(context.client.clone(), &context.namespace);
    let pdbs: Api<PodDisruptionBudget> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let deployment: Deployment = serde_json::from_value(json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": deployment_name},
        "spec": {"replicas": 3, "selector": {"matchLabels": {"app": deployment_name}}, "template": {
            "metadata": {"labels": {"app": deployment_name}},
            "spec": {"containers": [{"name": "busybox", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
        }}
    }))?;
    let pdb: PodDisruptionBudget = serde_json::from_value(json!({
        "apiVersion": "policy/v1",
        "kind": "PodDisruptionBudget",
        "metadata": {"name": pdb_name},
        "spec": {"minAvailable": 2, "selector": {"matchLabels": {"app": deployment_name}}}
    }))?;
    pdbs.create(&PostParams::default(), &pdb)
        .await
        .context("creating PodDisruptionBudget")?;
    // Deliberately publish the PDB before the Deployment. Controllers must
    // reconcile the empty initial match and then converge when the selected
    // Pods arrive on the independent Pod watch.
    deployments
        .create(&PostParams::default(), &deployment)
        .await
        .context("creating PDB test Deployment")?;

    let result = async {
        context
            .wait_until("PDB test Deployment has three ready Pods", Duration::from_secs(90), || {
                let deployments = deployments.clone();
                async move {
                    Ok(deployments
                        .get(deployment_name)
                        .await?
                        .status
                        .and_then(|status| status.ready_replicas)
                        == Some(3))
                }
            })
            .await?;
        context
            .wait_until("PDB expectedPods=3", Duration::from_secs(30), || {
                let pdbs = pdbs.clone();
                async move {
                    Ok(pdbs
                        .get(pdb_name)
                        .await?
                        .status
                        .is_some_and(|status| status.expected_pods == 3))
                }
            })
            .await?;
        context
            .wait_until("PDB currentHealthy=3", Duration::from_secs(30), || {
                let pdbs = pdbs.clone();
                async move {
                    Ok(pdbs
                        .get(pdb_name)
                        .await?
                        .status
                        .is_some_and(|status| status.current_healthy == 3))
                }
            })
            .await?;
        context
            .wait_until("PDB desiredHealthy=2", Duration::from_secs(30), || {
                let pdbs = pdbs.clone();
                async move {
                    Ok(pdbs
                        .get(pdb_name)
                        .await?
                        .status
                        .is_some_and(|status| status.desired_healthy == 2))
                }
            })
            .await?;
        context
            .wait_until("PDB disruptionsAllowed=1", Duration::from_secs(30), || {
                let pdbs = pdbs.clone();
                async move {
                    Ok(pdbs
                        .get(pdb_name)
                        .await?
                        .status
                        .is_some_and(|status| status.disruptions_allowed == 1))
                }
            })
            .await
    }
    .await;

    let _ = pdbs.delete(pdb_name, &DeleteParams::default()).await;
    let _ = deployments
        .delete(deployment_name, &DeleteParams::default())
        .await;
    result
}
