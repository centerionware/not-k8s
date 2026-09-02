use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Pod, ServiceAccount};
use kube::api::{Api, AttachParams, PostParams};
use kube::ResourceExt;
use serde_json::json;
use std::time::Duration;
use tokio::io::AsyncReadExt;

async fn exec_output(context: &E2eContext, pod_name: &str, command: &[&str]) -> Result<String> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let params = AttachParams::default()
        .container("app")
        .stdout(true)
        .stderr(false);
    let mut process = pods.exec(pod_name, command.iter().copied(), &params).await?;
    let mut stdout = Vec::new();
    if let Some(mut stream) = process.stdout() {
        stream.read_to_end(&mut stdout).await?;
    }
    process.join().await?;
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

pub(super) async fn projected_service_account_token_retries_after_service_account_appears(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test(
            "projected ServiceAccount token retry requires the CRI runtime",
        ));
    }

    let suffix = std::process::id();
    let service_account_name = format!("delayed-token-sa-{suffix}");
    let pod_name = format!("delayed-token-pod-{suffix}");
    let service_accounts: Api<ServiceAccount> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);

    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": pod_name},
        "spec": {
            "serviceAccountName": service_account_name,
            "restartPolicy": "Never",
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "test -s /var/run/secrets/tokens/api-token && sleep 3600"]
            }],
            "volumes": [{
                "name": "api-token",
                "projected": {
                    "sources": [{
                        "serviceAccountToken": {
                            "path": "api-token",
                            "expirationSeconds": 600
                        }
                    }]
                }
            }]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;

    // The pod must not start while its required ServiceAccount token request
    // returns 404. A successful implementation reports Pending and retries;
    // the old implementation started without the token instead.
    tokio::time::sleep(Duration::from_secs(10)).await;
    let stalled = pods
        .get(&pod_name)
        .await
        .context("reading delayed-token Pod before creating its ServiceAccount")?;
    anyhow::ensure!(
        stalled
            .status
            .as_ref()
            .and_then(|status| status.phase.as_deref())
            != Some("Running"),
        "Pod started before its referenced ServiceAccount existed"
    );

    let service_account: ServiceAccount = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "ServiceAccount",
        "metadata": {"name": service_account_name}
    }))?;
    service_accounts
        .create(&PostParams::default(), &service_account)
        .await?;

    context
        .wait_until("Pod to start after its ServiceAccount appears", Duration::from_secs(60), || {
            let pods = pods.clone();
            let pod_name = pod_name.clone();
            async move {
                Ok(pods
                    .get(&pod_name)
                    .await?
                    .status
                    .and_then(|status| status.phase)
                    .as_deref()
                    == Some("Running"))
            }
        })
        .await?;
    let token = exec_output(context, &pod_name, &["cat", "/var/run/secrets/tokens/api-token"]).await?;
    anyhow::ensure!(!token.trim().is_empty(), "Pod started without a projected ServiceAccount token");
    Ok(())
}
