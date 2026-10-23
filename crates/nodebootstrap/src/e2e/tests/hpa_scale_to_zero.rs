use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use base64::Engine;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::api::{Api, ListParams, LogParams};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const BUSYBOX_IMAGE: &str = "busybox:1.36.1";

fn endpoint_slices_have_ready_endpoint(slices: &[EndpointSlice]) -> bool {
    slices.iter().any(|slice| {
        slice.endpoints.iter().any(|endpoint| {
            endpoint
                .conditions
                .as_ref()
                .is_none_or(|conditions| conditions.ready != Some(false))
        })
    })
}

fn hpa_is_scaled_to_zero(hpa: &HorizontalPodAutoscaler) -> bool {
    hpa.status.as_ref().is_some_and(|status| {
        status.conditions.as_ref().is_some_and(|conditions| {
            conditions
                .iter()
                .any(|condition| condition.type_ == "ScaledToZero" && condition.status == "True")
        })
    })
}

pub(super) async fn hpa_scale_to_zero_activator_full_flow(context: &E2eContext) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("HPA scale-to-zero flow requires the CRI runtime"));
    }
    for tool in ["docker", "ctr", "openssl", "kubectl"] {
        if Command::new(tool).arg("--version").output().is_err() {
            return Err(skip_test(format!("HPA scale-to-zero flow requires {tool}")));
        }
    }

    let work = TestWork::create(context)?;
    work.load_test_images()?;
    work.apply_metrics_service(context)?;
    work.create_tls_material()?;
    work.apply_fixture(context)?;

    let deployments: Api<Deployment> = Api::namespaced(context.client.clone(), &context.namespace);
    let hpas: Api<HorizontalPodAutoscaler> =
        Api::namespaced(context.client.clone(), &context.namespace);
    let ready = context
        .wait_until(
            "nodeactivator to become ready",
            Duration::from_secs(180),
            || {
                let deployments = deployments.clone();
                async move {
                    Ok(deployments
                        .get("nodeactivator")
                        .await?
                        .status
                        .and_then(|status| status.available_replicas)
                        .unwrap_or_default()
                        > 0)
                }
            },
        )
        .await;
    if let Err(error) = ready {
        let signer_pods_api: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
        let signer_pods = signer_pods_api
            .list(&ListParams::default().labels("app=nodeactivator-cert-signer"))
            .await
            .context("listing nodeactivator certificate signer Pods")?;
        let mut signer_logs = Vec::new();
        for pod in signer_pods.items {
            let Some(name) = pod.metadata.name else {
                continue;
            };
            for previous in [false, true] {
                let log = signer_pods_api
                    .logs(
                        &name,
                        &LogParams {
                            previous,
                            ..Default::default()
                        },
                    )
                    .await;
                let label = if previous { "previous" } else { "current" };
                signer_logs.push(match log {
                    Ok(log) => format!("pod/{name} {label} logs:\n{log}"),
                    Err(error) => format!("pod/{name} {label} log error: {error:#}"),
                });
            }
        }
        anyhow::bail!(
            "{error:#}; certificate signer logs:\n{}",
            signer_logs.join("\n"),
        );
    }
    wait_for_apiservice(context, Duration::from_secs(120)).await?;

    let backend = deployments.get("backend").await?;
    anyhow::ensure!(
        backend
            .spec
            .and_then(|spec| spec.replicas)
            .unwrap_or_default()
            == 0,
        "backend must start at zero replicas"
    );
    let hpa = hpas.get("backend").await?;
    anyhow::ensure!(
        hpa.spec.as_ref().and_then(|spec| spec.min_replicas) == Some(0),
        "backend HPA must allow zero replicas"
    );

    let backend_slices: Api<EndpointSlice> =
        Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until(
            "the backend Service to have no ready endpoints at zero replicas",
            Duration::from_secs(90),
            || {
                let backend_slices = backend_slices.clone();
                async move {
                    let items = backend_slices
                        .list(&ListParams::default().labels("kubernetes.io/service-name=backend"))
                        .await?
                        .items;
                    Ok(!items.is_empty() && !endpoint_slices_have_ready_endpoint(&items))
                }
            },
        )
        .await?;

    work.apply_caller(context)?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    context
        .wait_until(
            "the queued request to activate the backend",
            Duration::from_secs(240),
            || {
                let deployments = deployments.clone();
                async move {
                    Ok(deployments
                        .get("backend")
                        .await?
                        .spec
                        .and_then(|spec| spec.replicas)
                        .unwrap_or_default()
                        > 0)
                }
            },
        )
        .await?;

    context
        .wait_until(
            "the backend Service to have a ready endpoint after HPA scale-up",
            Duration::from_secs(180),
            || {
                let backend_slices = backend_slices.clone();
                async move {
                    let items = backend_slices
                        .list(&ListParams::default().labels("kubernetes.io/service-name=backend"))
                        .await?
                        .items;
                    Ok(endpoint_slices_have_ready_endpoint(&items))
                }
            },
        )
        .await?;

    context
        .wait_until(
            "the buffered request to reach the backend",
            Duration::from_secs(180),
            || {
                let pods = pods.clone();
                async move {
                    let Ok(log) = pods.logs("caller", &Default::default()).await else {
                        return Ok(false);
                    };
                    Ok(log.contains("SCALE_TO_ZERO_BACKEND_RESPONSE"))
                }
            },
        )
        .await?;
    let log = pods.logs("caller", &Default::default()).await?;
    anyhow::ensure!(
        log.contains("SCALE_TO_ZERO_BACKEND_RESPONSE"),
        "caller did not receive the backend response: {log:?}"
    );

    context
        .wait_until(
            "the idle backend to scale back to zero",
            Duration::from_secs(240),
            || {
                let deployments = deployments.clone();
                let backend_slices = backend_slices.clone();
                let hpas = hpas.clone();
                async move {
                    let backend = deployments.get("backend").await?;
                    let backend_at_zero = backend
                        .spec
                        .as_ref()
                        .and_then(|spec| spec.replicas)
                        .unwrap_or_default()
                        == 0
                        && backend.status.as_ref().is_some_and(|status| {
                            status.replicas == Some(0)
                                && status.ready_replicas.unwrap_or_default() == 0
                                && status.available_replicas.unwrap_or_default() == 0
                        });
                    let slices = backend_slices
                        .list(&ListParams::default().labels("kubernetes.io/service-name=backend"))
                        .await?
                        .items;
                    let hpa = hpas.get("backend").await?;
                    Ok(backend_at_zero
                        && !slices.is_empty()
                        && !endpoint_slices_have_ready_endpoint(&slices)
                        && hpa
                            .status
                            .as_ref()
                            .is_some_and(|status| status.desired_replicas == 0)
                        && hpa_is_scaled_to_zero(&hpa))
                }
            },
        )
        .await?;
    Ok(())
}

async fn wait_for_apiservice(context: &E2eContext, timeout: Duration) -> Result<()> {
    let result = context
        .wait_until(
            "the external metrics APIService to become available",
            timeout,
            || async {
                let output = Command::new("kubectl")
                    .args([
                        "--request-timeout=10s",
                        "get",
                        "apiservice",
                        "v1beta1.external.metrics.k8s.io",
                        "-o",
                        "jsonpath={.status.conditions[?(@.type==\"Available\")].status}",
                    ])
                    .output()
                    .context("checking external metrics APIService")?;
                Ok(output.status.success()
                    && String::from_utf8_lossy(&output.stdout).trim() == "True")
            },
        )
        .await;
    if let Err(error) = result {
        let apiservice = Command::new("kubectl")
            .args([
                "--request-timeout=10s",
                "get",
                "apiservice",
                "v1beta1.external.metrics.k8s.io",
                "-o",
                "yaml",
            ])
            .output()
            .context("getting external metrics APIService diagnostics")?;
        let addon_logs = Command::new("kubectl")
            .args([
                "--request-timeout=10s",
                "logs",
                "deployment/nodeactivator",
                "--namespace",
                &context.namespace,
                "--tail=100",
                "--timestamps",
            ])
            .output()
            .context("getting nodeactivator logs")?;
        let signer_logs = Command::new("kubectl")
            .args([
                "--request-timeout=10s",
                "logs",
                "deployment/nodeactivator-cert-signer",
                "--namespace",
                &context.namespace,
                "--tail=100",
                "--timestamps",
            ])
            .output()
            .context("getting nodeactivator certificate signer logs")?;
        anyhow::bail!(
            "{error:#}; APIService state:\n{}{}\nnodeactivator logs:\n{}{}\ncertificate signer logs:\n{}{}",
            String::from_utf8_lossy(&apiservice.stdout),
            String::from_utf8_lossy(&apiservice.stderr),
            String::from_utf8_lossy(&addon_logs.stdout),
            String::from_utf8_lossy(&addon_logs.stderr),
            String::from_utf8_lossy(&signer_logs.stdout),
            String::from_utf8_lossy(&signer_logs.stderr),
        );
    }
    Ok(())
}

struct TestWork {
    dir: PathBuf,
    activator_image: String,
    signer_image: String,
}

impl TestWork {
    fn create(context: &E2eContext) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "nodeactivator-e2e-{}-{}",
            std::process::id(),
            context.namespace
        ));
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self {
            dir,
            activator_image: format!("nodeactivator-e2e:{}", context.namespace),
            signer_image: format!("nodeactivator-cert-signer-e2e:{}", context.namespace),
        })
    }

    fn load_test_images(&self) -> Result<()> {
        let binary = Path::new("dist-bin/nodeactivator");
        let signer_binary = Path::new("dist-bin/nodeactivator-cert-signer");
        anyhow::ensure!(
            binary.is_file(),
            "e2e artifact is missing {}",
            binary.display()
        );
        anyhow::ensure!(
            signer_binary.is_file(),
            "e2e artifact is missing {}",
            signer_binary.display()
        );
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let output = Command::new("docker")
            .current_dir(&repo)
            .args([
                "build",
                "--file",
                "addons/nodeactivator/e2e.Dockerfile",
                "--tag",
                &self.activator_image,
                ".",
            ])
            .output()
            .context("building the e2e nodeactivator image")?;
        anyhow::ensure!(
            output.status.success(),
            "docker build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = Command::new("docker")
            .current_dir(&repo)
            .args([
                "build",
                "--file",
                "addons/nodeactivator/e2e-cert-signer.Dockerfile",
                "--tag",
                &self.signer_image,
                ".",
            ])
            .output()
            .context("building the e2e nodeactivator certificate signer image")?;
        anyhow::ensure!(
            output.status.success(),
            "docker build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        root_run(&["docker", "pull", BUSYBOX_IMAGE])?;
        let archive = self.dir.join("images.tar");
        let archive_string = archive.to_string_lossy().into_owned();
        root_run(&[
            "docker",
            "save",
            "--output",
            &archive_string,
            BUSYBOX_IMAGE,
            &self.activator_image,
            &self.signer_image,
        ])?;
        root_run(&["ctr", "-n", "k8s.io", "images", "import", &archive_string])?;
        Ok(())
    }

    fn apply_metrics_service(&self, context: &E2eContext) -> Result<()> {
        apply_yaml(&format!(
            "apiVersion: v1\nkind: Service\nmetadata:\n  name: nodeactivator-metrics\n  namespace: {}\nspec:\n  selector:\n    app.kubernetes.io/name: nodeactivator\n  ports:\n  - name: https\n    port: 443\n    targetPort: https\n",
            context.namespace
        ))
    }

    fn create_tls_material(&self) -> Result<()> {
        let serving_ca_cert = self.dir.join("tls-ca.crt");
        let serving_ca_key = self.dir.join("tls-ca.key");
        run_command(
            "openssl",
            &[
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "365",
                "-keyout",
                serving_ca_key
                    .to_str()
                    .context("serving CA key path is not UTF-8")?,
                "-out",
                serving_ca_cert
                    .to_str()
                    .context("serving CA cert path is not UTF-8")?,
                "-subj",
                "/CN=nodeactivator-e2e-ca",
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-addext",
                "keyUsage=critical,keyCertSign,cRLSign",
            ],
        )?;
        Ok(())
    }

    fn apply_fixture(&self, context: &E2eContext) -> Result<()> {
        let serving_ca = fs::read(self.dir.join("tls-ca.crt"))?;
        let serving_ca_key = fs::read(self.dir.join("tls-ca.key"))?;
        let proxy_ca = fs::read(kube_pki_dir().join("front-proxy-ca.crt"))?;
        let yaml = fixture_yaml(
            &context.namespace,
            &self.activator_image,
            &self.signer_image,
            &base64::engine::general_purpose::STANDARD.encode(serving_ca_key),
            &base64::engine::general_purpose::STANDARD.encode(proxy_ca),
            &base64::engine::general_purpose::STANDARD.encode(serving_ca),
        );
        let mut command = Command::new("kubectl");
        command.args(["apply", "-f", "-"]).stdin(Stdio::piped());
        let mut child = command.spawn().context("starting kubectl apply")?;
        use std::io::Write;
        child
            .stdin
            .take()
            .context("kubectl apply stdin was not available")?
            .write_all(yaml.as_bytes())?;
        let output = child.wait_with_output()?;
        anyhow::ensure!(
            output.status.success(),
            "applying HPA e2e fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    fn apply_caller(&self, context: &E2eContext) -> Result<()> {
        let yaml = format!(
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: caller\n  namespace: {}\nspec:\n  restartPolicy: Never\n  containers:\n  - name: caller\n    image: {}\n    imagePullPolicy: Never\n    command: [\"/bin/sh\", \"-c\", \"wget -T 210 -qO- http://front.{}.svc/; echo REQUEST_FINISHED; sleep 3600\"]\n",
            context.namespace, BUSYBOX_IMAGE, context.namespace
        );
        apply_yaml(&yaml)
    }
}

impl Drop for TestWork {
    fn drop(&mut self) {
        let archive = self.dir.join("images.tar");
        let _ = fs::remove_file(archive);
        let _ = fs::remove_dir_all(&self.dir);
        let _ = Command::new("docker")
            .args(["image", "rm", &self.activator_image])
            .output();
        let _ = Command::new("docker")
            .args(["image", "rm", &self.signer_image])
            .output();
        let _ = Command::new("kubectl")
            .args([
                "delete",
                "apiservice",
                "v1beta1.external.metrics.k8s.io",
                "--ignore-not-found=true",
            ])
            .output();
        for resource in [
            ["clusterrolebinding", "nodeactivator-e2e-services"],
            ["clusterrolebinding", "nodeactivator-e2e-auth-delegator"],
            ["clusterrole", "nodeactivator-e2e-services"],
        ] {
            let _ = Command::new("kubectl")
                .args([
                    "delete",
                    resource[0],
                    resource[1],
                    "--ignore-not-found=true",
                ])
                .output();
        }
        for resource in [
            ["rolebinding", "nodeactivator-e2e-signer"],
            ["role", "nodeactivator-e2e-signer"],
            ["clusterrolebinding", "nodeactivator-e2e-signer-nodes"],
            ["clusterrole", "nodeactivator-e2e-signer-nodes"],
        ] {
            let _ = Command::new("kubectl")
                .args([
                    "delete",
                    resource[0],
                    resource[1],
                    "--ignore-not-found=true",
                ])
                .output();
        }
    }
}

fn fixture_yaml(
    namespace: &str,
    image: &str,
    signer_image: &str,
    signer_ca_key: &str,
    proxy_ca: &str,
    serving_ca: &str,
) -> String {
    format!(
        r#"
apiVersion: v1
kind: Secret
metadata: {{name: nodeactivator-signer-ca, namespace: {namespace}}}
data: {{ca.crt: {serving_ca}, ca.key: {signer_ca_key}}}
---
apiVersion: v1
kind: Secret
metadata: {{name: nodeapiserver-proxy-client-ca, namespace: {namespace}}}
data: {{"ca.crt": {proxy_ca}}}
---
apiVersion: v1
kind: ServiceAccount
metadata: {{name: nodeactivator, namespace: {namespace}}}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata: {{name: nodeactivator-e2e-signer, namespace: {namespace}}}
rules:
- apiGroups: ["certificates.k8s.io"]
  resources: ["podcertificaterequests", "podcertificaterequests/status"]
  verbs: ["get", "list", "watch", "patch", "update"]
- apiGroups: [""]
  resources: ["pods", "services", "serviceaccounts"]
  verbs: ["get"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata: {{name: nodeactivator-e2e-signer-nodes}}
rules:
- apiGroups: [""]
  resources: ["nodes"]
  verbs: ["get"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata: {{name: nodeactivator-e2e-signer-nodes}}
roleRef: {{apiGroup: rbac.authorization.k8s.io, kind: ClusterRole, name: nodeactivator-e2e-signer-nodes}}
subjects:
- {{kind: ServiceAccount, name: nodeactivator-cert-signer, namespace: {namespace}}}
---
apiVersion: v1
kind: ServiceAccount
metadata: {{name: nodeactivator-cert-signer, namespace: {namespace}}}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata: {{name: nodeactivator-e2e-signer, namespace: {namespace}}}
roleRef: {{apiGroup: rbac.authorization.k8s.io, kind: Role, name: nodeactivator-e2e-signer}}
subjects:
- {{kind: ServiceAccount, name: nodeactivator-cert-signer, namespace: {namespace}}}
---
apiVersion: apps/v1
kind: Deployment
metadata: {{name: nodeactivator-cert-signer, namespace: {namespace}}}
spec:
  replicas: 1
  selector: {{matchLabels: {{app: nodeactivator-cert-signer}}}}
  template:
    metadata: {{labels: {{app: nodeactivator-cert-signer}}}}
    spec:
      serviceAccountName: nodeactivator-cert-signer
      containers:
      - name: signer
        image: {signer_image}
        imagePullPolicy: Never
        env:
        - name: POD_NAMESPACE
          valueFrom: {{fieldRef: {{fieldPath: metadata.namespace}}}}
        - name: NODEACTIVATOR_SIGNER_CA_CERT
          value: /signer-ca/ca.crt
        - name: NODEACTIVATOR_SIGNER_CA_KEY
          value: /signer-ca/ca.key
        volumeMounts:
        - {{name: signer-ca, mountPath: /signer-ca, readOnly: true}}
      volumes:
      - {{name: signer-ca, secret: {{secretName: nodeactivator-signer-ca}}}}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata: {{name: nodeactivator-e2e-services}}
rules:
- apiGroups: [""]
  resources: ["services"]
  verbs: ["list", "watch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata: {{name: nodeactivator-e2e-services}}
roleRef: {{apiGroup: rbac.authorization.k8s.io, kind: ClusterRole, name: nodeactivator-e2e-services}}
subjects:
- {{kind: ServiceAccount, name: nodeactivator, namespace: {namespace}}}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata: {{name: nodeactivator-e2e-auth-delegator}}
roleRef: {{apiGroup: rbac.authorization.k8s.io, kind: ClusterRole, name: system:auth-delegator}}
subjects:
- {{kind: ServiceAccount, name: nodeactivator, namespace: {namespace}}}
---
apiVersion: apps/v1
kind: Deployment
metadata: {{name: nodeactivator, namespace: {namespace}}}
spec:
  replicas: 1
  selector: {{matchLabels: {{app: nodeactivator-e2e}}}}
  template:
    metadata:
      labels:
        app: nodeactivator-e2e
        app.kubernetes.io/name: nodeactivator
    spec:
      serviceAccountName: nodeactivator
      containers:
      - name: nodeactivator
        image: {image}
        imagePullPolicy: Never
        env:
        - {{name: ACTIVATOR_TLS_BUNDLE, value: /tls/credential.pem}}
        - {{name: ACTIVATOR_PROXY_CLIENT_CA, value: /proxy-ca/ca.crt}}
        - {{name: ACTIVATOR_TIMEOUT_SECONDS, value: "180"}}
        ports:
        - {{name: http, containerPort: 8080}}
        - {{name: https, containerPort: 8443}}
        volumeMounts:
        - {{name: tls, mountPath: /tls, readOnly: true}}
        - {{name: proxy-ca, mountPath: /proxy-ca, readOnly: true}}
        readinessProbe: {{httpGet: {{path: /readyz, port: http}}, periodSeconds: 3}}
      volumes:
      - name: tls
        projected:
          sources:
          - podCertificate:
              signerName: nodeactivator.notk8s.io/serving
              keyType: ECDSAP256
              maxExpirationSeconds: 86400
              credentialBundlePath: credential.pem
      - {{name: proxy-ca, secret: {{secretName: nodeapiserver-proxy-client-ca}}}}
---
apiVersion: v1
kind: Service
metadata: {{name: nodeactivator-metrics, namespace: {namespace}}}
spec:
  selector: {{app.kubernetes.io/name: nodeactivator}}
  ports:
  - {{name: https, port: 443, targetPort: https}}
---
apiVersion: apiregistration.k8s.io/v1
kind: APIService
metadata: {{name: v1beta1.external.metrics.k8s.io}}
spec:
  group: external.metrics.k8s.io
  version: v1beta1
  groupPriorityMinimum: 100
  versionPriority: 100
  service: {{namespace: {namespace}, name: nodeactivator-metrics, port: 443}}
  caBundle: {serving_ca}
---
apiVersion: v1
kind: ConfigMap
metadata: {{name: backend-content, namespace: {namespace}}}
data:
  index.html: SCALE_TO_ZERO_BACKEND_RESPONSE
---
apiVersion: apps/v1
kind: Deployment
metadata: {{name: backend, namespace: {namespace}}}
spec:
  replicas: 0
  selector: {{matchLabels: {{app: backend}}}}
  template:
    metadata: {{labels: {{app: backend}}}}
    spec:
      containers:
      - name: backend
        image: {busybox}
        imagePullPolicy: Never
        command: ["httpd", "-f", "-p", "8080", "-h", "/www"]
        ports: [{{name: http, containerPort: 8080}}]
        volumeMounts: [{{name: content, mountPath: /www, readOnly: true}}]
      volumes: [{{name: content, configMap: {{name: backend-content}}}}]
---
apiVersion: v1
kind: Service
metadata: {{name: backend, namespace: {namespace}}}
spec:
  selector: {{app: backend}}
  ports: [{{port: 80, targetPort: 8080}}]
---
apiVersion: v1
kind: Service
metadata:
  name: front
  namespace: {namespace}
  labels: {{activator.notk8s.io/enabled: "true"}}
  annotations: {{activator.notk8s.io/backend-url: "http://backend.{namespace}.svc.cluster.local:80"}}
spec:
  selector: {{app: nodeactivator-e2e}}
  ports: [{{port: 80, targetPort: http}}]
---
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata: {{name: backend, namespace: {namespace}}}
spec:
  scaleTargetRef: {{apiVersion: apps/v1, kind: Deployment, name: backend}}
  minReplicas: 0
  maxReplicas: 1
  metrics:
  - type: External
    external:
      metric:
        name: activator_requests_active
        selector: {{matchLabels: {{service: front}}}}
      target: {{type: Value, value: "1"}}
  behavior:
    scaleDown:
      stabilizationWindowSeconds: 15
"#,
        busybox = BUSYBOX_IMAGE
    )
}

fn apply_yaml(yaml: &str) -> Result<()> {
    let mut command = Command::new("kubectl");
    command.args(["apply", "-f", "-"]).stdin(Stdio::piped());
    let mut child = command.spawn().context("starting kubectl apply")?;
    use std::io::Write;
    child
        .stdin
        .take()
        .context("kubectl apply stdin was not available")?
        .write_all(yaml.as_bytes())?;
    let output = child.wait_with_output()?;
    anyhow::ensure!(
        output.status.success(),
        "kubectl apply failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn kube_pki_dir() -> PathBuf {
    std::env::var_os("NODEBOOTSTRAP_PKI_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/nodebootstrap/pki"))
}

fn root_run(args: &[&str]) -> Result<()> {
    let output = if Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "0")
    {
        Command::new(args[0]).args(&args[1..]).output()?
    } else {
        Command::new("sudo").arg("-n").args(args).output()?
    };
    anyhow::ensure!(
        output.status.success(),
        "{} failed: {}",
        args[0],
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn run_command(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program).args(args).output()?;
    anyhow::ensure!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
