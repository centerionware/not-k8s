use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use kube::api::{Api, DeleteParams, DynamicObject, Patch, PatchParams, PostParams};
use kube::core::{GroupVersionKind, ObjectMeta};
use kube::discovery::ApiResource;
use kube::ResourceExt;
use serde_json::{json, Value};
use std::process::Command;
use std::time::Duration;

const CERT_MANAGER_VERSION: &str = "v1.18.2";
const CERT_MANAGER_NAMESPACE: &str = "cert-manager";

fn kubectl_available() -> bool {
    Command::new("kubectl")
        .arg("version")
        .arg("--client=true")
        .status()
        .is_ok_and(|status| status.success())
}

fn nodecontroller_is_active() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", "nodecontroller"])
        .status()
        .is_ok_and(|status| status.success())
        || Command::new("pgrep")
            .args(["-x", "nodecontroller"])
            .status()
            .is_ok_and(|status| status.success())
}

fn nodecontroller_pid() -> Option<u32> {
    let systemd_pid = Command::new("systemctl")
        .args([
            "show",
            "nodecontroller.service",
            "--property=MainPID",
            "--value",
        ])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|pid| *pid != 0);
    systemd_pid.or_else(|| {
        Command::new("pgrep")
            .args(["-x", "nodecontroller"])
            .output()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|value| value.lines().next()?.trim().parse::<u32>().ok())
    })
}

fn run_kubectl(args: &[&str]) -> Result<()> {
    let output = Command::new("kubectl")
        .args(args)
        .output()
        .with_context(|| format!("running kubectl {}", args.join(" ")))?;
    anyhow::ensure!(
        output.status.success(),
        "kubectl {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn cert_manager_manifest_url() -> String {
    let version = std::env::var("TEST_CERT_MANAGER_VERSION")
        .unwrap_or_else(|_| CERT_MANAGER_VERSION.to_string());
    format!(
        "https://github.com/cert-manager/cert-manager/releases/download/{version}/cert-manager.yaml"
    )
}

fn resource(group: &str, version: &str, kind: &str) -> ApiResource {
    ApiResource::from_gvk(&GroupVersionKind::gvk(group, version, kind))
}

fn ready_condition(value: &Value) -> bool {
    value
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .is_some_and(|conditions| {
            conditions.iter().any(|condition| {
                condition.get("type").and_then(Value::as_str) == Some("Ready")
                    && condition.get("status").and_then(Value::as_str) == Some("True")
            })
        })
}

fn crd_is_established(
    crd: &k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition,
) -> bool {
    crd.status.as_ref().is_some_and(|status| {
        status.conditions.as_ref().is_some_and(|conditions| {
            conditions
                .iter()
                .any(|condition| condition.type_ == "Established" && condition.status == "True")
        })
    })
}

async fn certificate_secret_is_complete(secrets: &Api<Secret>, name: &str) -> Result<bool> {
    let Some(secret) = secrets.get_opt(name).await? else {
        return Ok(false);
    };
    Ok(secret
        .data
        .as_ref()
        .is_some_and(|data| data.contains_key("tls.crt") && data.contains_key("tls.key")))
}

pub(super) async fn cert_manager_crds_are_usable_without_nodecontroller_restart(
    context: &E2eContext,
) -> Result<()> {
    if !nodecontroller_is_active() {
        return Err(skip_test(
            "nodecontroller is not active; CRD discovery refresh requires --controller-manager=nodecontroller",
        ));
    }
    if !kubectl_available() {
        return Err(skip_test(
            "kubectl is unavailable; cert-manager installation cannot be tested",
        ));
    }
    let Some(initial_pid) = nodecontroller_pid() else {
        return Err(skip_test("could not determine nodecontroller's PID"));
    };

    let manifest_url = cert_manager_manifest_url();
    let issuer_name = format!("nodebootstrap-e2e-issuer-{}", std::process::id());
    let certificate_name = "nodebootstrap-e2e-certificate";
    let secret_name = "nodebootstrap-e2e-tls";
    let owner_name = "nodebootstrap-e2e-certificate-owner";
    let crd_api: Api<k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition> =
        Api::all(context.client.clone());
    let issuers: Api<DynamicObject> = Api::all_with(
        context.client.clone(),
        &resource("cert-manager.io", "v1", "ClusterIssuer"),
    );
    let certificates: Api<DynamicObject> = Api::namespaced_with(
        context.client.clone(),
        &context.namespace,
        &resource("cert-manager.io", "v1", "Certificate"),
    );
    let deployments: Api<Deployment> =
        Api::namespaced(context.client.clone(), CERT_MANAGER_NAMESPACE);
    let secrets: Api<Secret> = Api::namespaced(context.client.clone(), &context.namespace);
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &context.namespace);

    let result = async {
        run_kubectl(["apply", "-f", manifest_url.as_str()].as_slice())
            .context("installing cert-manager and its CRDs")?;

        context
            .wait_until(
                "cert-manager deployments to become ready",
                Duration::from_secs(120),
                || {
                    let deployments = deployments.clone();
                    async move {
                        let deployments = deployments.list(&Default::default()).await?;
                        Ok(deployments.items.len() >= 3
                            && deployments.items.iter().all(|deployment| {
                                deployment
                                    .status
                                    .as_ref()
                                    .and_then(|status| status.available_replicas)
                                    .unwrap_or_default()
                                    >= 1
                            }))
                    }
                },
            )
            .await?;

        context
            .wait_until(
                "cert-manager CRDs to be established",
                Duration::from_secs(90),
                || {
                    let crd_api = crd_api.clone();
                    async move {
                        Ok(crd_api
                            .get_opt("clusterissuers.cert-manager.io")
                            .await?
                            .is_some_and(|crd| crd_is_established(&crd)))
                    }
                },
            )
            .await?;
        anyhow::ensure!(
            nodecontroller_pid() == Some(initial_pid),
            "nodecontroller restarted while cert-manager CRDs were being installed"
        );

        let issuer: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "cert-manager.io/v1",
            "kind": "ClusterIssuer",
            "metadata": {"name": issuer_name},
            "spec": {"selfSigned": {}}
        }))?;
        context
            .wait_until(
                "ClusterIssuer creation after its CRD became established",
                Duration::from_secs(30),
                || {
                    let issuers = issuers.clone();
                    let issuer = issuer.clone();
                    async move {
                        match issuers.create(&PostParams::default(), &issuer).await {
                            Ok(_) => Ok(true),
                            Err(kube::Error::Api(error)) if error.code == 409 => Ok(true),
                            Err(error) => {
                                tracing::debug!(error = ?error, "ClusterIssuer create is not ready; retrying");
                                Ok(false)
                            }
                        }
                    }
                },
            )
            .await
            .context("creating a ClusterIssuer immediately after its CRD became established")?;

        context
            .wait_until(
                "ClusterIssuer API to accept reads",
                Duration::from_secs(30),
                || {
                    let issuers = issuers.clone();
                    let issuer_name = issuer_name.clone();
                    async move { Ok(issuers.get_opt(&issuer_name).await?.is_some()) }
                },
            )
            .await?;

        let applied_issuer = issuers
            .patch(
                &issuer_name,
                &PatchParams::apply("nodebootstrap-e2e"),
                &Patch::Apply(json!({
                    "apiVersion": "cert-manager.io/v1",
                    "kind": "ClusterIssuer",
                    "metadata": {"name": issuer_name},
                    "spec": {"selfSigned": {}}
                })),
            )
            .await
            .context("applying a CRD-backed ClusterIssuer through server-side apply")?;
        anyhow::ensure!(
            applied_issuer.data.pointer("/spec/selfSigned").is_some(),
            "server-side apply did not preserve the CRD-backed ClusterIssuer spec"
        );
        anyhow::ensure!(
            applied_issuer
                .data
                .pointer("/metadata/managedFields")
                .and_then(Value::as_array)
                .is_some_and(|entries| {
                    entries.iter().any(|entry| {
                        entry.get("manager").and_then(Value::as_str) == Some("nodebootstrap-e2e")
                            && entry.get("operation").and_then(Value::as_str) == Some("Apply")
                    })
                }),
            "server-side apply did not record the CRD field manager"
        );

        let owner = configmaps
            .create(
                &PostParams::default(),
                &ConfigMap {
                    metadata: ObjectMeta {
                        name: Some(owner_name.to_string()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .await
            .context("creating the owner for a CRD-backed garbage-collection check")?;
        let owner_uid = owner
            .uid()
            .context("cert-manager e2e owner ConfigMap had no UID")?;
        let certificate: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "cert-manager.io/v1",
            "kind": "Certificate",
            "metadata": {
                "name": certificate_name,
                "ownerReferences": [{
                    "apiVersion": "v1",
                    "kind": "ConfigMap",
                    "name": owner_name,
                    "uid": owner_uid
                }]
            },
            "spec": {
                "secretName": secret_name,
                "commonName": "nodebootstrap-e2e.test",
                "issuerRef": {"name": issuer_name, "kind": "ClusterIssuer"}
            }
        }))?;
        certificates
            .create(&PostParams::default(), &certificate)
            .await
            .context("creating a Certificate through the newly-established cert-manager CRD")?;
        context
            .wait_until(
                "the CRD-backed Certificate to be readable",
                Duration::from_secs(30),
                || {
                    let certificates = certificates.clone();
                    async move { Ok(certificates.get_opt(certificate_name).await?.is_some()) }
                },
            )
            .await?;

        context
            .wait_until(
                "cert-manager to issue the test Certificate",
                Duration::from_secs(150),
                || {
                    let certificates = certificates.clone();
                    let secrets = secrets.clone();
                    async move {
                        let Some(certificate) = certificates.get_opt(certificate_name).await?
                        else {
                            return Ok(false);
                        };
                        Ok(ready_condition(&certificate.data)
                            && certificate_secret_is_complete(&secrets, secret_name).await?)
                    }
                },
            )
            .await?;

        configmaps
            .delete(owner_name, &DeleteParams::default())
            .await
            .context("deleting the Certificate owner to exercise live CRD garbage collection")?;
        context
            .wait_until(
                "nodecontroller to garbage-collect the CRD-backed Certificate",
                Duration::from_secs(90),
                || {
                    let certificates = certificates.clone();
                    async move { Ok(certificates.get_opt(certificate_name).await?.is_none()) }
                },
            )
            .await?;
        anyhow::ensure!(
            nodecontroller_pid() == Some(initial_pid),
            "nodecontroller restarted instead of refreshing CRD discovery"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = issuers.delete(&issuer_name, &DeleteParams::default()).await;
    let _ = configmaps
        .delete(owner_name, &DeleteParams::default())
        .await;
    let _ = secrets.delete(secret_name, &DeleteParams::default()).await;
    let _ = run_kubectl(
        [
            "delete",
            "-f",
            manifest_url.as_str(),
            "--ignore-not-found",
            "--wait=true",
            "--timeout=120s",
        ]
        .as_slice(),
    );
    result
}
