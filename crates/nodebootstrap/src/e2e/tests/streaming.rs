use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, PostParams};
use serde_json::json;
use std::process::{Output, Stdio};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};

fn needs_cri() -> Result<()> {
    anyhow::ensure!(
        crate::config::Config::from_env()?.nodelet_runtime() == "cri",
        "nodelet streaming endpoints require the CRI runtime",
    );
    Ok(())
}

async fn create_pod(context: &E2eContext, name: &str, command: &[&str]) -> Result<()> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {
            "containers": [{"name": "app", "image": "busybox:latest", "command": command}]
        }
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("streaming test Pod to reach Running", Duration::from_secs(90), || {
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

fn kubectl_output(namespace: &str, args: &[&str]) -> Result<Output> {
    std::process::Command::new("kubectl")
        .arg("-n")
        .arg(namespace)
        .args(args)
        .output()
        .with_context(|| format!("running kubectl -n {namespace} {args:?}"))
}

fn ensure_kubectl_success(output: &Output, description: &str) -> Result<String> {
    anyhow::ensure!(
        output.status.success(),
        "{description} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn contains_later_line(output: &str, prefix: &str) -> bool {
    (2..=8).any(|line| output.contains(&format!("{prefix}{line}")))
}

async fn stream_until(namespace: &str, args: &[&str], prefix: &str) -> Result<String> {
    let mut child = Command::new("kubectl")
        .arg("-n")
        .arg(namespace)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting kubectl -n {namespace} {args:?}"))?;
    let mut stdout = child
        .stdout
        .take()
        .context("kubectl streaming command did not expose stdout")?;
    let mut bytes = Vec::new();
    let read_result = tokio::time::timeout(Duration::from_secs(40), async {
        let mut buffer = [0_u8; 1024];
        loop {
            let count = stdout.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            let output = String::from_utf8_lossy(&bytes);
            if output.contains(&format!("{prefix}1")) && contains_later_line(&output, prefix) {
                break;
            }
        }
        Ok::<(), std::io::Error>(())
    })
    .await;
    let _ = child.kill().await;
    let _ = child.wait().await;
    read_result
        .context("timed out waiting for a later line from the kubectl stream")??;
    let output = String::from_utf8_lossy(&bytes).into_owned();
    anyhow::ensure!(
        output.contains(&format!("{prefix}1")),
        "kubectl stream produced no initial {prefix}1 line: {output:?}"
    );
    anyhow::ensure!(
        contains_later_line(&output, prefix),
        "kubectl stream disconnected after its first line: {output:?}"
    );
    Ok(output)
}

async fn stop_child(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

pub(super) async fn kubectl_logs_returns_real_output(context: &E2eContext) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let name = "logs-check";
    create_pod(context, name, &["sh", "-c", "echo hello-from-nodelet-logs; sleep 3600"]).await?;
    let output = ensure_kubectl_success(
        &kubectl_output(&context.namespace, &["logs", name])?,
        "kubectl logs",
    )?;
    anyhow::ensure!(
        output.contains("hello-from-nodelet-logs"),
        "kubectl logs did not return the container output: {output:?}"
    );
    Ok(())
}

pub(super) async fn kubectl_logs_follow_streams_new_output(context: &E2eContext) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let name = "logs-follow-check";
    create_pod(
        context,
        name,
        &[
            "sh",
            "-c",
            "for i in 1 2 3 4 5 6 7 8; do echo line-$i; sleep 1; done; sleep 3600",
        ],
    )
    .await?;
    let _ = stream_until(&context.namespace, &["logs", "-f", name], "line-").await?;
    Ok(())
}

pub(super) async fn kubectl_exec_runs_a_command_and_returns_its_output(
    context: &E2eContext,
) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let name = "exec-check";
    create_pod(context, name, &["sleep", "3600"]).await?;
    let output = ensure_kubectl_success(
        &kubectl_output(
            &context.namespace,
            &["exec", name, "--", "echo", "hello-from-exec"],
        )?,
        "kubectl exec",
    )?;
    anyhow::ensure!(
        output.contains("hello-from-exec"),
        "kubectl exec did not return the command output: {output:?}"
    );
    Ok(())
}

pub(super) async fn kubectl_attach_streams_the_containers_stdout(
    context: &E2eContext,
) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let name = "attach-check";
    create_pod(
        context,
        name,
        &[
            "sh",
            "-c",
            "for i in 1 2 3 4 5 6 7 8; do echo attach-line-$i; sleep 1; done; sleep 3600",
        ],
    )
    .await?;
    let _ = stream_until(&context.namespace, &["attach", name], "attach-line-").await?;
    Ok(())
}

async fn port_forward_response(namespace: &str, name: &str) -> Result<String> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let local_port = listener.local_addr()?.port();
    drop(listener);
    let port_mapping = format!("{local_port}:8080");
    let mut child = Command::new("kubectl")
        .arg("-n")
        .arg(namespace)
        .arg("port-forward")
        .arg(name)
        .arg(&port_mapping)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting kubectl port-forward")?;

    let response = tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", local_port)).await {
                stream
                    .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .await?;
                let mut bytes = Vec::new();
                if tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut bytes))
                    .await
                    .is_ok()
                {
                    let response = String::from_utf8_lossy(&bytes).into_owned();
                    if response.contains("port-forward-marker") {
                        return Ok(response);
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await;
    stop_child(&mut child).await;
    Ok(response.context("timed out waiting for kubectl port-forward to reach the container")??)
}

pub(super) async fn kubectl_port_forward_reaches_a_real_container_port(
    context: &E2eContext,
) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let name = "port-forward-check";
    create_pod(
        context,
        name,
        &[
            "sh",
            "-c",
            "printf 'HTTP/1.1 200 OK\\r\\nContent-Type: text/plain\\r\\nConnection: close\\r\\n\\r\\nport-forward-marker\\n' > /tmp/resp && while true; do nc -lp 8080 < /tmp/resp; done",
        ],
    )
    .await?;
    let response = port_forward_response(&context.namespace, name).await?;
    anyhow::ensure!(
        response.contains("port-forward-marker"),
        "port-forward response did not contain the container marker: {response:?}"
    );
    Ok(())
}
