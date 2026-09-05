use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Namespace, Node, Pod, ServiceAccount};
use kube::api::{Api, ListParams, PostParams};
use serde_json::json;
use std::process::Command;
use std::time::Duration;

pub(super) async fn paginated_list_watch_preserves_concurrent_updates(context: &E2eContext) -> Result<()> {
    use futures::StreamExt;
    use k8s_openapi::api::core::v1::ConfigMap;
    use kube::api::{Patch, PatchParams, WatchEvent, WatchParams};

    let maps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);
    for name in ["snapshot-a", "snapshot-b", "snapshot-c"] {
        let map: ConfigMap = serde_json::from_value(json!({
            "apiVersion":"v1", "kind":"ConfigMap",
            "metadata":{"name":name,"labels":{"snapshot-test":"yes"}},
            "data":{"value":"before"}
        }))?;
        maps.create(&PostParams::default(), &map).await?;
    }
    let params = ListParams::default().labels("snapshot-test=yes").limit(1);
    let first = maps.list(&params).await?;
    let revision = first.metadata.resource_version.context("first page missing resourceVersion")?;
    anyhow::ensure!(first.items.len() == 1, "expected one object on the first page");
    let changed_name = first.items[0].metadata.name.as_deref().context("first item missing name")?;
    let changed = maps.patch(changed_name, &PatchParams::default(),
        &Patch::Merge(json!({"data":{"value":"after"}}))).await?;
    let changed_revision = changed.metadata.resource_version.context("PATCH missing resourceVersion")?;
    let mut token = first.metadata.continue_.context("first page missing continue token")?;
    let mut count = 1;
    while !token.is_empty() {
        let page = maps.list(&params.clone().continue_token(&token)).await?;
        anyhow::ensure!(page.metadata.resource_version.as_deref() == Some(revision.as_str()),
            "LIST snapshot advanced across a concurrent write: expected {revision}, got {:?}", page.metadata.resource_version);
        count += page.items.len();
        anyhow::ensure!(count <= 3, "pagination duplicated objects");
        token = page.metadata.continue_.unwrap_or_default();
    }
    anyhow::ensure!(count == 3, "pagination lost objects");
    let watch = maps.watch(&WatchParams::default().labels("snapshot-test=yes").timeout(20), &revision).await?;
    futures::pin_mut!(watch);
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(event) = watch.next().await {
            match event? {
                WatchEvent::Modified(map) if map.metadata.name.as_deref() == Some(changed_name)
                    && map.metadata.resource_version.as_deref() == Some(changed_revision.as_str()) => return Ok(()),
                WatchEvent::Error(error) => anyhow::bail!("watch returned an error: {error:?}"),
                _ => {}
            }
        }
        anyhow::bail!("watch ended without the update made between LIST pages")
    }).await.context("watch lost the update made between LIST pages")?
}

fn needs_cri() -> Result<()> {
    anyhow::ensure!(
        crate::config::Config::from_env()?.nodelet_runtime() == "cri",
        "watch recovery requires the CRI runtime",
    );
    Ok(())
}

fn active_control_plane_unit() -> Option<&'static str> {
    let configured = crate::config::Config::from_env()
        .ok()
        .map(|cfg| cfg.apiserver_service());
    configured
        .into_iter()
        .chain([
            "nodeapiserver.service",
            "kube-apiserver.service",
            "k3s.service",
        ])
        .find(|unit| {
            Command::new("systemctl")
                .args(["is-active", "--quiet", unit])
                .status()
                .is_ok_and(|status| status.success())
        })
}

fn restart_unit(unit: &str) -> Result<()> {
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .context("checking the e2e runner's uid")?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let mut command = if uid == "0" {
        let mut command = Command::new("systemctl");
        command.args(["restart", unit]);
        command
    } else {
        let mut command = Command::new("sudo");
        command.args(["systemctl", "restart", unit]);
        command
    };
    let output = command
        .output()
        .with_context(|| format!("restarting {unit}"))?;
    anyhow::ensure!(
        output.status.success(),
        "restarting {unit} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

async fn pod_is_running(pods: &Api<Pod>, name: &str) -> Result<bool> {
    Ok(pods
        .get(name)
        .await?
        .status
        .and_then(|status| status.phase)
        .as_deref()
        == Some("Running"))
}

pub(super) async fn node_still_reconciles_pods_after_an_apiserver_restart(
    context: &E2eContext,
) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let Some(unit) = active_control_plane_unit() else {
        return Err(skip_test(
            "no configured apiserver, nodeapiserver.service, kube-apiserver.service, or k3s.service is active to restart",
        ));
    };

    let nodes: Api<Node> = Api::all(context.client.clone());
    let node_name = nodes
        .list(&Default::default())
        .await?
        .items
        .into_iter()
        .find_map(|node| node.metadata.name)
        .ok_or_else(|| anyhow::anyhow!("the cluster has no node to pin the recovery Pod to"))?;

    restart_unit(unit)?;
    let namespaces: Api<Namespace> = Api::all(context.client.clone());
    context
        .wait_until(
            "the apiserver to become ready after restart",
            Duration::from_secs(180),
            || async { Ok(namespaces.list(&ListParams::default()).await.is_ok()) },
        )
        .await?;

    let service_accounts: Api<ServiceAccount> =
        Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until(
            "the e2e namespace's default ServiceAccount after restart",
            Duration::from_secs(60),
            || {
                let service_accounts = service_accounts.clone();
                async move { Ok(service_accounts.get_opt("default").await?.is_some()) }
            },
        )
        .await?;

    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let name = "watch-recovery-check";
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "nodeName": node_name,
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until(
            "a Pod created after the apiserver restart to reach Running",
            Duration::from_secs(120),
            || {
                let pods = pods.clone();
                async move { pod_is_running(&pods, name).await }
            },
        )
        .await
        .with_context(|| {
            format!(
                "the nodelet did not reconcile a Pod pinned to {node_name} after restarting {unit}"
            )
        })
}
