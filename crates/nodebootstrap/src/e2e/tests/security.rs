use super::context::E2eContext;
use super::resource_managers::NodeletEnvOverride;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{ConfigMap, Pod};
use kube::api::{Api, DeleteParams, PostParams};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::process::Command;
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

fn run_command(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
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

pub(super) async fn host_users_volume_ownership_translation_is_correct(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("user namespace ownership checks require the CRI runtime"));
    }
    let name = "hostusers-ownership";
    let host_dir = std::env::temp_dir().join(format!(
        "nodebootstrap-hostusers-ownership-{}",
        std::process::id()
    ));
    fs::create_dir_all(&host_dir)?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    create_pod(
        context,
        name,
        json!({
            "restartPolicy": "Never",
            "hostUsers": false,
            "securityContext": {"runAsUser": 2000, "runAsGroup": 3000},
            "volumes": [{"name": "host", "hostPath": {"path": host_dir, "type": "Directory"}}],
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "touch /host/marker && stat -c %u /host/marker > /dev/termination-log"], "volumeMounts": [{"name": "host", "mountPath": "/host"}]}]
        }),
    )
    .await?;
    let result = context
        .wait_until("hostUsers ownership translation", Duration::from_secs(120), || {
            let context = context.clone();
            async move {
                Ok(termination_message(&context, name)
                    .await?
                    .is_some_and(|message| message.trim() == "2000"))
            }
        })
        .await;
    let _ = pods.delete(name, &DeleteParams::default()).await;
    let _ = fs::remove_dir_all(&host_dir);
    result
}

pub(super) async fn supplemental_groups_policy_strict_ignores_image_group_membership(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("supplemental-groups checks require the CRI runtime"));
    }
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);
    let mut data = BTreeMap::new();
    data.insert(
        "passwd".to_string(),
        "testuser:x:2000:3000::/home:/bin/sh\n".to_string(),
    );
    data.insert(
        "group".to_string(),
        "root:x:0:\nimagegroup:x:4000:testuser\nusers:x:5000:\n".to_string(),
    );
    let configmap: ConfigMap = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {"name": "supplemental-groups-files"},
        "data": data
    }))?;
    configmaps.create(&PostParams::default(), &configmap).await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    for (name, policy) in [
        ("supplemental-groups-merge", "Merge"),
        ("supplemental-groups-strict", "Strict"),
    ] {
        create_pod(
            context,
            name,
            json!({
                "restartPolicy": "Never",
                "securityContext": {"runAsUser": 2000, "runAsGroup": 3000, "supplementalGroups": [5000], "supplementalGroupsPolicy": policy},
                "volumes": [{"name": "identity", "configMap": {"name": "supplemental-groups-files"}}],
                "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "id -G > /dev/termination-log"], "volumeMounts": [{"name": "identity", "mountPath": "/etc/passwd", "subPath": "passwd"}, {"name": "identity", "mountPath": "/etc/group", "subPath": "group"}]}]
            }),
        )
        .await?;
    }
    let result = async {
        let _merge = context
            .wait_until("Merge supplemental groups", Duration::from_secs(120), || {
                let context = context.clone();
                async move { Ok(termination_message(&context, "supplemental-groups-merge").await?.is_some()) }
            })
            .await?;
        let _strict = context
            .wait_until("Strict supplemental groups", Duration::from_secs(120), || {
                let context = context.clone();
                async move { Ok(termination_message(&context, "supplemental-groups-strict").await?.is_some()) }
            })
            .await?;
        let merge = termination_message(context, "supplemental-groups-merge").await?.unwrap_or_default();
        let strict = termination_message(context, "supplemental-groups-strict").await?.unwrap_or_default();
        anyhow::ensure!(merge.split_whitespace().any(|group| group == "3000"), "Merge omitted the primary group: {merge}");
        anyhow::ensure!(merge.split_whitespace().any(|group| group == "5000"), "Merge omitted the explicit supplemental group: {merge}");
        anyhow::ensure!(strict.split_whitespace().any(|group| group == "3000"), "Strict omitted the primary group: {strict}");
        anyhow::ensure!(strict.split_whitespace().any(|group| group == "5000"), "Strict omitted the explicit supplemental group: {strict}");
        if !merge.split_whitespace().any(|group| group == "4000") {
            return Err(skip_test(format!(
            "the container runtime did not merge image-defined group membership; explicit and Strict groups passed (Merge={merge}, Strict={strict})"
            )));
        }
        anyhow::ensure!(
            !strict.split_whitespace().any(|group| group == "4000"),
            "Strict included image-defined group 4000: {strict}"
        );
        Ok(())
    }
    .await;
    let _ = pods.delete("supplemental-groups-merge", &DeleteParams::default()).await;
    let _ = pods.delete("supplemental-groups-strict", &DeleteParams::default()).await;
    let _ = configmaps.delete("supplemental-groups-files", &DeleteParams::default()).await;
    result
}

pub(super) async fn client_certificate_authentication_works(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("client-certificate authentication requires the CRI runtime"));
    }
    if !Command::new("openssl")
        .arg("version")
        .status()
        .is_ok_and(|status| status.success())
    {
        return Err(skip_test("client-certificate authentication needs openssl"));
    }
    let work = std::env::temp_dir().join(format!(
        "nodebootstrap-client-cert-{}",
        std::process::id()
    ));
    fs::create_dir_all(&work)?;
    let ca_key = work.join("ca.key");
    let ca_crt = work.join("ca.crt");
    let client_key = work.join("client.key");
    let client_csr = work.join("client.csr");
    let client_crt = work.join("client.crt");
    let other_key = work.join("other.key");
    let other_crt = work.join("other.crt");
    let extensions = work.join("client.ext");
    run_command(
        "openssl",
        &[
            "req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-keyout", ca_key.to_str().context("CA key path is not UTF-8")?,
            "-out", ca_crt.to_str().context("CA cert path is not UTF-8")?,
            "-days", "1", "-subj", "/CN=nodebootstrap-e2e-ca",
            "-addext", "basicConstraints=critical,CA:true",
            "-addext", "keyUsage=critical,keyCertSign,cRLSign",
        ],
    )?;
    run_command(
        "openssl",
        &[
            "req", "-newkey", "rsa:2048", "-nodes",
            "-keyout", client_key.to_str().context("client key path is not UTF-8")?,
            "-out", client_csr.to_str().context("client CSR path is not UTF-8")?,
            "-subj", "/CN=alice/O=system:masters",
        ],
    )?;
    fs::write(
        &extensions,
        "basicConstraints=critical,CA:false\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=clientAuth\n",
    )?;
    run_command(
        "openssl",
        &[
            "x509", "-req",
            "-in", client_csr.to_str().context("client CSR path is not UTF-8")?,
            "-CA", ca_crt.to_str().context("CA cert path is not UTF-8")?,
            "-CAkey", ca_key.to_str().context("CA key path is not UTF-8")?,
            "-CAcreateserial",
            "-out", client_crt.to_str().context("client cert path is not UTF-8")?,
            "-days", "1",
            "-extfile", extensions.to_str().context("extension path is not UTF-8")?,
        ],
    )?;
    run_command(
        "openssl",
        &[
            "req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-keyout", other_key.to_str().context("other key path is not UTF-8")?,
            "-out", other_crt.to_str().context("other cert path is not UTF-8")?,
            "-days", "1", "-subj", "/CN=untrusted",
            "-addext", "basicConstraints=critical,CA:true",
            "-addext", "keyUsage=critical,keyCertSign,cRLSign",
        ],
    )?;
    let _work_guard = CertDir(work.clone());
    let _override = NodeletEnvOverride::install(&[(
        "NODELET_CLIENT_CA_FILE",
        ca_crt.to_str().context("CA cert path is not UTF-8")?,
    )])?;
    let nodes: Api<k8s_openapi::api::core::v1::Node> = Api::all(context.client.clone());
    let node_ip = nodes
        .list(&Default::default())
        .await?
        .items
        .into_iter()
        .flat_map(|node| node.status.and_then(|status| status.addresses).unwrap_or_default())
        .find(|address| address.type_ == "InternalIP")
        .map(|address| address.address)
        .context("cluster Node has no InternalIP")?;
    let port = std::env::var("NODELET_SERVER_PORT").unwrap_or_else(|_| "10250".to_string());
    let endpoint = format!("https://{node_ip}:{port}/stats/summary");
    let trusted = Command::new("curl")
        .args([
            "-k", "-sS", "--max-time", "10",
            "--cert", client_crt.to_str().context("client cert path is not UTF-8")?,
            "--key", client_key.to_str().context("client key path is not UTF-8")?,
            &endpoint,
        ])
        .output()
        .context("calling nodelet with a trusted client certificate")?;
    anyhow::ensure!(
        trusted.status.success()
            && String::from_utf8_lossy(&trusted.stdout).contains("nodeName"),
        "trusted client certificate did not authenticate to nodelet: {}",
        String::from_utf8_lossy(&trusted.stderr)
    );
    let untrusted = Command::new("curl")
        .args([
            "-k", "-sS", "--max-time", "10",
            "--cert", other_crt.to_str().context("other cert path is not UTF-8")?,
            "--key", other_key.to_str().context("other key path is not UTF-8")?,
            &endpoint,
        ])
        .output()
        .context("calling nodelet with an untrusted client certificate")?;
    anyhow::ensure!(
        !untrusted.status.success(),
        "an untrusted client certificate unexpectedly authenticated to nodelet"
    );
    let no_auth = Command::new("curl")
        .args([
            "-k", "-sS", "--max-time", "10", "-o", "/dev/null", "-w", "%{http_code}",
            &endpoint,
        ])
        .output()
        .context("calling nodelet without authentication")?;
    anyhow::ensure!(
        String::from_utf8_lossy(&no_auth.stdout).trim() == "401",
        "nodelet's unauthenticated fallback returned {:?}, expected 401",
        String::from_utf8_lossy(&no_auth.stdout).trim()
    );
    drop(_override);
    Ok(())
}

struct CertDir(std::path::PathBuf);

impl Drop for CertDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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
            "containers": [{"name": "app", "image": "busybox:latest", "command": ["sh", "-c", "cat /proc/sys/net/ipv4/ip_unprivileged_port_start > /dev/termination-log"]}]
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
