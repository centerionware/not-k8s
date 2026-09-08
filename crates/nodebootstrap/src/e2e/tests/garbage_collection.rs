use super::context::{labels, E2eContext};
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

fn needs_cri() -> Result<()> {
    anyhow::ensure!(
        crate::config::Config::from_env()?.nodelet_runtime() == "cri",
        "container cleanup checks require the CRI runtime",
    );
    Ok(())
}

fn ctr_path() -> Option<&'static str> {
    ["/usr/local/bin/ctr", "/usr/bin/ctr", "ctr"]
        .into_iter()
        .find(|path| {
            if path.contains('/') {
                std::path::Path::new(path).is_file()
            } else {
                std::env::var_os("PATH").is_some_and(|paths| {
                    std::env::split_paths(&paths).any(|directory| directory.join(path).is_file())
                })
            }
        })
}

fn containerd_has_container(ctr: &str, id: &str) -> Result<bool> {
    let output = Command::new("sudo")
        .args([ctr, "-n", "k8s.io", "containers", "ls", "-q"])
        .output()
        .with_context(|| format!("running sudo {ctr} -n k8s.io containers ls -q"))?;
    anyhow::ensure!(
        output.status.success(),
        "ctr could not list containerd containers: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.trim() == id))
}

fn containerd_has_image(ctr: &str, image: &str) -> Result<bool> {
    let output = Command::new("sudo")
        .args([ctr, "-n", "k8s.io", "images", "ls", "-q"])
        .output()
        .with_context(|| format!("running sudo {ctr} -n k8s.io images ls -q"))?;
    anyhow::ensure!(
        output.status.success(),
        "ctr could not list containerd images: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let canonical = if image.contains('/') {
        image.to_owned()
    } else {
        format!("docker.io/library/{image}")
    };
    Ok(String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let line = line.trim();
        line == image || line == canonical
    }))
}

async fn restart_nodelet_with_override(context: &E2eContext, contents: &str) -> Result<()> {
    write_nodelet_gc_override(Some(contents))?;
    run_systemctl(&["daemon-reload",])?;
    run_systemctl(&["reset-failed", "nodelet.service"])?;
    run_systemctl(&["restart", "nodelet.service"])?;
    context
        .wait_until("nodelet to become active after image-GC configuration", Duration::from_secs(60), || async {
            Ok(Command::new("systemctl")
                .args(["is-active", "--quiet", "nodelet.service"])
                .status()
                .is_ok_and(|status| status.success()))
        })
        .await
}

async fn image_gc_case(context: &E2eContext, override_contents: &str, image: &str, expect_present: bool) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let Some(ctr) = ctr_path() else {
        return Err(skip_test("ctr is not installed; image-GC state cannot be verified"));
    };
    if !Command::new("systemctl")
        .args(["cat", "nodelet.service"])
        .status()
        .is_ok_and(|status| status.success())
    {
        return Err(skip_test("image-GC checks require a systemd-managed nodelet service"));
    }
    let usage_path = std::env::var("NODELET_DISK_PATH").unwrap_or_else(|_| "/".to_owned());
    let usage = Command::new("df")
        .args(["--output=pcent", &usage_path])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|value| value.lines().last().map(|line| line.trim().trim_end_matches('%').parse::<u8>().ok()).flatten());
    if !expect_present && usage.is_some_and(|value| value >= 99) {
        return Err(skip_test("the test filesystem is already at 99% usage; a low watermark would not be a controlled image-GC check"));
    }

    let name = if expect_present { "image-gc-below-watermark-check" } else { "image-gc-watermark-check" };
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let result = async {
        restart_nodelet_with_override(context, override_contents).await?;
        let pod: Pod = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": name},
            "spec": {"containers": [{"name": "app", "image": image, "command": ["sleep", "60"]}]}
        }))?;
        pods.create(&PostParams::default(), &pod).await?;
        context
            .wait_until("image-GC test Pod to reach Running", Duration::from_secs(90), || {
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
        anyhow::ensure!(containerd_has_image(ctr, image)?, "containerd did not retain pulled image {image}");
        pods.delete(name, &DeleteParams::default()).await?;
        context
            .wait_until("image-GC test Pod to disappear", Duration::from_secs(120), || {
                let pods = pods.clone();
                async move { Ok(pods.get_opt(name).await?.is_none()) }
            })
            .await?;
        context
            .wait_until("image-GC result", Duration::from_secs(120), || {
                let ctr = ctr.to_owned();
                async move { Ok(containerd_has_image(&ctr, image)? == expect_present) }
            })
            .await
    }
    .await;
    let _ = pods.delete(name, &DeleteParams::default()).await;
    let _ = write_nodelet_gc_override(None);
    let _ = run_systemctl(&["daemon-reload"]);
    let _ = run_systemctl(&["restart", "nodelet.service"]);
    result
}

pub(super) async fn unreferenced_image_is_not_removed_below_the_watermark(
    context: &E2eContext,
) -> Result<()> {
    image_gc_case(
        context,
        "[Service]\nEnvironment=NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT=99\nEnvironment=NODELET_IMAGE_GC_LOW_THRESHOLD_PERCENT=90\nEnvironment=NODELET_IMAGE_GC_MIN_AGE_SECS=1\nEnvironment=NODELET_GC_INTERVAL_SECS=10\n",
        "busybox:1.36.1",
        true,
    )
    .await
}

pub(super) async fn image_gc_removes_unreferenced_images_above_the_watermark(
    context: &E2eContext,
) -> Result<()> {
    image_gc_case(
        context,
        "[Service]\nEnvironment=NODELET_IMAGE_GC_HIGH_THRESHOLD_PERCENT=10\nEnvironment=NODELET_IMAGE_GC_LOW_THRESHOLD_PERCENT=5\nEnvironment=NODELET_IMAGE_GC_MIN_AGE_SECS=1\nEnvironment=NODELET_GC_INTERVAL_SECS=10\n",
        "busybox:1.33.1",
        false,
    )
    .await
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let uid = Command::new("id").arg("-u").output()?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_owned();
    let mut command = if uid == "0" {
        let mut command = Command::new("systemctl");
        command.args(args);
        command
    } else {
        let mut command = Command::new("sudo");
        command.arg("systemctl").args(args);
        command
    };
    let output = command.output()?;
    anyhow::ensure!(
        output.status.success(),
        "systemctl {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn write_nodelet_gc_override(contents: Option<&str>) -> Result<()> {
    let path = "/etc/systemd/system/nodelet.service.d/99-nodebootstrap-e2e.conf";
    let uid = Command::new("id").arg("-u").output()?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_owned();
    if uid == "0" {
        std::fs::create_dir_all("/etc/systemd/system/nodelet.service.d")?;
        match contents {
            Some(contents) => std::fs::write(path, contents)?,
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
    } else if let Some(contents) = contents {
        anyhow::ensure!(
            Command::new("sudo")
                .args(["mkdir", "-p", "/etc/systemd/system/nodelet.service.d"])
                .status()?
                .success(),
            "sudo mkdir failed creating nodelet override directory"
        );
        let mut child = Command::new("sudo")
            .args(["tee", path])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()?;
        child
            .stdin
            .take()
            .context("sudo tee did not provide stdin")?
            .write_all(contents.as_bytes())?;
        anyhow::ensure!(child.wait()?.success(), "sudo tee failed writing nodelet override");
    } else {
        anyhow::ensure!(
            Command::new("sudo").args(["rm", "-f", path]).status()?.success(),
            "sudo rm failed removing nodelet override"
        );
    }
    Ok(())
}

pub(super) async fn orphaned_sandbox_gc_reaps_a_pod_deleted_while_nodelet_is_down(
    context: &E2eContext,
) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let Some(ctr) = ctr_path() else {
        return Err(skip_test("ctr is not installed; orphaned sandbox cleanup cannot be verified"));
    };
    if !Command::new("systemctl")
        .args(["cat", "nodelet.service"])
        .status()
        .is_ok_and(|status| status.success())
    {
        return Err(skip_test("orphaned sandbox cleanup requires a systemd-managed nodelet service"));
    }

    let override_contents = "[Service]\nEnvironment=NODELET_GC_INTERVAL_SECS=10\n";
    write_nodelet_gc_override(Some(override_contents))?;
    let setup = run_systemctl(&["daemon-reload"]).and_then(|_| run_systemctl(&["restart", "nodelet.service"]));
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let name = "orphaned-sandbox-gc-check";
    let result = async {
        setup?;
        context
            .wait_until("nodelet to be active for orphaned sandbox setup", Duration::from_secs(60), || async {
                Ok(Command::new("systemctl")
                    .args(["is-active", "--quiet", "nodelet.service"])
                    .status()
                    .is_ok_and(|status| status.success()))
            })
            .await?;
        let pod: Pod = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": name},
            "spec": {"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
        }))?;
        pods.create(&PostParams::default(), &pod).await?;
        context
            .wait_until("orphaned sandbox test Pod to reach Running", Duration::from_secs(90), || {
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
        let container_id = serde_json::to_value(pods.get(name).await?)?
            .pointer("/status/containerStatuses/0/containerID")
            .and_then(Value::as_str)
            .and_then(|value| value.strip_prefix("containerd://"))
            .map(str::to_owned)
            .context("Running Pod has no containerd container ID")?;
        run_systemctl(&["stop", "nodelet.service"])?;
        pods.delete(
            name,
            &DeleteParams {
                grace_period_seconds: Some(0),
                ..Default::default()
            },
        )
        .await?;
        context
            .wait_until("forced-delete Pod to disappear", Duration::from_secs(30), || {
                let pods = pods.clone();
                async move { Ok(pods.get_opt(name).await?.is_none()) }
            })
            .await?;
        run_systemctl(&["start", "nodelet.service"])?;
        context
            .wait_until("nodelet to recover after orphaned sandbox setup", Duration::from_secs(60), || async {
                Ok(Command::new("systemctl")
                    .args(["is-active", "--quiet", "nodelet.service"])
                    .status()
                    .is_ok_and(|status| status.success()))
            })
            .await?;
        context
            .wait_until("orphaned container to be garbage-collected", Duration::from_secs(120), || {
                let ctr = ctr.to_owned();
                let container_id = container_id.clone();
                async move { Ok(!containerd_has_container(&ctr, &container_id)?) }
            })
            .await
    }
    .await;
    let _ = write_nodelet_gc_override(None);
    let _ = run_systemctl(&["daemon-reload"]);
    let _ = run_systemctl(&["restart", "nodelet.service"]);
    result
}

pub(super) async fn garbage_collector_cascades_deployment_delete_to_replicaset_and_pods(
    context: &E2eContext,
) -> Result<()> {
    let name = "garbage-collector-test";
    let deployments: Api<Deployment> = Api::namespaced(context.client.clone(), &context.namespace);
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let deployment: Deployment = serde_json::from_value(json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": name},
        "spec": {"replicas": 2, "selector": {"matchLabels": {"app": name}}, "template": {
            "metadata": {"labels": {"app": name}},
            "spec": {"containers": [{"name": "busybox", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
        }}
    }))?;
    deployments
        .create(&PostParams::default(), &deployment)
        .await
        .context("creating garbage collector test Deployment")?;
    context
        .wait_until("garbage collector test Deployment creates a ReplicaSet", Duration::from_secs(60), || {
            let replicasets: Api<k8s_openapi::api::apps::v1::ReplicaSet> =
                Api::namespaced(context.client.clone(), &context.namespace);
            async move {
                Ok(replicasets
                    .list(&labels(&format!("app={name}")))
                    .await?
                    .items
                    .len()
                    == 1)
            }
        })
        .await?;
    context
        .wait_until("garbage collector test Deployment creates two Pods", Duration::from_secs(60), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .list(&labels(&format!("app={name}")))
                    .await?
                    .items
                    .len()
                    == 2)
            }
        })
        .await?;
    deployments.delete(name, &DeleteParams::default()).await?;
    let cascade = context
        .wait_until("garbage collector removes the Deployment ReplicaSet", Duration::from_secs(60), || {
            let replicasets: Api<k8s_openapi::api::apps::v1::ReplicaSet> =
                Api::namespaced(context.client.clone(), &context.namespace);
            async move {
                Ok(replicasets
                    .list(&labels(&format!("app={name}")))
                    .await?
                    .items
                    .is_empty())
            }
        })
        .await;
    if let Err(error) = cascade {
        let replicasets: Api<k8s_openapi::api::apps::v1::ReplicaSet> =
            Api::namespaced(context.client.clone(), &context.namespace);
        // Capture evidence before namespace cleanup deletes the owner chain.
        // A recreated owner, a stuck finalizer, and a missed GC event are
        // different failures even though all three look like a timeout.
        eprintln!("GC Deployment at failure: {:?}", deployments.get_opt(name).await);
        eprintln!("GC ReplicaSets at failure: {:?}", replicasets.list(&labels(&format!("app={name}"))).await);
        eprintln!("GC Pods at failure: {:?}", pods.list(&labels(&format!("app={name}"))).await);
        return Err(error);
    }
    context
        .wait_until("garbage collector removes the Deployment Pods", Duration::from_secs(120), || {
            let pods = pods.clone();
            async move {
                Ok(pods
                    .list(&ListParams::default().labels(&format!("app={name}")))
                    .await?
                    .items
                    .is_empty())
            }
        })
        .await
}

pub(super) async fn pod_teardown_actually_removes_the_sandbox(
    context: &E2eContext,
) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let Some(ctr) = ctr_path() else {
        return Err(skip_test("ctr is not installed; container cleanup cannot be verified"));
    };
    let name = "teardown-check";
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("sandbox cleanup Pod to reach Running", Duration::from_secs(90), || {
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
    let container_id = serde_json::to_value(pods.get(name).await?)?
        .pointer("/status/containerStatuses/0/containerID")
        .and_then(|value| value.as_str())
        .and_then(|value| value.strip_prefix("containerd://"))
        .map(str::to_string)
        .context("Running Pod has no containerd container ID")?;
    pods.delete(name, &DeleteParams::default()).await?;
    context
        .wait_until("deleted Pod object to disappear", Duration::from_secs(120), || {
            let pods = pods.clone();
            async move { Ok(pods.get_opt(name).await?.is_none()) }
        })
        .await?;
    context
        .wait_until("containerd container to be removed after Pod deletion", Duration::from_secs(40), || {
            let ctr = ctr.to_string();
            let container_id = container_id.clone();
            async move { Ok(!containerd_has_container(&ctr, &container_id)?) }
        })
        .await
}
