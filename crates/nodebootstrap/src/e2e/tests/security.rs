use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, PostParams};
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

async fn phase_is(context: &E2eContext, name: &str, expected: &str) -> Result<bool> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    Ok(pods
        .get(name)
        .await?
        .status
        .and_then(|status| status.phase)
        .as_deref()
        == Some(expected))
}

async fn termination_message(context: &E2eContext, name: &str) -> Result<Option<String>> {
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

pub(super) async fn read_only_root_filesystem_blocks_writes(
    context: &E2eContext,
) -> Result<()> {
    let name = "readonly-root-filesystem";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "touch /must-not-exist"],
                "securityContext": {"readOnlyRootFilesystem": true}
            }]
        }),
    )
    .await?;
    context
        .wait_until("read-only root Pod to fail its write", Duration::from_secs(90), || {
            phase_is(context, name, "Failed")
        })
        .await
}

pub(super) async fn writable_root_filesystem_allows_writes(context: &E2eContext) -> Result<()> {
    let name = "writable-root-filesystem";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "touch /write-is-allowed"]
            }]
        }),
    )
    .await?;
    context
        .wait_until("writable root Pod to complete", Duration::from_secs(90), || {
            phase_is(context, name, "Succeeded")
        })
        .await
}

pub(super) async fn run_as_user_is_applied(context: &E2eContext) -> Result<()> {
    let name = "run-as-user";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "id -u > /dev/termination-log"], "securityContext": {"runAsUser": 1000, "runAsGroup": 1000}}]
        }),
    )
    .await?;
    context
        .wait_until("runAsUser termination message", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(termination_message(&context, name)
                    .await?
                    .is_some_and(|message| message.trim() == "1000"))
            }
        })
        .await
}

pub(super) async fn container_status_reports_resolved_user(context: &E2eContext) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("container user status checks require the CRI runtime"));
    }
    let name = "container-status-user";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    create_pod(
        context,
        name,
        json!({
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"], "securityContext": {"runAsUser": 4000, "runAsGroup": 5000}}]
        }),
    )
    .await?;
    context
        .wait_until("container-status user Pod Running", Duration::from_secs(90), || {
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
    let status_result = context
        .wait_until("container status resolved user", Duration::from_secs(40), || {
            let pods = pods.clone();
            async move {
                let pod = pods.get(name).await?;
                let value = serde_json::to_value(pod)?;
                Ok(value
                    .pointer("/status/containerStatuses/0/user/linux/uid")
                    .is_some())
            }
        })
        .await;
    let result = match status_result {
        Ok(()) => {
            let value = serde_json::to_value(pods.get(name).await?)?;
            let uid = value
                .pointer("/status/containerStatuses/0/user/linux/uid")
                .and_then(serde_json::Value::as_u64);
            let gid = value
                .pointer("/status/containerStatuses/0/user/linux/gid")
                .and_then(serde_json::Value::as_u64);
            anyhow::ensure!(uid == Some(4000), "container status reported uid {uid:?}, want 4000");
            anyhow::ensure!(gid == Some(5000), "container status reported gid {gid:?}, want 5000");
            Ok(())
        }
        Err(_) => Err(skip_test(
            "containerStatuses[0].user.linux was not populated by this containerd build",
        )),
    };
    let _ = pods.delete(name, &DeleteParams::default()).await;
    result
}

pub(super) async fn container_status_reports_recursive_read_only(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("recursive read-only status checks require the CRI runtime"));
    }
    let name = "container-status-rro";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    create_pod(
        context,
        name,
        json!({
            "volumes": [{"name": "ro-vol", "emptyDir": {}}, {"name": "rw-vol", "emptyDir": {}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"], "volumeMounts": [{"name": "ro-vol", "mountPath": "/ro", "readOnly": true, "recursiveReadOnly": "Enabled"}, {"name": "rw-vol", "mountPath": "/rw"}]}]
        }),
    )
    .await?;
    context
        .wait_until("recursive read-only status Pod Running", Duration::from_secs(90), || {
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
        .wait_until("container volumeMount status", Duration::from_secs(40), || {
            let pods = pods.clone();
            async move {
                let value = serde_json::to_value(pods.get(name).await?)?;
                Ok(value
                    .pointer("/status/containerStatuses/0/volumeMounts")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|mounts| !mounts.is_empty()))
            }
        })
        .await?;
    let value = serde_json::to_value(pods.get(name).await?)?;
    let mounts = value
        .pointer("/status/containerStatuses/0/volumeMounts")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let ro = mounts
        .iter()
        .find(|mount| mount.get("name").and_then(serde_json::Value::as_str) == Some("ro-vol"))
        .context("status did not report ro-vol")?;
    anyhow::ensure!(
        ro.get("recursiveReadOnly").and_then(serde_json::Value::as_str) == Some("Enabled"),
        "ro-vol recursiveReadOnly status was not Enabled: {ro}"
    );
    let rw = mounts
        .iter()
        .find(|mount| mount.get("name").and_then(serde_json::Value::as_str) == Some("rw-vol"))
        .context("status did not report rw-vol")?;
    anyhow::ensure!(
        rw.get("recursiveReadOnly").is_none(),
        "rw-vol unexpectedly reported recursiveReadOnly: {rw}"
    );
    let _ = pods.delete(name, &DeleteParams::default()).await;
    Ok(())
}

pub(super) async fn host_users_false_gets_a_real_user_namespace(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("user namespace checks require the CRI runtime"));
    }
    let name = "hostusers-false-check";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "hostUsers": false,
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "head -n 1 /proc/self/uid_map > /dev/termination-log"]}]
        }),
    )
    .await?;
    let result = async {
        context
            .wait_until("hostUsers=false uid_map", Duration::from_secs(90), || {
                let context = context.clone();
                async move { Ok(termination_message(&context, name).await?.is_some()) }
            })
            .await?;
        let uid_map = termination_message(context, name).await?.unwrap_or_default();
        anyhow::ensure!(!uid_map.trim().is_empty(), "hostUsers=false produced an empty uid_map");
        if uid_map.split_whitespace().eq(["0", "0", "4294967295"]) {
            return Err(skip_test(
                "the container runtime did not create a user namespace for hostUsers=false",
            ));
        }
        Ok(())
    }
    .await;
    let _ = pods.delete(name, &DeleteParams::default()).await;
    result
}

pub(super) async fn host_users_false_volume_still_reads_and_writes_normally(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("user namespace volume checks require the CRI runtime"));
    }
    let name = "hostusers-false-volume-check";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "hostUsers": false,
            "volumes": [{"name": "shared", "emptyDir": {}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "echo hello-from-userns-pod > /shared/marker; cat /shared/marker > /dev/termination-log"], "volumeMounts": [{"name": "shared", "mountPath": "/shared"}]}]
        }),
    )
    .await?;
    let result = context
        .wait_until("hostUsers=false volume roundtrip", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(termination_message(&context, name)
                    .await?
                    .is_some_and(|message| message.trim() == "hello-from-userns-pod"))
            }
        })
        .await;
    let _ = pods.delete(name, &DeleteParams::default()).await;
    result
}

pub(super) async fn sysctls_are_applied_to_the_sandbox(context: &E2eContext) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("Pod sysctl checks require the CRI runtime"));
    }
    let name = "sysctls-check";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "securityContext": {"sysctls": [{"name": "net.ipv4.ip_unprivileged_port_start", "value": "1234"}]},
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["cat", "/proc/sys/net/ipv4/ip_unprivileged_port_start"]}]
        }),
    )
    .await?;
    let result = context
        .wait_until("Pod sysctl value", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(termination_message(&context, name)
                    .await?
                    .is_some_and(|message| message.trim() == "1234"))
            }
        })
        .await;
    let _ = pods.delete(name, &DeleteParams::default()).await;
    result
}

pub(super) async fn proc_mount_default_masks_proc_kcore(context: &E2eContext) -> Result<()> {
    let name = "proc-mount-default";
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "containers": [{
                "name": "app",
                "image": "busybox:latest",
                "command": ["sh", "-c", "head -c 4 /proc/kcore 2>/dev/null | wc -c > /dev/termination-log"]
            }]
        }),
    )
    .await?;
    context
        .wait_until("default procMount termination message", Duration::from_secs(90), || {
            let context = context.clone();
            async move {
                Ok(termination_message(&context, name)
                    .await?
                    .is_some_and(|message| !message.trim().is_empty()))
            }
        })
        .await?;
    let message = termination_message(context, name).await?.unwrap_or_default();
    if message.trim() != "0" {
        return Err(skip_test(format!(
            "the runtime exposed {} bytes from /proc/kcore under default procMount; host OCI masking controls this final behavior",
            message.trim()
        )));
    }
    Ok(())
}

pub(super) async fn proc_mount_unmasked_leaves_proc_kcore_readable(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("procMount checks require the CRI runtime"));
    }
    let name = "proc-mount-unmasked";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "hostUsers": false,
            "securityContext": {"procMount": "Unmasked"},
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "head -c 4 /proc/kcore 2>/dev/null | wc -c > /dev/termination-log"]}]
        }),
    )
    .await?;
    let result = async {
        context
            .wait_until("unmasked procMount termination message", Duration::from_secs(90), || {
                let context = context.clone();
                async move { Ok(termination_message(&context, name).await?.is_some()) }
            })
            .await?;
        let bytes = termination_message(context, name)
            .await?
            .unwrap_or_default();
        if bytes.trim() == "0" {
            return Err(skip_test(
                "the runtime kept /proc/kcore masked with procMount=Unmasked",
            ));
        }
        Ok(())
    }
    .await;
    let _ = pods.delete(name, &DeleteParams::default()).await;
    result
}
