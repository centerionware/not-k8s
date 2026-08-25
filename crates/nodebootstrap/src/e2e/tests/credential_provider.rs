use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use base64::Engine;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, PostParams};
use serde_json::json;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

pub(super) async fn credential_provider_config_unset_by_default(
    _context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("image credential provider checks require the CRI runtime"));
    }
    if std::env::var_os("NODELET_IMAGE_CREDENTIAL_PROVIDER_CONFIG")
        .is_some_and(|value| !value.is_empty())
    {
        return Err(skip_test(
            "NODELET_IMAGE_CREDENTIAL_PROVIDER_CONFIG is set; the default-off case does not apply to this deployment",
        ));
    }
    Ok(())
}

fn root_command(program: &str, args: &[&str]) -> Command {
    let root = Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "0");
    let command = if root {
        let mut command = Command::new(program);
        command.args(args);
        command
    } else {
        let mut command = Command::new("sudo");
        command.arg("-n").arg(program).args(args);
        command
    };
    command
}

fn root_run(program: &str, args: &[&str]) -> Result<Output> {
    let output = root_command(program, args).output()?;
    anyhow::ensure!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn command_available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn docker_command(config_dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("docker");
    command.env("DOCKER_CONFIG", config_dir).args(args);
    command
}

fn containerd_config_path() -> &'static Path {
    Path::new("/etc/containerd/config.toml")
}

fn configure_containerd_registry(config: &str) -> String {
    let section = r#"[plugins."io.containerd.grpc.v1.cri".registry]"#;
    let config_line = r#"config_path = "/etc/containerd/certs.d""#;

    let mut output = Vec::with_capacity(config.len() + 256);
    let mut in_registry_section = false;
    let mut in_legacy_mirror_section = false;
    let mut saw_registry_section = false;
    let mut saw_config_path = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(r#"[plugins."io.containerd.grpc.v1.cri".registry.mirrors."#) {
            // containerd rejects legacy registry.mirrors tables when the
            // modern config_path form is present. The hosts.toml fixture
            // below is the supported replacement and also lets us express
            // the intentionally-plain-HTTP local registry.
            in_legacy_mirror_section = true;
            continue;
        }
        if in_legacy_mirror_section {
            if trimmed.starts_with('[') {
                in_legacy_mirror_section = false;
            } else {
                continue;
            }
        }
        if trimmed == section {
            in_registry_section = true;
            saw_registry_section = true;
        } else if in_registry_section && trimmed.starts_with('[') {
            if !saw_config_path {
                output.push(config_line.to_string());
                saw_config_path = true;
            }
            in_registry_section = false;
        }

        if in_registry_section && trimmed.starts_with("config_path") {
            if !saw_config_path {
                output.push(config_line.to_string());
                saw_config_path = true;
            }
            continue;
        }
        output.push(line.to_string());
    }
    if in_registry_section && !saw_config_path {
        output.push(config_line.to_string());
        saw_config_path = true;
    }
    if !saw_registry_section {
        output.push(String::new());
        output.push(section.to_string());
        output.push(config_line.to_string());
    }

    let mut updated = output.join("\n");
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated
}

fn registry_requires_auth(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.write_all(
        b"GET /v2/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response.starts_with("HTTP/1.1 401") || response.starts_with("HTTP/1.0 401")
}

async fn pod_pull_failed(pods: &Api<Pod>, name: &str) -> Result<bool> {
    let pod = pods.get(name).await?;
    Ok(serde_json::to_value(pod)?
        .pointer("/status/containerStatuses/0/state/waiting/reason")
        .and_then(|value| value.as_str())
        .is_some_and(|reason| reason == "ImagePullBackOff" || reason == "ErrImagePull"))
}

async fn create_pull_pod(pods: &Api<Pod>, name: &str, image: &str) -> Result<()> {
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name},
        "spec": {"containers": [{"name": "app", "image": image, "command": ["sleep", "3600"]}]}
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    Ok(())
}

pub(super) async fn credential_provider_supplies_auth_for_an_otherwise_rejected_pull(
    context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("image credential providers require the CRI runtime"));
    }
    if !command_available("docker") {
        return Err(skip_test(
            "credential-provider registry test requires Docker to run a local auth-required registry",
        ));
    }
    if !Command::new("sudo")
        .args(["-n", "true"])
        .status()
        .is_ok_and(|status| status.success())
        && !Command::new("id")
            .arg("-u")
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "0")
    {
        return Err(skip_test(
            "credential-provider registry test requires root or passwordless sudo",
        ));
    }

    let work = std::env::temp_dir().join(format!(
        "nodebootstrap-credential-provider-{}",
        std::process::id()
    ));
    fs::create_dir_all(&work)?;
    let registry_name = "nodebootstrap-e2e-registry";
    let registry_port = 5001u16;
    // Use the IPv4 loopback address explicitly.  `localhost` can resolve to
    // ::1 on the runner while Docker publishes only an IPv4 socket, and a
    // stale localhost certs.d entry can make containerd try HTTPS before the
    // fixture's HTTP hosts.toml is considered.
    let registry_host = format!("127.0.0.1:{registry_port}");
    let image = format!("{registry_host}/credential-provider-check:1");
    let config_path = containerd_config_path();
    let original_config = fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config_backup = work.join("config.toml");
    let mut config_changed = false;

    let result = async {
        let htpasswd = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--entrypoint",
                "htpasswd",
                "httpd:2.4-alpine",
                "-Bbn",
                "testuser",
                "testpass123",
            ])
            .output()?;
        anyhow::ensure!(
            htpasswd.status.success(),
            "generating registry credentials failed: {}",
            String::from_utf8_lossy(&htpasswd.stderr)
        );
        let htpasswd_path = work.join("htpasswd");
        fs::write(&htpasswd_path, htpasswd.stdout)?;
        let registry = Command::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                registry_name,
                "-p",
                "127.0.0.1:5001:5000",
            ])
            .args([
                "-v",
                &format!("{}:/auth/htpasswd:ro", htpasswd_path.display()),
                "-e",
                "REGISTRY_AUTH=htpasswd",
                "-e",
                "REGISTRY_AUTH_HTPASSWD_REALM=nodebootstrap-e2e",
                "-e",
                "REGISTRY_AUTH_HTPASSWD_PATH=/auth/htpasswd",
                "registry:2",
            ])
            .output()?;
        anyhow::ensure!(
            registry.status.success(),
            "starting auth-required registry failed: {}",
            String::from_utf8_lossy(&registry.stderr)
        );
        context
            .wait_until("local registry to require authentication", Duration::from_secs(45), || async {
                Ok(registry_requires_auth(registry_port))
            })
            .await?;

        let updated = configure_containerd_registry(&original_config);
        if updated != original_config {
            fs::copy(config_path, &config_backup)?;
            let updated_path = work.join("config.updated.toml");
            fs::write(&updated_path, updated)?;
            root_run(
                "install",
                &[
                    "-m",
                    "0644",
                    updated_path.to_str().context("updated config path is not UTF-8")?,
                    config_path.to_str().context("containerd config path is not UTF-8")?,
                ],
            )?;
            config_changed = true;
        }

        let hosts_dir = PathBuf::from("/etc/containerd/certs.d").join(&registry_host);
        let hosts_file = work.join("hosts.toml");
        fs::write(
            &hosts_file,
            format!(
                "server = \"http://{registry_host}\"\n\n[host.\"http://{registry_host}\"]\ncapabilities = [\"pull\", \"resolve\", \"push\"]\n"
            ),
        )?;
        // A cancelled or timed-out e2e run can leave a certs.d directory
        // behind.  Remove it before installing this fixture so an unrelated
        // CA/TLS entry cannot override the explicit HTTP endpoint below.
        root_run("rm", &["-rf", hosts_dir.to_str().context("hosts path is not UTF-8")?])?;
        root_run("mkdir", &["-p", hosts_dir.to_str().context("hosts path is not UTF-8")?])?;
        root_run(
            "install",
            &[
                "-m",
                "0644",
                hosts_file.to_str().context("hosts file path is not UTF-8")?,
                &format!("{}/hosts.toml", hosts_dir.display()),
            ],
        )?;
        root_run("systemctl", &["restart", "containerd"])?;
        context
            .wait_until("containerd to return after registry configuration", Duration::from_secs(45), || async {
                Ok(root_command("ctr", &["version"]).status().is_ok_and(|status| status.success()))
            })
            .await?;

        anyhow::ensure!(
            Command::new("docker")
                .args(["pull", "busybox:latest"])
                .status()?
                .success(),
            "pulling the base image for the private registry failed"
        );
        let auth = base64::engine::general_purpose::STANDARD.encode("testuser:testpass123");
        let docker_config_dir = work.join("docker");
        fs::create_dir_all(&docker_config_dir)?;
        fs::write(
            docker_config_dir.join("config.json"),
            format!(r#"{{"auths":{{"{registry_host}":{{"auth":"{auth}"}}}}}}"#),
        )?;
        anyhow::ensure!(
            docker_command(&docker_config_dir, &["tag", "busybox:latest", &image])
                .status()?
                .success(),
            "tagging the private-registry image failed"
        );
        anyhow::ensure!(
            docker_command(&docker_config_dir, &["push", &image])
                .status()?
                .success(),
            "pushing the private-registry image failed"
        );

        let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
        let negative = "credential-provider-check-neg";
        create_pull_pod(&pods, negative, &image).await?;
        context
            .wait_until("unauthenticated pull to fail", Duration::from_secs(150), || {
                let pods = pods.clone();
                async move { pod_pull_failed(&pods, negative).await }
            })
            .await?;
        pods.delete(negative, &DeleteParams::default()).await?;

        let provider = work.join("fake-credential-provider");
        fs::write(
            &provider,
            format!(
                "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{{\"apiVersion\":\"credentialprovider.kubelet.k8s.io/v1\",\"kind\":\"CredentialProviderResponse\",\"cacheKeyType\":\"Registry\",\"cacheDuration\":\"0s\",\"auth\":{{\"{registry_host}\":{{\"username\":\"testuser\",\"password\":\"testpass123\"}}}}}}'\\n"
            ),
        )?;
        let mut permissions = fs::metadata(&provider)?.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o755);
            fs::set_permissions(&provider, permissions)?;
        }
        let provider_config = work.join("credential-provider.yaml");
        fs::write(
            &provider_config,
            format!(
                "apiVersion: kubelet.config.k8s.io/v1\nkind: CredentialProviderConfig\nproviders:\n  - name: fake-credential-provider\n    matchImages: [\"{registry_host}/*\"]\n    defaultCacheDuration: \"0s\"\n"
            ),
        )?;
        let _nodelet_env = super::resource_managers::NodeletEnvOverride::install(&[
            (
                "NODELET_IMAGE_CREDENTIAL_PROVIDER_CONFIG",
                provider_config.to_str().context("provider config path is not UTF-8")?,
            ),
            (
                "NODELET_IMAGE_CREDENTIAL_PROVIDER_BIN_DIR",
                work.to_str().context("provider directory is not UTF-8")?,
            ),
        ])
        .map_err(|error| skip_test(format!("could not restart nodelet with credential provider: {error}")))?;

        let positive = "credential-provider-check";
        create_pull_pod(&pods, positive, &image).await?;
        context
            .wait_until("credential-provider-authenticated pull", Duration::from_secs(150), || {
                let pods = pods.clone();
                async move {
                    Ok(pods
                        .get(positive)
                        .await?
                        .status
                        .and_then(|status| status.phase)
                        .as_deref()
                        == Some("Running"))
                }
            })
            .await?;
        Ok(())
    }
    .await;

    let hosts_dir = PathBuf::from("/etc/containerd/certs.d").join(&registry_host);
    let _ = root_run("rm", &["-rf", hosts_dir.to_str().unwrap_or_default()]);
    if config_changed {
        let _ = root_run(
            "install",
            &[
                "-m",
                "0644",
                config_backup.to_str().unwrap_or_default(),
                config_path.to_str().unwrap_or_default(),
            ],
        );
    }
    let _ = root_run("systemctl", &["restart", "containerd"]);
    let _ = Command::new("docker")
        .args(["rm", "-f", registry_name])
        .status();
    let _ = fs::remove_dir_all(&work);
    result
}
