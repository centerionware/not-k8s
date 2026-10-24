//! Optional in-cluster addons that use standard Kubernetes APIs and run as Pods.

use anyhow::{Context, Result};
use base64::Engine as _;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};

use crate::config::{Config, DEFAULT_NODEACTIVATOR_IMAGE};

const METRICS_SERVER_VERSION: &str = "v0.9.0";
const NODEACTIVATOR_MANIFEST: &str =
    include_str!("../../../addons/nodeactivator/deploy/nodeactivator.yaml");

pub fn install(cfg: &Config) -> Result<()> {
    let kubeconfig = cfg.cluster_kubeconfig()?;
    let kubelet_ca = if cfg.install_metrics_server {
        Some(
            std::fs::read(cfg.nodelet_server_ca_path()).with_context(|| {
                format!(
                    "reading nodelet serving CA from {}",
                    cfg.nodelet_server_ca_path().display()
                )
            })?,
        )
    } else {
        None
    };
    let activator_image = cfg.nodeactivator_image.clone();
    crate::kube_api::block_on(&kubeconfig, move |client| async move {
        if let Some(ca) = kubelet_ca {
            let manifest =
                render_metrics_server(&base64::engine::general_purpose::STANDARD.encode(ca));
            crate::kube_api::apply_yaml(&client, &manifest, "nodebootstrap")
                .await
                .context("installing metrics-server")?;
            wait_for_deployment(&client, "metrics-server").await?;
            wait_for_api_service(&client, "v1beta1.metrics.k8s.io").await?;
        }
        if let Some(image) = activator_image {
            let (cert, key) = signer_ca(&client).await?;
            let manifest = render_nodeactivator(&image, &cert, &key)?;
            crate::kube_api::apply_yaml(&client, &manifest, "nodebootstrap")
                .await
                .context("installing nodeactivator")?;
            wait_for_deployment(&client, "nodeactivator-cert-signer").await?;
            wait_for_deployment(&client, "nodeactivator").await?;
            wait_for_api_service(&client, "v1beta1.external.metrics.k8s.io").await?;
        }
        Ok(())
    })
}

async fn wait_for_deployment(client: &kube::Client, name: &str) -> Result<()> {
    use k8s_openapi::api::{apps::v1::Deployment, core::v1::Pod};
    use kube::api::{Api, ListParams, LogParams};
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), "kube-system");
    let pods: Api<Pod> = Api::namespaced(client.clone(), "kube-system");
    let selector = format!("app.kubernetes.io/name={name}");
    let mut last_status = "Deployment has not been observed yet".to_string();
    for _ in 0..60 {
        if let Some(deployment) = deployments
            .get_opt(name)
            .await
            .context("checking addon Deployment readiness")?
        {
            let observed = deployment
                .status
                .as_ref()
                .and_then(|status| status.observed_generation)
                .unwrap_or_default();
            let generation = deployment.metadata.generation.unwrap_or_default();
            let available = deployment
                .status
                .as_ref()
                .and_then(|status| status.available_replicas)
                .unwrap_or_default();
            if observed >= generation && available > 0 {
                return Ok(());
            }
            last_status = format!(
                "observed_generation={observed}, generation={generation}, available_replicas={available}, status={:?}",
                deployment.status
            );
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    let mut diagnostics = format!("Deployment status: {last_status}");
    match pods.list(&ListParams::default().labels(&selector)).await {
        Ok(list) => {
            for pod in list.items {
                let pod_name = pod.metadata.name.as_deref().unwrap_or("<unnamed>");
                diagnostics.push_str(&format!("\nPod {pod_name}: {pod:?}"));
                match pods
                    .logs(
                        pod_name,
                        &LogParams {
                            tail_lines: Some(100),
                            timestamps: true,
                            ..Default::default()
                        },
                    )
                    .await
                {
                    Ok(logs) => {
                        diagnostics.push_str(&format!("\nLogs for Pod {pod_name}:\n{logs}"))
                    }
                    Err(error) => diagnostics.push_str(&format!(
                        "\nCould not read logs for Pod {pod_name}: {error:#}"
                    )),
                }
            }
        }
        Err(error) => diagnostics.push_str(&format!("\nCould not list addon Pods: {error:#}")),
    }
    anyhow::bail!(
        "Deployment/kube-system/{name} did not become Available within 5 minutes; {diagnostics}"
    )
}

async fn wait_for_api_service(client: &kube::Client, name: &str) -> Result<()> {
    use kube::{
        api::{Api, DynamicObject},
        core::GroupVersionKind,
        discovery::ApiResource,
    };
    let resource = ApiResource::from_gvk(&GroupVersionKind::gvk(
        "apiregistration.k8s.io",
        "v1",
        "APIService",
    ));
    let services: Api<DynamicObject> = Api::all_with(client.clone(), &resource);
    let mut last_status = "APIService has not been observed yet".to_string();
    for _ in 0..60 {
        if let Some(service) = services
            .get_opt(name)
            .await
            .context("checking aggregated API readiness")?
        {
            let conditions = service
                .data
                .pointer("/status/conditions")
                .and_then(serde_json::Value::as_array);
            if let Some(available) = conditions.and_then(|conditions| {
                conditions.iter().find(|condition| {
                    condition.get("type").and_then(serde_json::Value::as_str) == Some("Available")
                })
            }) {
                let status = available
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Unknown");
                let reason = available
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unspecified");
                let message = available
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("no condition message");
                last_status = format!("Available={status}, reason={reason}: {message}");
                if status == "True" {
                    return Ok(());
                }
            } else {
                last_status = "APIService has no Available condition".to_string();
            }
        } else {
            last_status = "APIService object is not present".to_string();
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    anyhow::bail!(
        "APIService/{name} did not become Available within 5 minutes; last observed status: {last_status}"
    )
}

async fn signer_ca(client: &kube::Client) -> Result<(String, String)> {
    use k8s_openapi::{ByteString, api::core::v1::Secret};
    use kube::api::{Api, ObjectMeta, PostParams};
    use std::collections::BTreeMap;

    let secrets: Api<Secret> = Api::namespaced(client.clone(), "kube-system");
    if let Some(secret) = secrets
        .get_opt("nodeactivator-signer-ca")
        .await
        .context("reading nodeactivator signer CA")?
    {
        let data = secret
            .data
            .context("nodeactivator signer CA Secret has no data")?;
        let cert = String::from_utf8(
            data.get("ca.crt")
                .context("signer CA Secret is missing ca.crt")?
                .0
                .clone(),
        )
        .context("signer CA certificate is not UTF-8 PEM")?;
        let key = String::from_utf8(
            data.get("ca.key")
                .context("signer CA Secret is missing ca.key")?
                .0
                .clone(),
        )
        .context("signer CA private key is not UTF-8 PEM")?;
        return Ok((cert, key));
    }
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let pair = KeyPair::generate()?;
    let certificate = params.self_signed(&pair)?;
    let cert = certificate.pem();
    let key = pair.serialize_pem();
    let mut data = BTreeMap::new();
    data.insert("ca.crt".into(), ByteString(cert.as_bytes().to_vec()));
    data.insert("ca.key".into(), ByteString(key.as_bytes().to_vec()));
    secrets
        .create(
            &PostParams::default(),
            &Secret {
                metadata: ObjectMeta {
                    name: Some("nodeactivator-signer-ca".into()),
                    namespace: Some("kube-system".into()),
                    ..Default::default()
                },
                data: Some(data),
                type_: Some("Opaque".into()),
                ..Default::default()
            },
        )
        .await
        .context("creating dedicated nodeactivator signer CA Secret")?;
    Ok((cert, key))
}

fn render_nodeactivator(image: &str, ca: &str, key: &str) -> Result<String> {
    anyhow::ensure!(
        !image.trim().is_empty() && !image.contains(['\n', '\r']),
        "nodeactivator image must be a single non-empty image reference"
    );
    let encoded_ca = base64::engine::general_purpose::STANDARD.encode(ca);
    let encoded_key = base64::engine::general_purpose::STANDARD.encode(key);
    let pull_policy = if image == DEFAULT_NODEACTIVATOR_IMAGE {
        "Always"
    } else {
        "IfNotPresent"
    };
    let mut manifest = NODEACTIVATOR_MANIFEST.replace("nodeactivator:latest", image);
    manifest = manifest.replace(
        "imagePullPolicy: IfNotPresent",
        &format!("imagePullPolicy: {pull_policy}"),
    );
    manifest = manifest.replace("REPLACE_WITH_BASE64_SERVING_CA", &encoded_ca);
    let secret = format!(
        "---\napiVersion: v1\nkind: Secret\nmetadata:\n  name: nodeactivator-signer-ca\n  namespace: kube-system\ntype: Opaque\ndata:\n  ca.crt: {encoded_ca}\n  ca.key: {encoded_key}\n"
    );
    Ok(format!("{secret}---\n{manifest}"))
}

fn render_metrics_server(ca_bundle: &str) -> String {
    format!(
        r#"apiVersion: v1
kind: ServiceAccount
metadata: {{name: metrics-server, namespace: kube-system}}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata: {{name: system:metrics-server}}
rules:
- {{apiGroups: [""], resources: ["pods", "nodes"], verbs: ["get", "list", "watch"]}}
- {{apiGroups: [""], resources: ["nodes/metrics", "nodes/stats"], verbs: ["get"]}}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata: {{name: system:metrics-server}}
roleRef: {{apiGroup: rbac.authorization.k8s.io, kind: ClusterRole, name: system:metrics-server}}
subjects: [{{kind: ServiceAccount, name: metrics-server, namespace: kube-system}}]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata: {{name: metrics-server:system:auth-delegator}}
roleRef: {{apiGroup: rbac.authorization.k8s.io, kind: ClusterRole, name: system:auth-delegator}}
subjects: [{{kind: ServiceAccount, name: metrics-server, namespace: kube-system}}]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata: {{name: metrics-server-auth-reader, namespace: kube-system}}
roleRef: {{apiGroup: rbac.authorization.k8s.io, kind: Role, name: extension-apiserver-authentication-reader}}
subjects: [{{kind: ServiceAccount, name: metrics-server, namespace: kube-system}}]
---
apiVersion: v1
kind: ConfigMap
metadata: {{name: metrics-server-kubelet-ca, namespace: kube-system}}
binaryData:
  ca.crt: {ca_bundle}
---
apiVersion: v1
kind: Service
metadata: {{name: metrics-server, namespace: kube-system}}
spec:
  selector: {{k8s-app: metrics-server}}
  ports: [{{name: https, port: 443, targetPort: https}}]
---
apiVersion: apps/v1
kind: Deployment
metadata: {{name: metrics-server, namespace: kube-system}}
spec:
  replicas: 1
  selector: {{matchLabels: {{k8s-app: metrics-server}}}}
  template:
    metadata: {{labels: {{k8s-app: metrics-server}}}}
    spec:
      serviceAccountName: metrics-server
      containers:
      - name: metrics-server
        image: registry.k8s.io/metrics-server/metrics-server:{METRICS_SERVER_VERSION}
        args: ["--cert-dir=/tmp", "--secure-port=10250", "--kubelet-preferred-address-types=InternalIP,ExternalIP,Hostname", "--kubelet-use-node-status-port", "--metric-resolution=15s", "--kubelet-certificate-authority=/var/run/metrics-server/kubelet-ca/ca.crt"]
        ports: [{{name: https, containerPort: 10250}}]
        volumeMounts: [{{name: tmp, mountPath: /tmp}}, {{name: kubelet-ca, mountPath: /var/run/metrics-server/kubelet-ca, readOnly: true}}]
        readinessProbe: {{httpGet: {{path: /readyz, port: https, scheme: HTTPS}}, initialDelaySeconds: 20}}
        livenessProbe: {{httpGet: {{path: /livez, port: https, scheme: HTTPS}}, initialDelaySeconds: 20}}
        resources: {{requests: {{cpu: 100m, memory: 200Mi}}}}
        securityContext: {{allowPrivilegeEscalation: false, readOnlyRootFilesystem: true, capabilities: {{drop: [ALL]}}}}
      volumes: [{{name: tmp, emptyDir: {{}}}}, {{name: kubelet-ca, configMap: {{name: metrics-server-kubelet-ca}}}}]
---
apiVersion: apiregistration.k8s.io/v1
kind: APIService
metadata: {{name: v1beta1.metrics.k8s.io}}
spec:
  group: metrics.k8s.io
  version: v1beta1
  groupPriorityMinimum: 100
  versionPriority: 100
  insecureSkipTLSVerify: true
  service: {{namespace: kube-system, name: metrics-server, port: 443}}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::{
        render_metrics_server, render_nodeactivator, DEFAULT_NODEACTIVATOR_IMAGE,
    };
    use base64::Engine as _;

    #[test]
    fn metrics_server_manifest_trusts_the_kubelet_ca() {
        let manifest = render_metrics_server("Y2EtcGVt");
        assert!(manifest.contains("metrics-server/metrics-server:v0.9.0"));
        assert!(manifest.contains("binaryData:\n  ca.crt: Y2EtcGVt"));
        assert!(
            manifest.contains(
                "--kubelet-certificate-authority=/var/run/metrics-server/kubelet-ca/ca.crt"
            )
        );
        assert!(manifest.contains("insecureSkipTLSVerify: true"));
        assert!(manifest.contains("name: metrics-server-auth-reader, namespace: kube-system"));
    }

    #[test]
    fn activator_uses_a_dedicated_signer_ca_and_proxy_ca_configmap() {
        let manifest = render_nodeactivator(
            "registry.example/activator:v1",
            "dedicated-ca",
            "dedicated-key",
        )
        .expect("render addon manifest");
        let encoded_ca = base64::engine::general_purpose::STANDARD.encode("dedicated-ca");
        let encoded_key = base64::engine::general_purpose::STANDARD.encode("dedicated-key");
        assert!(manifest.contains("image: registry.example/activator:v1"));
        assert!(manifest.contains(format!("ca.key: {encoded_key}\n---\napiVersion: v1").as_str()));
        assert!(manifest.contains(format!("caBundle: {encoded_ca}").as_str()));
        assert!(manifest.contains("name: extension-apiserver-authentication"));
        assert!(manifest.contains("resources: [\"signers\"]"));
        assert!(manifest.contains("resourceNames: [\"nodeactivator.notk8s.io/serving\"]"));
        assert!(manifest.contains("verbs: [\"sign\"]"));
        assert!(!manifest.contains("nodeapiserver-proxy-client-ca"));
        assert!(!manifest.contains("REPLACE_WITH_BASE64_SERVING_CA"));
    }

    #[test]
    fn default_activator_image_is_pulled_from_ghcr_and_custom_images_are_local_first() {
        let default_manifest = render_nodeactivator(
            DEFAULT_NODEACTIVATOR_IMAGE,
            "dedicated-ca",
            "dedicated-key",
        )
        .expect("render default addon manifest");
        assert!(default_manifest.contains(DEFAULT_NODEACTIVATOR_IMAGE));
        assert!(default_manifest.contains("imagePullPolicy: Always"));

        let custom_manifest = render_nodeactivator(
            "registry.example/activator:test",
            "dedicated-ca",
            "dedicated-key",
        )
        .expect("render custom addon manifest");
        assert!(custom_manifest.contains("imagePullPolicy: IfNotPresent"));
    }
}
