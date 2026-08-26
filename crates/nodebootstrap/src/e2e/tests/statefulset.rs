use super::context::E2eContext;
use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use serde_json::json;
use std::time::Duration;

pub(super) async fn statefulset_creates_ordinal_pods_and_scales_down_highest_first(
    context: &E2eContext,
) -> Result<()> {
    let name = "statefulset-controller";
    let statefulsets: Api<StatefulSet> = Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let statefulset: StatefulSet = serde_json::from_value(json!({
        "apiVersion": "apps/v1",
        "kind": "StatefulSet",
        "metadata": {"name": name},
        "spec": {
            "serviceName": name,
            "replicas": 2,
            "selector": {"matchLabels": {"app": name}},
            "template": {"metadata": {"labels": {"app": name}}, "spec": {
                "containers": [{
                    "name": "busybox",
                    "image": "busybox:latest",
                    "command": ["sh", "-c", "sleep 15; touch /tmp/release; sleep 3600"],
                    "readinessProbe": {"exec": {"command": ["test", "-f", "/tmp/release"]}, "periodSeconds": 1}
                }]
            }}
        }
    }))?;
    statefulsets
        .create(&PostParams::default(), &statefulset)
        .await
        .context("creating StatefulSet")?;
    context
        .wait_until("StatefulSet ordinal zero", Duration::from_secs(60), || {
            let pods = pods.clone();
            async move { Ok(pods.get_opt(&format!("{name}-0")).await?.is_some()) }
        })
        .await?;
    context
        .wait_until(
            "StatefulSet ordinal zero to be Running but initially unready",
            Duration::from_secs(30),
            || {
                let pods = pods.clone();
                async move {
                    let pod = pods.get(&format!("{name}-0")).await?;
                    let running = pod
                        .status
                        .as_ref()
                        .and_then(|status| status.phase.as_deref())
                        == Some("Running");
                    let ready = pod
                        .status
                        .and_then(|status| status.conditions)
                        .unwrap_or_default()
                        .iter()
                        .any(|condition| {
                            condition.type_ == "Ready" && condition.status == "True"
                        });
                    Ok(running && !ready)
                }
            },
        )
        .await?;
    anyhow::ensure!(
        pods.get_opt(&format!("{name}-1")).await?.is_none(),
        "OrderedReady created ordinal one before ordinal zero became ready"
    );
    context
        .wait_until(
            "StatefulSet ordinal one after ordinal zero is ready",
            Duration::from_secs(90),
            || {
                let pods = pods.clone();
                async move { Ok(pods.get_opt(&format!("{name}-1")).await?.is_some()) }
            },
        )
        .await?;
    context
        .wait_until(
            "StatefulSet to report two ready replicas",
            Duration::from_secs(90),
            || {
                let statefulsets = statefulsets.clone();
                async move {
                    Ok(statefulsets
                        .get(name)
                        .await?
                        .status
                        .and_then(|status| status.ready_replicas)
                        == Some(2))
                }
            },
        )
        .await?;
    let patch = json!({"spec": {"replicas": 1}});
    statefulsets
        .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .context("scaling StatefulSet")?;
    context
        .wait_until(
            "StatefulSet to delete the highest ordinal first",
            Duration::from_secs(60),
            || {
                let pods = pods.clone();
                async move { Ok(pods.get_opt(&format!("{name}-1")).await?.is_none()) }
            },
        )
        .await?;
    anyhow::ensure!(
        pods.get_opt(&format!("{name}-0")).await?.is_some(),
        "StatefulSet deleted ordinal zero instead of the highest ordinal"
    );
    let _ = statefulsets.delete(name, &DeleteParams::default()).await;
    Ok(())
}

pub(super) async fn statefulset_with_a_volume_claim_template_creates_an_accepted_pod(
    context: &E2eContext,
) -> Result<()> {
    let name = "statefulset-controller-pvc";
    let statefulsets: Api<StatefulSet> = Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pvcs: Api<PersistentVolumeClaim> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let statefulset: StatefulSet = serde_json::from_value(json!({
        "apiVersion": "apps/v1",
        "kind": "StatefulSet",
        "metadata": {"name": name},
        "spec": {
            "serviceName": name,
            "replicas": 1,
            "selector": {"matchLabels": {"app": name}},
            "template": {"metadata": {"labels": {"app": name}}, "spec": {
                "containers": [{"name": "busybox", "image": "busybox:latest", "command": ["sleep", "3600"], "volumeMounts": [{"name": "data", "mountPath": "/data"}]}]
            }},
            "volumeClaimTemplates": [{"metadata": {"name": "data"}, "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "64Mi"}}}}]
        }
    }))?;
    statefulsets
        .create(&PostParams::default(), &statefulset)
        .await
        .context("creating StatefulSet with a volume claim template")?;
    let pod_name = format!("{name}-0");
    context
        .wait_until("StatefulSet PVC-backed Pod", Duration::from_secs(60), || {
            let pods = pods.clone();
            let pod_name = pod_name.clone();
            async move { Ok(pods.get_opt(&pod_name).await?.is_some()) }
        })
        .await?;
    let pod = pods.get(&pod_name).await?;
    let volume = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.volumes.as_ref())
        .into_iter()
        .flatten()
        .find(|volume| volume.name == "data")
        .context("StatefulSet Pod is missing the injected data volume")?;
    anyhow::ensure!(
        volume
            .persistent_volume_claim
            .as_ref()
            .is_some_and(|claim| claim.claim_name == "data-statefulset-controller-pvc-0"),
        "StatefulSet Pod volume must reference the generated data-statefulset-controller-pvc-0 claim"
    );
    let _ = statefulsets.delete(name, &DeleteParams::default()).await;
    let _ = pvcs
        .delete_collection(&DeleteParams::default(), &ListParams::default())
        .await;
    Ok(())
}
