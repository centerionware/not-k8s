use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use http::Request;
use k8s_openapi::api::authentication::v1::{TokenRequest, TokenRequestSpec};
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ListParams, PostParams};
use serde_json::json;
use std::time::Duration;

fn needs_cri() -> Result<()> {
    anyhow::ensure!(
        crate::config::Config::from_env()?.nodelet_runtime() == "cri",
        "nodelet metrics endpoints require the CRI runtime",
    );
    Ok(())
}

async fn create_usage_pod(context: &E2eContext, name: &str) -> Result<()> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("metrics test Pod Running", Duration::from_secs(90), || {
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
        .await
}

async fn node_internal_ip(context: &E2eContext) -> Result<String> {
    let nodes: Api<Node> = Api::all(context.client.clone());
    nodes
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .flat_map(|node| node.status.and_then(|status| status.addresses).unwrap_or_default())
        .find(|address| address.type_ == "InternalIP")
        .map(|address| address.address)
        .context("the cluster has no Node InternalIP for direct nodelet metrics access")
}

async fn create_token(context: &E2eContext) -> Result<String> {
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/namespaces/default/serviceaccounts/default/token")
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&TokenRequest {
            metadata: Default::default(),
            spec: TokenRequestSpec {
                audiences: vec!["https://kubernetes.default.svc".to_owned()],
                bound_object_ref: None,
                expiration_seconds: Some(600),
            },
            status: None,
        })?)?;
    context
        .client
        .request::<TokenRequest>(request)
        .await?
        .status
        .map(|status| status.token)
        .context("TokenRequest response had no status.token")
}

fn fetch_endpoint(agent: &ureq::Agent, node_ip: &str, token: &str, path: &str) -> Option<String> {
    let port = std::env::var("NODELET_SERVER_PORT").unwrap_or_else(|_| "10250".to_string());
    let url = format!("https://{node_ip}:{port}{path}");
    agent
        .get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .ok()?
        .into_string()
        .ok()
}

async fn endpoint_body(context: &E2eContext, pod_name: &str, path: &str) -> Result<String> {
    let token = create_token(context).await.map_err(|error| {
        skip_test(format!(
            "could not mint a default ServiceAccount token for the nodelet metrics endpoint: {error}"
        ))
    })?;
    if token.is_empty() {
        return Err(skip_test(
            "TokenRequest returned an empty token for the nodelet metrics endpoint",
        ));
    }
    let agent = crate::targets::upstream::trusting_agent(
        &crate::config::Config::from_env()?.nodelet_server_ca_path(),
    )
    .map_err(|error| skip_test(format!("could not load the nodelet server CA: {error}")))?;
    let agent_ref = &agent;
    let node_ip = node_internal_ip(context).await?;
    let body = std::sync::Arc::new(std::sync::Mutex::new(None));
    let body_for_check = body.clone();
    context
        .wait_until(
            "nodelet metrics endpoint to include the test Pod",
            Duration::from_secs(90),
            || {
                let body_for_check = body_for_check.clone();
                let node_ip = node_ip.clone();
                let token = token.clone();
                let path = path.to_string();
                let pod_name = pod_name.to_string();
                let agent = agent_ref;
                async move {
                    let Some(value) = fetch_endpoint(agent, &node_ip, &token, &path) else {
                        return Ok(false);
                    };
                    if value.contains(&pod_name) {
                        *body_for_check.lock().expect("metrics body mutex poisoned") = Some(value);
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                }
            },
        )
        .await?;
    let body = body
        .lock()
        .expect("metrics body mutex poisoned")
        .clone();
    body.context("metrics endpoint disappeared after the wait predicate succeeded")
}

pub(super) async fn stats_summary_returns_real_pod_usage(context: &E2eContext) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let name = "stats-check";
    create_usage_pod(context, name).await?;
    let body = endpoint_body(context, name, "/stats/summary").await?;
    for field in ["\"nodeName\"", "\"podRef\"", name] {
        anyhow::ensure!(
            body.contains(field),
            "/stats/summary did not contain {field}: {body:?}"
        );
    }
    Ok(())
}

pub(super) async fn metrics_resource_returns_real_pod_usage(
    context: &E2eContext,
) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let name = "metrics-resource-check";
    create_usage_pod(context, name).await?;
    let body = endpoint_body(context, name, "/metrics/resource").await?;
    for marker in [
        "# TYPE node_cpu_usage_seconds_total counter",
        "# TYPE container_memory_working_set_bytes gauge",
        &format!("pod=\"{name}\""),
    ] {
        anyhow::ensure!(
            body.contains(marker),
            "/metrics/resource did not contain {marker}: {body:?}"
        );
    }
    Ok(())
}

pub(super) async fn metrics_cadvisor_returns_real_container_usage(
    context: &E2eContext,
) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let name = "metrics-cadvisor-check";
    create_usage_pod(context, name).await?;
    let body = endpoint_body(context, name, "/metrics/cadvisor").await?;
    for marker in [
        "# TYPE container_cpu_usage_seconds_total counter",
        "container=\"app\"",
        "# TYPE container_last_seen gauge",
        "container_last_seen{namespace=",
        "# TYPE container_network_receive_bytes_total counter",
        "# TYPE container_network_transmit_bytes_total counter",
    ] {
        anyhow::ensure!(
            body.contains(marker),
            "/metrics/cadvisor did not contain {marker}: {body:?}"
        );
    }
    Ok(())
}
