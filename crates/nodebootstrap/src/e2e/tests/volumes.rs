use super::context::E2eContext;
use anyhow::Result;
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Secret};
use kube::api::{Api, Patch, PatchParams, PostParams};
use serde_json::json;
use std::time::Duration;

async fn create_pod(context: &E2eContext, name: &str, spec: serde_json::Value) -> Result<()> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": spec
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    Ok(())
}

async fn terminated_message(context: &E2eContext, name: &str) -> Result<Option<String>> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    Ok(pods
        .get(name)
        .await?
        .status
        .and_then(|status| status.container_statuses)
        .unwrap_or_default()
        .into_iter()
        .find(|status| status.name == "app")
        .and_then(|status| status.state)
        .and_then(|state| state.terminated)
        .and_then(|terminated| terminated.message))
}

pub(super) async fn configmap_and_secret_volumes_are_materialized(
    context: &E2eContext,
) -> Result<()> {
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);
    let secrets: Api<Secret> = Api::namespaced(context.client.clone(), &context.namespace);
    let configmap: ConfigMap = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {"name": "volume-config"},
        "data": {"config": "config-value"}
    }))?;
    let secret: Secret = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {"name": "volume-secret"},
        "stringData": {"secret": "secret-value"}
    }))?;
    configmaps.create(&PostParams::default(), &configmap).await?;
    secrets.create(&PostParams::default(), &secret).await?;
    let name = "configmap-secret-volumes";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "volumes": [
                {"name": "config", "configMap": {"name": "volume-config"}},
                {"name": "secret", "secret": {"secretName": "volume-secret"}}
            ],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "cat /config/config /secret/secret > /dev/termination-log"], "volumeMounts": [{"name": "config", "mountPath": "/config"}, {"name": "secret", "mountPath": "/secret"}] }]
        }),
    )
    .await?;
    context
        .wait_until("ConfigMap and Secret volume content", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(terminated_message(&context, name)
                    .await?
                    .is_some_and(|message| message.contains("config-value") && message.contains("secret-value")))
            }
        })
        .await
}

pub(super) async fn downward_api_volume_writes_pod_metadata(context: &E2eContext) -> Result<()> {
    let name = "downward-api-volume";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "volumes": [{"name": "downward", "downwardAPI": {"items": [{"path": "name", "fieldRef": {"fieldPath": "metadata.name"}}]}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "cat /downward/name > /dev/termination-log"], "volumeMounts": [{"name": "downward", "mountPath": "/downward"}] }]
        }),
    )
    .await?;
    context
        .wait_until("Downward API volume content", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(terminated_message(&context, name)
                    .await?
                    .is_some_and(|message| message.trim() == name))
            }
        })
        .await
}

pub(super) async fn projected_volume_merges_configmap_and_downward_api(
    context: &E2eContext,
) -> Result<()> {
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);
    let configmap: ConfigMap = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {"name": "projected-config"},
        "data": {"config": "projected-value"}
    }))?;
    configmaps.create(&PostParams::default(), &configmap).await?;
    let name = "projected-volume";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "volumes": [{"name": "projected", "projected": {"sources": [
                {"configMap": {"name": "projected-config"}},
                {"downwardAPI": {"items": [{"path": "name", "fieldRef": {"fieldPath": "metadata.name"}}]}}
            ]}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "cat /projected/config /projected/name > /dev/termination-log"], "volumeMounts": [{"name": "projected", "mountPath": "/projected"}] }]
        }),
    )
    .await?;
    context
        .wait_until("projected volume content", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(terminated_message(&context, name)
                    .await?
                    .is_some_and(|message| message.contains("projected-value") && message.contains(name)))
            }
        })
        .await
}

pub(super) async fn service_account_token_projected_volume_mints_a_token(
    context: &E2eContext,
) -> Result<()> {
    let name = "service-account-token-volume";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "volumes": [{"name": "token", "projected": {"sources": [{"serviceAccountToken": {"path": "token", "expirationSeconds": 3600}}]}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "wc -c < /token/token > /dev/termination-log"], "volumeMounts": [{"name": "token", "mountPath": "/token"}] }]
        }),
    )
    .await?;
    context
        .wait_until("projected service-account token", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(terminated_message(&context, name)
                    .await?
                    .and_then(|message| message.trim().parse::<usize>().ok())
                    .is_some_and(|length| length > 100))
            }
        })
        .await
}

pub(super) async fn host_aliases_are_written_to_etc_hosts(context: &E2eContext) -> Result<()> {
    let name = "host-aliases";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "hostAliases": [{"ip": "203.0.113.10", "hostnames": ["e2e-alias.test"]}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "grep e2e-alias.test /etc/hosts > /dev/termination-log"]}]
        }),
    )
    .await?;
    context
        .wait_until("hostAliases entry", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(terminated_message(&context, name)
                    .await?
                    .is_some_and(|message| message.contains("203.0.113.10") && message.contains("e2e-alias.test")))
            }
        })
        .await
}

pub(super) async fn empty_dir_memory_is_backed_by_tmpfs(context: &E2eContext) -> Result<()> {
    let name = "emptydir-memory";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "volumes": [{"name": "cache", "emptyDir": {"medium": "Memory"}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "grep ' /cache ' /proc/mounts > /dev/termination-log"], "volumeMounts": [{"name": "cache", "mountPath": "/cache"}] }]
        }),
    )
    .await?;
    context
        .wait_until("memory emptyDir mount", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(terminated_message(&context, name)
                    .await?
                    .is_some_and(|message| message.contains("tmpfs") && message.contains("/cache")))
            }
        })
        .await
}

pub(super) async fn configmap_volume_updates_live_without_pod_restart(
    context: &E2eContext,
) -> Result<()> {
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);
    let configmap: ConfigMap = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {"name": "live-volume-config"},
        "data": {"value": "old"}
    }))?;
    configmaps.create(&PostParams::default(), &configmap).await?;
    let name = "live-configmap-volume";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "volumes": [{"name": "config", "configMap": {"name": "live-volume-config"}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "while [ \"$(cat /config/value)\" != new ]; do sleep 1; done; cat /config/value > /dev/termination-log"], "volumeMounts": [{"name": "config", "mountPath": "/config"}] }]
        }),
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until("live ConfigMap Pod Running", Duration::from_secs(90), || {
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
    configmaps
        .patch(
            "live-volume-config",
            &PatchParams::default(),
            &Patch::Merge(&json!({"data": {"value": "new"}})),
        )
        .await?;
    context
        .wait_until("ConfigMap volume to refresh in place", Duration::from_secs(120), || {
            let context = context.clone();
            async move {
                Ok(terminated_message(&context, name)
                    .await?
                    .is_some_and(|message| message.trim() == "new"))
            }
        })
        .await
}

fn host_path_test_dir(label: &str) -> String {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("/tmp/nodebootstrap-e2e-{label}-{}-{suffix}", std::process::id())
}

pub(super) async fn host_path_directory_mounts_the_real_host_directory(
    context: &E2eContext,
) -> Result<()> {
    let host_path = host_path_test_dir("mount");
    std::fs::create_dir_all(&host_path)?;
    let marker = format!("{host_path}/marker");
    let name = "hostpath-directory";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "volumes": [{"name": "host", "hostPath": {"path": host_path, "type": "Directory"}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "echo host-path > /host/marker; echo done > /dev/termination-log"], "volumeMounts": [{"name": "host", "mountPath": "/host"}] }]
        }),
    )
    .await?;
    let wait_result = context
        .wait_until("hostPath Pod to finish", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(terminated_message(&context, name)
                    .await?
                    .is_some_and(|message| message.trim() == "done"))
            }
        })
        .await;
    let marker_exists = std::path::Path::new(&marker).is_file();
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_dir(&host_path);
    wait_result?;
    anyhow::ensure!(marker_exists, "hostPath did not write through to the host directory");
    Ok(())
}

pub(super) async fn host_path_directory_or_create_creates_missing_directory(
    context: &E2eContext,
) -> Result<()> {
    let host_path = host_path_test_dir("directory-or-create");
    let name = "hostpath-directory-or-create";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "volumes": [{"name": "host", "hostPath": {"path": host_path, "type": "DirectoryOrCreate"}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "test -d /host && echo created > /dev/termination-log"], "volumeMounts": [{"name": "host", "mountPath": "/host"}] }]
        }),
    )
    .await?;
    let wait_result = context
        .wait_until("DirectoryOrCreate hostPath Pod", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(terminated_message(&context, name)
                    .await?
                    .is_some_and(|message| message.trim() == "created"))
            }
        })
        .await;
    let directory_exists = std::path::Path::new(&host_path).is_dir();
    let _ = std::fs::remove_dir_all(&host_path);
    wait_result?;
    anyhow::ensure!(directory_exists, "DirectoryOrCreate did not create the host directory");
    Ok(())
}
