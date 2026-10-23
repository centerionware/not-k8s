use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use base64::Engine;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Pod;
use kube::api::Api;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const BUSYBOX_IMAGE: &str = "busybox:1.36.1";

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
    work.create_tls_material(context)?;
    work.apply_fixture(context)?;

    let deployments: Api<Deployment> = Api::namespaced(context.client.clone(), &context.namespace);
    context
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
        .await?;
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
                async move {
                    Ok(deployments
                        .get("backend")
                        .await?
                        .spec
                        .and_then(|spec| spec.replicas)
                        .unwrap_or_default()
                        == 0)
                }
            },
        )
        .await?;
    Ok(())
}

async fn wait_for_apiservice(context: &E2eContext, timeout: Duration) -> Result<()> {
    context
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
        .await
}

struct TestWork {
    dir: PathBuf,
    activator_image: String,
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
        })
    }

    fn load_test_images(&self) -> Result<()> {
        let binary = Path::new("dist-bin/nodeactivator");
        anyhow::ensure!(
            binary.is_file(),
            "e2e artifact is missing {}",
            binary.display()
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
        ])?;
        root_run(&["ctr", "-n", "k8s.io", "images", "import", &archive_string])?;
        Ok(())
    }

    fn create_tls_material(&self, context: &E2eContext) -> Result<()> {
        let cert = self.dir.join("tls.crt");
        let key = self.dir.join("tls.key");
        let ca = kube_pki_dir().join("front-proxy-ca.crt");
        anyhow::ensure!(
            ca.is_file(),
            "front proxy CA is missing at {}",
            ca.display()
        );
        run_command(
            "openssl",
            &[
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-keyout",
                key.to_str().context("TLS key path is not UTF-8")?,
                "-out",
                cert.to_str().context("TLS cert path is not UTF-8")?,
                "-subj",
                "/CN=nodeactivator-metrics",
                "-addext",
                &format!(
                    "subjectAltName=DNS:nodeactivator-metrics.{0}.svc,DNS:nodeactivator-metrics.{0}.svc.cluster.local",
                    context.namespace
                ),
            ],
        )?;
        Ok(())
    }

    fn apply_fixture(&self, context: &E2eContext) -> Result<()> {
        let cert = fs::read(self.dir.join("tls.crt"))?;
        let key = fs::read(self.dir.join("tls.key"))?;
        let proxy_ca = fs::read(kube_pki_dir().join("front-proxy-ca.crt"))?;
        let yaml = fixture_yaml(
            &context.namespace,
            &self.activator_image,
            &base64::engine::general_purpose::STANDARD.encode(cert),
            &base64::engine::general_purpose::STANDARD.encode(key),
            &base64::engine::general_purpose::STANDARD.encode(proxy_ca),
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
    }
}

fn fixture_yaml(namespace: &str, image: &str, cert: &str, key: &str, proxy_ca: &str) -> String {
    format!(
        r#"
apiVersion: v1
kind: Secret
metadata: {{name: nodeactivator-serving, namespace: {namespace}}}
type: kubernetes.io/tls
data: {{tls.crt: {cert}, tls.key: {key}}}
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
    metadata: {{labels: {{app: nodeactivator-e2e}}}}
    spec:
      serviceAccountName: nodeactivator
      containers:
      - name: nodeactivator
        image: {image}
        imagePullPolicy: Never
        env:
        - {{name: ACTIVATOR_TLS_CERT, value: /tls/tls.crt}}
        - {{name: ACTIVATOR_TLS_KEY, value: /tls/tls.key}}
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
      - {{name: tls, secret: {{secretName: nodeactivator-serving}}}}
      - {{name: proxy-ca, secret: {{secretName: nodeapiserver-proxy-client-ca}}}}
---
apiVersion: v1
kind: Service
metadata: {{name: nodeactivator-metrics, namespace: {namespace}}}
spec:
  selector: {{app: nodeactivator-e2e}}
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
  caBundle: {cert}
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
