use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use http::Request;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, PostParams};
use kube::discovery;
use serde_json::json;
use std::time::Duration;

async fn request_json(
    context: &E2eContext,
    method: &str,
    uri: String,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value> {
    let mut request = Request::builder().method(method).uri(uri);
    if body.is_some() {
        request = request.header("Content-Type", "application/json");
    }
    Ok(context
        .client
        .request(request.body(serde_json::to_vec(&body.unwrap_or_default())?)?)
        .await?)
}

async fn delete_resource(context: &E2eContext, uri: String) -> Result<()> {
    let request = Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(br#"{}"#.to_vec())?;
    let _: serde_json::Value = context.client.request(request).await?;
    Ok(())
}

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

pub(super) async fn plugin_registry_watches_for_dra_drivers_too(
    _context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("DRA plugin registration checks require the CRI runtime"));
    }
    let path = std::env::var("NODELET_PLUGIN_REGISTRY_PATH")
        .unwrap_or_else(|_| "/var/lib/nodelet/plugins_registry".to_owned());
    if !std::path::Path::new(&path).is_dir() {
        return Err(skip_test(format!(
            "plugin registry directory {path} is not present on this deployment"
        )));
    }
    Ok(())
}

pub(super) async fn dra_claim_is_allocated_and_reserved_for_the_pod(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("DRA claim allocation requires the CRI runtime"));
    }
    let device_class = match std::env::var("TEST_DRA_DEVICE_CLASS") {
        Ok(value) if !value.is_empty() => value,
        _ => {
            return Err(skip_test(
                "TEST_DRA_DEVICE_CLASS is not set; the reference DRA driver is unavailable",
            ))
        }
    };
    let group = discovery::group(&context.client, "resource.k8s.io")
        .await
        .map_err(|error| skip_test(format!("DRA API discovery failed: {error}")))?;
    if !group
        .recommended_resources()
        .iter()
        .any(|(resource, _)| resource.plural == "resourceclaims")
    {
        return Err(skip_test(
            "resource.k8s.io/resourceclaims is not registered on this apiserver",
        ));
    }

    let name = "dra-claim-check";
    let template = format!("{name}-template");
    let template_uri = format!(
        "/apis/resource.k8s.io/v1/namespaces/{}/resourceclaimtemplates",
        context.namespace
    );
    let template_body = json!({
        "apiVersion": "resource.k8s.io/v1",
        "kind": "ResourceClaimTemplate",
        "metadata": {"name": template},
        "spec": {"spec": {"devices": {"requests": [{
            "name": "gpu",
            "exactly": {"deviceClassName": device_class}
        }]}}}
    });
    request_json(context, "POST", template_uri.clone(), Some(template_body)).await?;

    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "env; sleep 3600"], "resources": {"claims": [{"name": "gpu"}]}}],
            "resourceClaims": [{"name": "gpu", "resourceClaimTemplateName": template}]
        }
    }))?;
    let claims_uri = format!(
        "/apis/resource.k8s.io/v1/namespaces/{}/resourceclaims",
        context.namespace
    );
    let result = async {
        pods.create(&PostParams::default(), &pod).await?;
        context
            .wait_until("DRA claim Pod to reach Running", Duration::from_secs(120), || {
                let pods = pods.clone();
                async move {
                    Ok(pods
                        .get(name)
                        .await?
                        .status
                        .and_then(|status| status.phase)
                        .as_deref()
                        == Some("Running"))
                }
            })
            .await?;
        let claims = request_json(context, "GET", claims_uri.clone(), None).await?;
        let claim_name = claims
            .pointer("/items")
            .and_then(serde_json::Value::as_array)
            .and_then(|items| {
                items.iter().find_map(|claim| {
                    claim
                        .pointer("/metadata/ownerReferences")
                        .and_then(serde_json::Value::as_array)
                        .and_then(|owners| {
                            owners
                                .iter()
                                .any(|owner| {
                                    owner.get("name").and_then(serde_json::Value::as_str)
                                        == Some(name)
                                })
                                .then_some(())
                        })
                        .then(|| claim.pointer("/metadata/name"))
                        .flatten()
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
            })
            .context("no ResourceClaim owned by the DRA test Pod")?;
        let claim = request_json(context, "GET", format!("{claims_uri}/{claim_name}"), None).await?;
        anyhow::ensure!(
            claim.pointer("/status/allocation").is_some_and(|value| !value.is_null()),
            "DRA ResourceClaim {claim_name} has no status.allocation: {claim}"
        );
        anyhow::ensure!(
            claim
                .pointer("/status/reservedFor")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|reserved| reserved.iter().any(|entry| {
                    entry.get("name").and_then(serde_json::Value::as_str) == Some(name)
                })),
            "DRA ResourceClaim {claim_name} was not reserved for Pod {name}: {claim}"
        );
        Ok(())
    }
    .await;
    let _ = pods.delete(name, &kube::api::DeleteParams::default()).await;
    let _ = delete_resource(context, template_uri).await;
    result
}
