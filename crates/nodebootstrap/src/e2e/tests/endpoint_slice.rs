use super::context::E2eContext;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Pod, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use serde_json::json;
use std::time::Duration;

fn slice_has_address(slice: &EndpointSlice, address: &str) -> bool {
    slice
        .endpoints
        .iter()
        .any(|endpoint| endpoint.addresses.iter().any(|value| value == address))
}

pub(super) async fn endpointslice_is_produced_for_a_selected_pod(
    context: &E2eContext,
) -> Result<()> {
    let service_name = "endpoint-slice-test";
    let pod_name = "endpoint-slice-pod";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let services: Api<Service> = Api::namespaced(context.client.clone(), &context.namespace);
    let slices: Api<EndpointSlice> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": pod_name, "labels": {"app": service_name}},
        "spec": {"containers": [{"name": "busybox", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
    }))?;
    pods.create(&PostParams::default(), &pod)
        .await
        .context("creating EndpointSlice test Pod")?;
    let service: Service = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {"name": service_name},
        "spec": {
            "selector": {"app": service_name},
            "ports": [{"name": "http", "port": 80, "targetPort": 80}]
        }
    }))?;
    services
        .create(&PostParams::default(), &service)
        .await
        .context("creating EndpointSlice test Service")?;

    let result = async {
        let pod_ip = {
            context
                .wait_until("EndpointSlice test Pod Running with an IP", Duration::from_secs(90), || {
                    let pods = pods.clone();
                    async move {
                        Ok(pods
                            .get(pod_name)
                            .await?
                            .status
                            .and_then(|status| {
                                (status.phase.as_deref() == Some("Running"))
                                    .then_some(status.pod_ip)
                            })
                            .flatten()
                            .is_some())
                    }
                })
                .await?;
            pods.get(pod_name)
                .await?
                .status
                .and_then(|status| status.pod_ip)
                .context("EndpointSlice test Pod has no podIP after becoming Running")?
        };
        context
            .wait_until("EndpointSlice carries the selected Pod address", Duration::from_secs(60), || {
                let slices = slices.clone();
                let pod_ip = pod_ip.clone();
                async move {
                    Ok(slices
                        .list(&ListParams::default().labels(&format!(
                            "kubernetes.io/service-name={service_name}"
                        )))
                        .await?
                        .items
                        .iter()
                        .any(|slice| slice_has_address(slice, &pod_ip)))
                }
            })
            .await?;
        let slice = slices
            .list(&ListParams::default().labels(&format!(
                "kubernetes.io/service-name={service_name}"
            )))
            .await?
            .items
            .into_iter()
            .find(|slice| slice_has_address(slice, &pod_ip))
            .context("EndpointSlice disappeared after address wait")?;
        anyhow::ensure!(
            slice.endpoints.iter().any(|endpoint| {
                slice_has_address(&slice, &pod_ip)
                    && endpoint
                        .conditions
                        .as_ref()
                        .is_some_and(|conditions| conditions.ready == Some(true))
            }),
            "EndpointSlice endpoint for the Running Pod must be ready"
        );

        pods.delete(pod_name, &DeleteParams::default()).await?;
        context
            .wait_until("EndpointSlice test Pod deletion", Duration::from_secs(60), || {
                let pods = pods.clone();
                async move { Ok(pods.get_opt(pod_name).await?.is_none()) }
            })
            .await?;
        context
            .wait_until("EndpointSlice drops the deleted Pod address", Duration::from_secs(30), || {
                let slices = slices.clone();
                let pod_ip = pod_ip.clone();
                async move {
                    Ok(!slices
                        .list(&ListParams::default().labels(&format!(
                            "kubernetes.io/service-name={service_name}"
                        )))
                        .await?
                        .items
                        .iter()
                        .any(|slice| slice_has_address(slice, &pod_ip)))
                }
            })
            .await
    }
    .await;

    let _ = services.delete(service_name, &DeleteParams::default()).await;
    let _ = pods.delete(pod_name, &DeleteParams::default()).await;
    result
}
