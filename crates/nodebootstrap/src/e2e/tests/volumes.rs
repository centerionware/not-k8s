use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{ConfigMap, Node, Pod, Secret, ServiceAccount};
use kube::api::{Api, AttachParams, DeleteParams, Patch, PatchParams, PostParams};
use serde_json::json;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use tokio::io::AsyncReadExt;

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

fn privileged_available() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "0")
        || Command::new("sudo")
            .args(["-n", "true"])
            .status()
            .is_ok_and(|status| status.success())
}

fn run_privileged(program: &str, args: &[&str]) -> Result<()> {
    let uid = Command::new("id").arg("-u").output()?;
    let mut command = if String::from_utf8_lossy(&uid.stdout).trim() == "0" {
        let mut command = Command::new(program);
        command.args(args);
        command
    } else {
        let mut command = Command::new("sudo");
        command.arg(program).args(args);
        command
    };
    let output = command.output()?;
    anyhow::ensure!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

async fn exec_in_pod(context: &E2eContext, pod: &str, args: &[&str]) -> Result<(bool, String)> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let params = AttachParams::default()
        .container("app")
        .stdout(true)
        .stderr(false);
    let mut process = pods.exec(pod, args.iter().copied(), &params).await?;
    let status = process.take_status();
    let mut stdout = Vec::new();
    if let Some(mut stream) = process.stdout() {
        stream.read_to_end(&mut stdout).await?;
    }
    process.join().await?;
    let succeeded = match status {
        Some(status) => status.await.is_some_and(|status| {
            serde_json::to_value(status).ok().is_some_and(|status| {
                status.get("status").and_then(serde_json::Value::as_str) == Some("Success")
            })
        }),
        None => true,
    };
    Ok((succeeded, String::from_utf8_lossy(&stdout).into_owned()))
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

/// A required projected token must keep the Pod Pending when its
/// ServiceAccount is temporarily unavailable. The Pod is admitted while the
/// account exists, then nodelet sees the account disappear on its first
/// reconcile. Recreating the account and changing the Pod proves recovery
/// from the failed TokenRequest without ever starting the container without
/// its required token.
pub(super) async fn projected_service_account_token_waits_for_service_account(
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

    let service_account = ServiceAccount {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(service_account_name.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    service_accounts
        .create(&PostParams::default(), &service_account)
        .await?;

    run_privileged("systemctl", &["stop", "nodelet.service"])?;
    let result = async {
        create_pod(
            context,
            &pod_name,
            json!({
                "serviceAccountName": service_account_name,
                "restartPolicy": "Never",
                "containers": [{
                    "name": "app",
                    "image": "busybox:latest",
                    "command": ["sh", "-c", "test -s /var/run/secrets/tokens/api-token && sleep 3600"]
                }],
                "volumes": [{
                    "name": "api-token",
                    "projected": {"sources": [{
                        "serviceAccountToken": {"path": "api-token", "expirationSeconds": 600}
                    }]}
                }]
            }),
        )
        .await?;
        service_accounts
            .delete(&service_account_name, &DeleteParams::default())
            .await?;
        context
            .wait_until("temporary ServiceAccount deletion", Duration::from_secs(30), || {
                let service_accounts = service_accounts.clone();
                let service_account_name = service_account_name.clone();
                async move { Ok(service_accounts.get_opt(&service_account_name).await?.is_none()) }
            })
            .await?;
        run_privileged("systemctl", &["start", "nodelet.service"])?;
        context
            .wait_until("Pod to remain Pending while its ServiceAccount is absent", Duration::from_secs(180), || {
                let pods = pods.clone();
                let pod_name = pod_name.clone();
                async move {
                    let pod = pods.get(&pod_name).await?;
                    Ok(pod
                        .status
                        .as_ref()
                        .and_then(|status| status.phase.as_deref())
                        == Some("Pending")
                        && pod.status.as_ref().and_then(|status| status.message.as_deref()).is_some_and(
                            |message| message.starts_with("waiting for projected ServiceAccount token(s)"),
                        ))
                }
            })
            .await?;

        service_accounts
            .create(&PostParams::default(), &service_account)
            .await?;
        pods.patch(
            &pod_name,
            &PatchParams::default(),
            &Patch::Merge(json!({
                "metadata": {"annotations": {"e2e.not-k8s.dev/service-account-recovered": "true"}}
            })),
        )
        .await?;
        context
            .wait_until("Pod to start after its ServiceAccount is restored", Duration::from_secs(60), || {
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
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let started = run_privileged("systemctl", &["start", "nodelet.service"]);
    result?;
    started?;
    let (succeeded, token) = exec_in_pod(
        context,
        &pod_name,
        &["cat", "/var/run/secrets/tokens/api-token"],
    )
    .await
    .context("reading the projected ServiceAccount token")?;
    anyhow::ensure!(
        succeeded && !token.trim().is_empty(),
        "Pod started without a projected ServiceAccount token"
    );
    Ok(())
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
            "restartPolicy": "Never",
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
            "containers": [{"name": "app", "image": "busybox:latest", "resources": {"limits": {"hugepages-2Mi": "4Mi", "memory": "67108864"}}, "command": ["sh", "-c", "sleep 3600"], "volumeMounts": [{"name": "hugepool", "mountPath": "/huge"}]}]
        }),
    )
    .await?;
    let result = async {
        context
            .wait_until("HugePages emptyDir Pod to run", Duration::from_secs(90), || {
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
        let pod_uid = pods
            .get(name)
            .await?
            .metadata
            .uid
            .context("HugePages emptyDir Pod has no UID")?;
        let volume_path = std::path::PathBuf::from("/var/lib/nodelet/pods")
            .join(pod_uid)
            .join("volumes/hugepool");
        context
            .wait_until("HugePages emptyDir host volume", Duration::from_secs(30), || {
                let volume_path = volume_path.clone();
                async move { Ok(volume_path.is_dir()) }
            })
            .await?;
        let stat = Command::new("stat")
            .args(["-f", "-c", "%T"])
            .arg(&volume_path)
            .output()
            .context("checking HugePages emptyDir filesystem")?;
        anyhow::ensure!(
            stat.status.success(),
            "stat failed for HugePages emptyDir host volume {}: {}",
            volume_path.display(),
            String::from_utf8_lossy(&stat.stderr).trim()
        );
        let filesystem = String::from_utf8_lossy(&stat.stdout);
        anyhow::ensure!(
            filesystem.trim() == "hugetlbfs",
            "HugePages emptyDir host volume {} was backed by {}, not hugetlbfs",
            volume_path.display(),
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
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "volumes": [
                {"name": "img", "image": {"reference": "busybox:latest", "pullPolicy": "IfNotPresent"}},
                {"name": "shared", "emptyDir": {}}
            ],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "ls -A /img > /shared/listing.txt 2>&1; (echo x > /img/should-fail-readonly 2>/shared/write.err && echo WROTE > /shared/write.result) || echo BLOCKED > /shared/write.result; sleep 3600"], "volumeMounts": [{"name": "img", "mountPath": "/img"}, {"name": "shared", "mountPath": "/shared"}]}]
        }),
    )
    .await?;
    context
        .wait_until("image volume Pod Running", Duration::from_secs(120), || {
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
    let pod_uid = pods
        .get(name)
        .await?
        .metadata
        .uid
        .context("image-volume Pod has no UID")?;
    let shared = std::path::PathBuf::from("/var/lib/nodelet/pods")
        .join(pod_uid)
        .join("volumes/shared");
    context
        .wait_until("image volume listing", Duration::from_secs(30), || {
            let path = shared.join("listing.txt");
            async move { Ok(std::fs::read_to_string(path).is_ok_and(|value| !value.trim().is_empty())) }
        })
        .await?;
    context
        .wait_until("image volume read-only result", Duration::from_secs(30), || {
            let path = shared.join("write.result");
            async move { Ok(std::fs::read_to_string(path).is_ok_and(|value| !value.trim().is_empty())) }
        })
        .await?;
    let listing = std::fs::read_to_string(shared.join("listing.txt")).unwrap_or_default();
    let write_result = std::fs::read_to_string(shared.join("write.result")).unwrap_or_default();
    anyhow::ensure!(!listing.trim().is_empty(), "image volume mount was empty");
    anyhow::ensure!(
        write_result.trim() == "BLOCKED",
        "writing inside an image volume mount returned {:?}, expected BLOCKED",
        write_result.trim()
    );
    let _ = pods.delete(name, &DeleteParams::default()).await;
    Ok(())
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

pub(super) async fn mount_propagation_host_to_container_sees_a_new_host_mount(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("mount propagation checks require the CRI runtime"));
    }
    if !privileged_available() {
        return Err(skip_test("host mount propagation checks require root or passwordless sudo"));
    }
    let host_path = host_path_test_dir("mount-propagation-h2c");
    let source_path = host_path_test_dir("mount-propagation-source");
    let nested_path = format!("{host_path}/newmount");
    std::fs::create_dir_all(&nested_path)?;
    std::fs::create_dir_all(&source_path)?;
    std::fs::write(format!("{source_path}/marker"), "from-a-new-host-mount")?;
    let name = "mount-propagation-h2c-check";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    create_pod(
        context,
        name,
        json!({
            "volumes": [{"name": "host", "hostPath": {"path": host_path, "type": "Directory"}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"], "volumeMounts": [{"name": "host", "mountPath": "/hostvol", "mountPropagation": "HostToContainer"}]}]
        }),
    )
    .await?;
    let result = async {
        context
            .wait_until("HostToContainer propagation Pod", Duration::from_secs(90), || {
                let pods = pods.clone();
                async move { Ok(pods.get(name).await?.status.and_then(|status| status.phase).as_deref() == Some("Running")) }
            })
            .await?;
        run_privileged("mount", &["--bind", &source_path, &nested_path])
            .map_err(|error| skip_test(format!("host bind mount unavailable: {error}")))?;
        context
            .wait_until("new host mount to propagate into the Pod", Duration::from_secs(30), || {
                async move {
                    let (succeeded, output) = exec_in_pod(context, name, &["cat", "/hostvol/newmount/marker"]).await?;
                    Ok(succeeded && output.contains("from-a-new-host-mount"))
                }
            })
            .await
    }
    .await;
    let _ = run_privileged("umount", &[&nested_path]);
    let _ = pods.delete(name, &DeleteParams::default()).await;
    let _ = std::fs::remove_dir_all(&host_path);
    let _ = std::fs::remove_dir_all(&source_path);
    result
}

pub(super) async fn mount_propagation_private_default_does_not_see_a_new_host_mount(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("mount propagation checks require the CRI runtime"));
    }
    if !privileged_available() {
        return Err(skip_test("host mount propagation checks require root or passwordless sudo"));
    }
    let host_path = host_path_test_dir("mount-propagation-private");
    let source_path = host_path_test_dir("mount-propagation-private-source");
    let nested_path = format!("{host_path}/newmount");
    std::fs::create_dir_all(&nested_path)?;
    std::fs::create_dir_all(&source_path)?;
    std::fs::write(format!("{source_path}/marker"), "from-a-new-host-mount")?;
    let name = "mount-propagation-private-check";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    create_pod(
        context,
        name,
        json!({
            "volumes": [{"name": "host", "hostPath": {"path": host_path, "type": "Directory"}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"], "volumeMounts": [{"name": "host", "mountPath": "/hostvol"}]}]
        }),
    )
    .await?;
    let result = async {
        context
            .wait_until("Private propagation Pod", Duration::from_secs(90), || {
                let pods = pods.clone();
                async move { Ok(pods.get(name).await?.status.and_then(|status| status.phase).as_deref() == Some("Running")) }
            })
            .await?;
        run_privileged("mount", &["--bind", &source_path, &nested_path])
            .map_err(|error| skip_test(format!("host bind mount unavailable: {error}")))?;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let (_, output) = exec_in_pod(context, name, &["cat", "/hostvol/newmount/marker"]).await?;
        anyhow::ensure!(!output.contains("from-a-new-host-mount"), "Private mount propagation leaked a host-side mount into the container");
        Ok(())
    }
    .await;
    let _ = run_privileged("umount", &[&nested_path]);
    let _ = pods.delete(name, &DeleteParams::default()).await;
    let _ = std::fs::remove_dir_all(&host_path);
    let _ = std::fs::remove_dir_all(&source_path);
    result
}

pub(super) async fn recursive_read_only_enabled_blocks_writes_in_a_nested_mount_too(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("recursive read-only checks require the CRI runtime"));
    }
    if !privileged_available() {
        return Err(skip_test("nested recursive read-only checks require root or passwordless sudo"));
    }
    let host_path = host_path_test_dir("recursive-readonly-nested");
    let source_path = host_path_test_dir("recursive-readonly-nested-source");
    let nested_path = format!("{host_path}/nested");
    std::fs::create_dir_all(&nested_path)?;
    std::fs::create_dir_all(&source_path)?;
    run_privileged("mount", &["--bind", &source_path, &nested_path])
        .map_err(|error| skip_test(format!("nested host bind mount unavailable: {error}")))?;
    let name = "recursive-readonly-nested-check";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    create_pod(
        context,
        name,
        json!({
            "volumes": [{"name": "host", "hostPath": {"path": host_path, "type": "Directory"}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"], "volumeMounts": [{"name": "host", "mountPath": "/hostvol", "readOnly": true, "recursiveReadOnly": "Enabled"}]}]
        }),
    )
    .await?;
    let result = async {
        context
            .wait_until("recursiveReadOnly nested Pod", Duration::from_secs(90), || {
                let pods = pods.clone();
                async move { Ok(pods.get(name).await?.status.and_then(|status| status.phase).as_deref() == Some("Running")) }
            })
            .await?;
        let (succeeded, _) = exec_in_pod(context, name, &["touch", "/hostvol/nested/test"]).await?;
        anyhow::ensure!(!succeeded, "recursiveReadOnly allowed a write inside the nested mount");
        Ok(())
    }
    .await;
    let _ = run_privileged("umount", &[&nested_path]);
    let _ = pods.delete(name, &DeleteParams::default()).await;
    let _ = std::fs::remove_dir_all(&host_path);
    let _ = std::fs::remove_dir_all(&source_path);
    result
}

async fn recursive_read_only_if_possible(
    context: &E2eContext,
    name: &str,
    require_capability_match: bool,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("recursive read-only checks require the CRI runtime"));
    }
    let nodes: Api<Node> = Api::all(context.client.clone());
    let node = nodes
        .list(&Default::default())
        .await?
        .items
        .into_iter()
        .next()
        .context("cluster has no Node")?;
    let node_value = serde_json::to_value(node)?;
    let capability = node_value
        .pointer("/status/runtimeHandlers/0/features/recursiveReadOnlyMounts")
        .and_then(serde_json::Value::as_bool);
    if require_capability_match && capability.is_none() {
        return Err(skip_test("runtime handler did not advertise a boolean recursiveReadOnlyMounts capability"));
    }
    let host_path = host_path_test_dir(name);
    std::fs::create_dir_all(&host_path)?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    create_pod(
        context,
        name,
        json!({"volumes": [{"name": "host", "hostPath": {"path": host_path, "type": "Directory"}}], "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"], "volumeMounts": [{"name": "host", "mountPath": "/host", "readOnly": true, "recursiveReadOnly": "IfPossible"}]}]}),
    )
    .await?;
    let result = async {
        context
            .wait_until("recursiveReadOnly IfPossible Pod", Duration::from_secs(90), || {
                let pods = pods.clone();
                async move { Ok(pods.get(name).await?.status.and_then(|status| status.phase).as_deref() == Some("Running")) }
            })
            .await?;
        let pod_value = serde_json::to_value(pods.get(name).await?)?;
        let got = pod_value
            .pointer("/status/containerStatuses/0/volumeMounts")
            .and_then(serde_json::Value::as_array)
            .and_then(|mounts| mounts.iter().find(|mount| mount.get("name").and_then(serde_json::Value::as_str) == Some("host")))
            .and_then(|mount| mount.get("recursiveReadOnly").and_then(serde_json::Value::as_str))
            .context("IfPossible status omitted recursiveReadOnly")?;
        anyhow::ensure!(got == "Enabled" || got == "Disabled", "IfPossible reported invalid value {got}");
        if let Some(capability) = capability {
            let expected = if capability { "Enabled" } else { "Disabled" };
            anyhow::ensure!(got == expected, "IfPossible reported {got}, but runtime handler advertised {expected}");
        }
        Ok(())
    }
    .await;
    let _ = pods.delete(name, &DeleteParams::default()).await;
    let _ = std::fs::remove_dir_all(&host_path);
    result
}

pub(super) async fn recursive_read_only_if_possible_falls_back_without_erroring(
    context: &E2eContext,
) -> Result<()> {
    recursive_read_only_if_possible(context, "recursive-readonly-ifpossible-check", false).await
}

pub(super) async fn recursive_read_only_if_possible_tracks_the_runtime_handlers_own_capability(
    context: &E2eContext,
) -> Result<()> {
    recursive_read_only_if_possible(context, "recursive-readonly-capability-check", true).await
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
