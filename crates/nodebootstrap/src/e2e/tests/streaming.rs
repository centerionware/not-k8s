use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use futures::io::AsyncBufReadExt;
use futures::TryStreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, AttachParams, LogParams, PostParams};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

fn contains_later_line(output: &str, prefix: &str) -> bool {
    (2..=8).any(|line| output.contains(&format!("{prefix}{line}")))
}

async fn stream_until<R>(mut stdout: R, prefix: &str) -> Result<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
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
    read_result
        .context("timed out waiting for a later line from the stream")??;
    let output = String::from_utf8_lossy(&bytes).into_owned();
    anyhow::ensure!(
        output.contains(&format!("{prefix}1")),
        "stream produced no initial {prefix}1 line: {output:?}"
    );
    anyhow::ensure!(
        contains_later_line(&output, prefix),
        "stream disconnected after its first line: {output:?}"
    );
    Ok(output)
}

async fn exec_output(context: &E2eContext, name: &str, command: &[&str]) -> Result<String> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let params = AttachParams::default()
        .container("app")
        .stdout(true)
        .stderr(false);
    let mut process = pods.exec(name, command.iter().copied(), &params).await?;
    let mut stdout = Vec::new();
    if let Some(mut stream) = process.stdout() {
        stream.read_to_end(&mut stdout).await?;
    }
    process.join().await?;
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

pub(super) async fn kubectl_logs_returns_real_output(context: &E2eContext) -> Result<()> {
    if let Err(error) = needs_cri() {
        return Err(skip_test(error.to_string()));
    }
    let name = "logs-check";
    create_pod(context, name, &["sh", "-c", "echo hello-from-nodelet-logs; sleep 3600"]).await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let output = pods.logs(name, &LogParams::default()).await?;
    anyhow::ensure!(
        output.contains("hello-from-nodelet-logs"),
        "logs did not return the container output: {output:?}"
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
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let mut logs = pods
        .log_stream(
            name,
            &LogParams {
                follow: true,
                ..Default::default()
            },
        )
        .await?
        .lines();
    let output = tokio::time::timeout(Duration::from_secs(40), async {
        let mut output = String::new();
        while let Some(line) = logs.try_next().await? {
            output.push_str(&line);
            output.push('\n');
            if output.contains("line-1") && contains_later_line(&output, "line-") {
                return Ok::<String, anyhow::Error>(output);
            }
        }
        Ok(output)
    })
    .await
    .context("timed out waiting for a later line from the log stream")??;
    anyhow::ensure!(contains_later_line(&output, "line-"), "log stream disconnected after its first line: {output:?}");
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
    let output = exec_output(context, name, &["echo", "hello-from-exec"]).await?;
    anyhow::ensure!(
        output.contains("hello-from-exec"),
        "exec did not return the command output: {output:?}"
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
            "sleep 10; for i in 1 2 3 4 5 6 7 8; do echo attach-line-$i; sleep 1; done; sleep 3600",
        ],
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let params = AttachParams::default()
        .container("app")
        .stdout(true)
        .stderr(false);
    let mut process = pods.attach(name, &params).await?;
    let output = if let Some(stdout) = process.stdout() {
        stream_until(stdout, "attach-line-").await?
    } else {
        anyhow::bail!("attach did not expose container stdout")
    };
    process.abort();
    anyhow::ensure!(contains_later_line(&output, "attach-line-"));
    Ok(())
}

async fn port_forward_response(context: &E2eContext, name: &str) -> Result<String> {
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let mut forwarder = pods.portforward(name, &[8080]).await?;
    let mut stream = forwarder
        .take_stream(8080)
        .context("port-forward did not expose the container port")?;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await?;
    let response = tokio::time::timeout(Duration::from_secs(40), async {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let count = stream.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            let response = String::from_utf8_lossy(&bytes);
            if response.contains("port-forward-marker") {
                return Ok::<String, std::io::Error>(response.into_owned());
            }
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    })
    .await
    .context("timed out waiting for port-forward to reach the container")??;
    forwarder.abort();
    Ok(response)
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
    let response = port_forward_response(context, name).await?;
    anyhow::ensure!(
        response.contains("port-forward-marker"),
        "port-forward response did not contain the container marker: {response:?}"
    );
    Ok(())
}
