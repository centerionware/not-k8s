use super::context::E2eContext;
use super::skip_test;
use anyhow::Result;
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Secret};
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams};
use serde_json::json;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
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

pub(super) async fn host_aliases_still_work_under_host_users_false(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("hostUsers and hostAliases checks require the CRI runtime"));
    }
    let name = "hostaliases-userns";
    create_pod(
        context,
        name,
        json!({
            "hostUsers": false,
            "hostAliases": [{"ip": "10.1.2.3", "hostnames": ["custom.example.com"]}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "grep '10.1.2.3.*custom.example.com' /etc/hosts > /dev/termination-log"]}]
        }),
    )
    .await?;
    context
        .wait_until("hostAliases with hostUsers=false", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(terminated_message(&context, name)
                    .await?
                    .is_some_and(|message| message.contains("10.1.2.3")))
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

pub(super) async fn empty_dir_hugepages_is_backed_by_hugetlbfs(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("HugePages emptyDir checks require the CRI runtime"));
    }
    let reserved = std::fs::read_to_string("/proc/sys/vm/nr_hugepages")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or_default();
    if reserved == 0 {
        return Err(skip_test(
            "no hugepages are reserved on this node; HugePages emptyDir cannot be mounted",
        ));
    }
    let name = "empty-dir-hugepages";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "volumes": [{"name": "hugepool", "emptyDir": {"medium": "HugePages-2Mi", "sizeLimit": "4Mi"}}],
            "containers": [{"name": "app", "image": "busybox:latest", "resources": {"limits": {"hugepages-2Mi": "4Mi", "memory": "67108864"}}, "command": ["stat", "-f", "-c", "%T", "/huge"], "volumeMounts": [{"name": "hugepool", "mountPath": "/huge"}]}]
        }),
    )
    .await?;
    let result = async {
        context
            .wait_until("HugePages emptyDir to terminate", Duration::from_secs(90), || {
                let context = context.clone();
                async move { Ok(terminated_message(&context, name).await?.is_some()) }
            })
            .await?;
        let filesystem = terminated_message(context, name)
            .await?
            .unwrap_or_default();
        anyhow::ensure!(
            filesystem.trim() == "hugetlbfs",
            "HugePages emptyDir was backed by {}, not hugetlbfs",
            filesystem.trim()
        );
        Ok(())
    }
    .await;
    let _ = pods.delete(name, &DeleteParams::default()).await;
    result
}

pub(super) async fn image_volume_source_mounts_a_read_only_image(
    context: &E2eContext,
) -> Result<()> {
    if let Err(error) = crate::config::Config::from_env().and_then(|config| {
        anyhow::ensure!(
            config.nodelet_runtime() == "cri",
            "image volumes require the CRI runtime",
        );
        Ok(())
    }) {
        return Err(skip_test(error.to_string()));
    }
    let name = "image-volume-check";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "volumes": [{"name": "img", "image": {"reference": "busybox:latest", "pullPolicy": "IfNotPresent"}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "test -x /img/bin/sh && (echo x > /img/should-fail-readonly 2>/dev/null && echo WROTE || echo BLOCKED) > /dev/termination-log"], "volumeMounts": [{"name": "img", "mountPath": "/img"}]}]
        }),
    )
    .await?;
    context
        .wait_until("image volume Pod to report its read-only mount", Duration::from_secs(120), || {
            let context = context.clone();
            async move {
                Ok(terminated_message(&context, name)
                    .await?
                    .is_some_and(|message| message.trim() == "BLOCKED"))
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

pub(super) async fn sub_path_expr_expands_a_downward_api_env_var(
    context: &E2eContext,
) -> Result<()> {
    let name = "subpath-expr";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "volumes": [{"name": "shared", "emptyDir": {}}],
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "env": [{"name": "POD_NAME", "valueFrom": {"fieldRef": {"fieldPath": "metadata.name"}}}],
                "command": ["sh", "-c", "echo expanded > /data/marker; cat /base/$(POD_NAME)/marker > /dev/termination-log"],
                "volumeMounts": [
                    {"name": "shared", "mountPath": "/base"},
                    {"name": "shared", "mountPath": "/data", "subPathExpr": "$(POD_NAME)"}
                ]
            }]
        }),
    )
    .await?;
    context
        .wait_until("subPathExpr expansion", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(terminated_message(&context, name)
                    .await?
                    .is_some_and(|message| message.trim() == "expanded"))
            }
        })
        .await
}

async fn container_waiting_reason(context: &E2eContext, name: &str) -> Result<Option<String>> {
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
        .and_then(|state| state.waiting)
        .and_then(|waiting| waiting.reason))
}

pub(super) async fn host_path_directory_type_rejects_a_nonexistent_path(
    context: &E2eContext,
) -> Result<()> {
    let host_path = host_path_test_dir("missing-directory");
    let name = "hostpath-missing-directory";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "volumes": [{"name": "host", "hostPath": {"path": host_path, "type": "Directory"}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "30"], "volumeMounts": [{"name": "host", "mountPath": "/host"}] }]
        }),
    )
    .await?;
    let wait_result = context
        .wait_until("missing Directory hostPath rejection", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(container_waiting_reason(&context, name)
                    .await?
                    .as_deref()
                    == Some("CreateContainerConfigError"))
            }
        })
        .await;
    let _ = std::fs::remove_dir_all(&host_path);
    wait_result
}

pub(super) async fn mount_propagation_host_to_container_still_mounts_normally(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("mount propagation checks require the CRI runtime"));
    }
    let host_path = host_path_test_dir("mount-propagation");
    std::fs::create_dir_all(&host_path)?;
    std::fs::write(format!("{host_path}/marker"), "written-by-the-host")?;
    let from_container = format!("{host_path}/from-container");
    let name = "mount-propagation-check";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let create_result = create_pod(
        context,
        name,
        json!({
            "volumes": [{"name": "host", "hostPath": {"path": host_path, "type": "Directory"}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "cat /host/marker > /host/from-container; sleep 3600"], "volumeMounts": [{"name": "host", "mountPath": "/host", "mountPropagation": "HostToContainer"}]}]
        }),
    )
    .await;
    if let Err(error) = create_result {
        let _ = std::fs::remove_dir_all(&host_path);
        return Err(error);
    }
    let result = async {
        context
            .wait_until("mountPropagation Pod Running", Duration::from_secs(90), || {
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
        context
            .wait_until("mountPropagation container write", Duration::from_secs(40), || {
                let from_container = from_container.clone();
                async move { Ok(Path::new(&from_container).is_file()) }
            })
            .await?;
        let content = std::fs::read_to_string(&from_container)?;
        anyhow::ensure!(
            content.contains("written-by-the-host"),
            "mountPropagation Pod could not read the hostPath marker"
        );
        Ok(())
    }
    .await;
    let _ = pods.delete(name, &DeleteParams::default()).await;
    let _ = std::fs::remove_dir_all(&host_path);
    result
}

pub(super) async fn recursive_read_only_still_mounts_read_only_normally(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("recursive read-only checks require the CRI runtime"));
    }
    let host_path = host_path_test_dir("recursive-readonly");
    std::fs::create_dir_all(&host_path)?;
    let name = "recursive-readonly-check";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let create_result = create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "volumes": [{"name": "host", "hostPath": {"path": host_path, "type": "Directory"}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "if touch /host/roottest 2>/dev/null; then echo writable > /dev/termination-log; else echo readonly > /dev/termination-log; fi"], "volumeMounts": [{"name": "host", "mountPath": "/host", "readOnly": true, "recursiveReadOnly": "Enabled"}]}]
        }),
    )
    .await;
    if let Err(error) = create_result {
        let _ = std::fs::remove_dir_all(&host_path);
        return Err(error);
    }
    let result = context
        .wait_until("recursiveReadOnly Pod to report readonly", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(terminated_message(&context, name)
                    .await?
                    .is_some_and(|message| message.trim() == "readonly"))
            }
        })
        .await;
    let _ = pods.delete(name, &DeleteParams::default()).await;
    let _ = std::fs::remove_dir_all(&host_path);
    result
}

pub(super) async fn fsgroup_never_applies_to_hostpath_volumes(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("hostPath fsGroup checks require the CRI runtime"));
    }
    let host_path = host_path_test_dir("fsgroup-hostpath");
    std::fs::create_dir_all(&host_path)?;
    let original_gid = std::fs::metadata(&host_path)?.gid();
    let name = "fsgroup-hostpath";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let create_result = create_pod(
        context,
        name,
        json!({
            "securityContext": {"fsGroup": 4322},
            "volumes": [{"name": "host", "hostPath": {"path": host_path, "type": "Directory"}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"], "volumeMounts": [{"name": "host", "mountPath": "/host"}]}]
        }),
    )
    .await;
    if let Err(error) = create_result {
        let _ = std::fs::remove_dir_all(&host_path);
        return Err(error);
    }
    let result = async {
        context
            .wait_until("hostPath fsGroup Pod Running", Duration::from_secs(90), || {
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
        tokio::time::sleep(Duration::from_secs(2)).await;
        let gid = std::fs::metadata(&host_path)?.gid();
        anyhow::ensure!(
            gid == original_gid,
            "fsGroup changed hostPath directory gid from {original_gid} to {gid}"
        );
        Ok(())
    };
    let result = result.await;
    let _ = pods.delete(name, &kube::api::DeleteParams::default()).await;
    let _ = std::fs::remove_dir_all(&host_path);
    result
}

pub(super) async fn fsgroup_chowns_materialized_volumes(context: &E2eContext) -> Result<()> {
    let name = "fsgroup-emptydir";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "securityContext": {"fsGroup": 2000},
            "volumes": [{"name": "data", "emptyDir": {}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "stat -c '%u:%g' /data > /dev/termination-log"], "volumeMounts": [{"name": "data", "mountPath": "/data"}]}]
        }),
    )
    .await?;
    context
        .wait_until("fsGroup ownership on emptyDir", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(terminated_message(&context, name)
                    .await?
                    .is_some_and(|message| message.trim() == "0:2000"))
            }
        })
        .await
}
