use anyhow::{Context, Result, anyhow, ensure};
use base64::Engine;
use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Node, Pod, Service, ServiceAccount};
use kube::api::{Api, Patch, PatchParams};
use kube::core::{ApiResource, DynamicObject};
use kube::runtime::watcher::{self, Event};
use kube::{Client, ResourceExt};
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{info, warn};

const SIGNER_NAME: &str = "nodeactivator.notk8s.io/serving";
const NODEACTIVATOR_LABEL: &str = "app.kubernetes.io/name";
const NODEACTIVATOR_LABEL_VALUE: &str = "nodeactivator";
const SERVICE_NAME: &str = "nodeactivator-metrics";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    info!("starting nodeactivator PodCertificate signer");
    rustls::crypto::ring::default_provider()
        .install_default()
        .context("installing the rustls ring crypto provider")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building nodeactivator certificate signer runtime")?;
    runtime.block_on(run_from_environment())
}

async fn run_from_environment() -> Result<()> {
    let ca_cert = env::var("NODEACTIVATOR_SIGNER_CA_CERT")
        .unwrap_or_else(|_| "/var/run/nodeactivator-signer/ca.crt".to_string());
    let ca_key = env::var("NODEACTIVATOR_SIGNER_CA_KEY")
        .unwrap_or_else(|_| "/var/run/nodeactivator-signer/ca.key".to_string());
    info!("loading in-cluster Kubernetes client");
    let client = Client::try_default()
        .await
        .context("loading in-cluster Kubernetes client")?;
    info!("loaded in-cluster Kubernetes client");
    run(client, &ca_cert, &ca_key).await
}

async fn run(client: Client, ca_cert: &str, ca_key: &str) -> Result<()> {
    let namespace = env::var("POD_NAMESPACE").context("POD_NAMESPACE must be set")?;
    info!(namespace, "discovering PodCertificateRequest API");
    let resource = pod_certificate_resource(&client).await?;
    info!(api_version = %resource.api_version, "watching PodCertificateRequests");
    let requests: Api<DynamicObject> = Api::namespaced_with(client.clone(), &namespace, &resource);
    let config = watcher::Config::default();
    let mut stream = Box::pin(watcher::watcher(requests, config));
    loop {
        match stream.next().await {
            Some(Ok(Event::Apply(request))) | Some(Ok(Event::InitApply(request))) => {
                if request
                    .data
                    .pointer("/spec/signerName")
                    .and_then(Value::as_str)
                    != Some(SIGNER_NAME)
                {
                    continue;
                }
                if let Err(error) =
                    issue_for_request(&client, &resource, &request, ca_cert, ca_key).await
                {
                    warn!(
                        request = %request.name_any(),
                        error = ?error,
                        "failed to issue PodCertificateRequest"
                    );
                    if let Err(status_error) =
                        mark_failed(&client, &resource, &request, &error).await
                    {
                        warn!(
                            request = %request.name_any(),
                            error = ?status_error,
                            "failed to record PodCertificateRequest failure"
                        );
                    }
                }
            }
            Some(Ok(Event::Delete(_))) | Some(Ok(Event::Init)) | Some(Ok(Event::InitDone)) => {}
            Some(Err(error)) => warn!(error = ?error, "PodCertificateRequest watch failed"),
            None => return Err(anyhow!("PodCertificateRequest watch ended")),
        }
    }
}

async fn mark_failed(
    client: &Client,
    resource: &ApiResource,
    request: &DynamicObject,
    error: &anyhow::Error,
) -> Result<()> {
    let namespace = request
        .namespace()
        .context("PodCertificateRequest has no namespace")?;
    let requests: Api<DynamicObject> = Api::namespaced_with(client.clone(), &namespace, resource);
    requests
        .patch_status(
            &request.name_any(),
            &PatchParams::default(),
            &Patch::Merge(json!({
                "status": {
                    "conditions": [{
                        "type": "Failed",
                        "status": "True",
                        "lastTransitionTime": Utc::now().to_rfc3339(),
                        "reason": "SignerError",
                        "message": error.to_string()
                    }]
                }
            })),
        )
        .await
        .context("writing PodCertificateRequest failure status")?;
    Ok(())
}

async fn pod_certificate_resource(client: &Client) -> Result<ApiResource> {
    for version in ["v1", "v1alpha1"] {
        let group_version = format!("certificates.k8s.io/{version}");
        if let Ok(resources) = client.list_api_group_resources(&group_version).await {
            if resources
                .resources
                .iter()
                .any(|resource| resource.name == "podcertificaterequests")
            {
                return Ok(ApiResource {
                    group: "certificates.k8s.io".to_string(),
                    version: version.to_string(),
                    api_version: group_version,
                    kind: "PodCertificateRequest".to_string(),
                    plural: "podcertificaterequests".to_string(),
                });
            }
        }
    }
    anyhow::bail!("API server does not serve PodCertificateRequests in v1 or v1alpha1")
}

async fn issue_for_request(
    client: &Client,
    resource: &ApiResource,
    request: &DynamicObject,
    ca_cert: &str,
    ca_key: &str,
) -> Result<()> {
    if request
        .data
        .pointer("/status/certificateChain")
        .and_then(Value::as_str)
        .is_some()
    {
        return Ok(());
    }

    let spec = request
        .data
        .get("spec")
        .context("PodCertificateRequest has no spec")?;
    ensure!(
        spec.get("signerName").and_then(Value::as_str) == Some(SIGNER_NAME),
        "request uses an unsupported signer"
    );
    let namespace = request
        .namespace()
        .context("PodCertificateRequest has no namespace")?;
    let pod_name = required_string(spec, "podName")?;
    let pod_uid = required_string(spec, "podUID")?;
    let service_account = required_string(spec, "serviceAccountName")?;
    ensure!(
        service_account == "nodeactivator",
        "request is not from the nodeactivator ServiceAccount"
    );

    let pods: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    let pod = pods
        .get(&pod_name)
        .await
        .context("reading requesting Pod")?;
    ensure!(
        pod.metadata.uid.as_deref() == Some(pod_uid),
        "PodCertificateRequest UID does not match the live Pod"
    );
    ensure!(
        pod.spec
            .as_ref()
            .and_then(|pod_spec| pod_spec.service_account_name.as_deref())
            == Some(service_account),
        "requesting Pod has a different ServiceAccount"
    );
    let service_accounts: Api<ServiceAccount> = Api::namespaced(client.clone(), &namespace);
    let service_account_object = service_accounts
        .get(service_account)
        .await
        .context("reading requesting ServiceAccount")?;
    ensure!(
        service_account_object.metadata.uid.as_deref()
            == Some(required_string(spec, "serviceAccountUID")?),
        "PodCertificateRequest ServiceAccount UID does not match the live ServiceAccount"
    );
    let requested_node_name = required_string(spec, "nodeName")?;
    ensure!(
        pod.spec
            .as_ref()
            .and_then(|pod_spec| pod_spec.node_name.as_deref())
            == Some(requested_node_name),
        "PodCertificateRequest node does not match the live Pod assignment"
    );
    let nodes: Api<Node> = Api::all(client.clone());
    let node = nodes
        .get(requested_node_name)
        .await
        .context("reading requesting Node")?;
    ensure!(
        node.metadata.uid.as_deref() == Some(required_string(spec, "nodeUID")?),
        "PodCertificateRequest Node UID does not match the live Node"
    );
    ensure!(
        pod.labels()
            .get(NODEACTIVATOR_LABEL)
            .is_some_and(|value| value == NODEACTIVATOR_LABEL_VALUE),
        "requesting Pod is not a nodeactivator Pod"
    );

    let services: Api<Service> = Api::namespaced(client.clone(), &namespace);
    let service = services
        .get(SERVICE_NAME)
        .await
        .context("reading nodeactivator metrics Service")?;
    let selector = service
        .spec
        .as_ref()
        .and_then(|spec| spec.selector.as_ref())
        .context("metrics Service has no selector")?;
    ensure!(
        selector.get(NODEACTIVATOR_LABEL).map(String::as_str) == Some(NODEACTIVATOR_LABEL_VALUE),
        "metrics Service selector does not identify nodeactivator Pods"
    );
    ensure!(
        selector.iter().all(|(key, value)| pod
            .labels()
            .get(key)
            .is_some_and(|pod_value| pod_value == value)),
        "requesting Pod is not selected by the metrics Service"
    );
    let service_ip = service
        .spec
        .as_ref()
        .and_then(|spec| spec.cluster_ip.as_deref())
        .context("metrics Service has no ClusterIP")?
        .parse::<IpAddr>()
        .context("metrics Service ClusterIP is not a valid IP address")?;
    let stable_csr = spec.get("stubPKCS10Request").and_then(Value::as_str);
    let alpha_public_key = spec.get("pkixPublicKey").and_then(Value::as_str);
    ensure!(
        stable_csr.is_some() ^ alpha_public_key.is_some(),
        "PodCertificateRequest must contain exactly one supported public-key request field"
    );

    let max_expiration = spec
        .get("maxExpirationSeconds")
        .and_then(Value::as_u64)
        .unwrap_or(86_400)
        .min(86_400);
    ensure!(
        max_expiration == 86_400,
        "nodeactivator signer supports a 24-hour certificate lifetime"
    );
    let work_dir = RequestWorkDir::create(&pod_uid, &request.name_any())?;
    let request_path = work_dir.path.join(if stable_csr.is_some() {
        "request.der"
    } else {
        "public-key.der"
    });
    let public_key_pem = work_dir.path.join("public-key.pem");
    let extensions_path = work_dir.path.join("server-ext.cnf");
    let cert_path = work_dir.path.join("issued.crt");
    let serial_path = work_dir.path.join("ca.srl");
    let request_bytes = base64::engine::general_purpose::STANDARD
        .decode(
            stable_csr
                .or(alpha_public_key)
                .context("missing PodCertificateRequest public key")?,
        )
        .context("decoding PodCertificateRequest public-key material")?;
    fs::write(&request_path, request_bytes)?;
    fs::write(
        &extensions_path,
        format!(
            "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:{SERVICE_NAME}.{namespace}.svc,DNS:{SERVICE_NAME}.{namespace}.svc.cluster.local,IP:{service_ip}\n"
        ),
    )?;
    let mut openssl_args = vec!["x509"];
    if stable_csr.is_some() {
        run_openssl(&[
            "req",
            "-verify",
            "-inform",
            "DER",
            "-in",
            path_arg(&request_path)?,
            "-noout",
        ])?;
        openssl_args.extend(["-req", "-inform", "DER", "-in"]);
    } else {
        run_openssl(&[
            "pkey",
            "-pubin",
            "-inform",
            "DER",
            "-in",
            path_arg(&request_path)?,
            "-out",
            path_arg(&public_key_pem)?,
        ])?;
        let proof = spec
            .get("proofOfPossession")
            .and_then(Value::as_str)
            .context("alpha PodCertificateRequest has no proof of possession")?;
        let proof_path = work_dir.path.join("proof.bin");
        let pod_uid_path = work_dir.path.join("pod-uid.txt");
        fs::write(
            &proof_path,
            base64::engine::general_purpose::STANDARD
                .decode(proof)
                .context("decoding alpha PodCertificateRequest proof of possession")?,
        )?;
        fs::write(&pod_uid_path, pod_uid.as_bytes())?;
        run_openssl(&[
            "dgst",
            "-sha256",
            "-verify",
            path_arg(&public_key_pem)?,
            "-signature",
            path_arg(&proof_path)?,
            path_arg(&pod_uid_path)?,
        ])?;
        openssl_args.extend(["-new", "-force_pubkey"]);
    }
    if alpha_public_key.is_some() {
        // In the alpha API, kube-apiserver has already checked the proof of
        // possession and the signer receives the public key directly.
        openssl_args.push(path_arg(&public_key_pem)?);
        openssl_args.extend(["-subj", "/CN=nodeactivator"]);
    } else {
        openssl_args.push(path_arg(&request_path)?);
    }
    openssl_args.extend([
        "-CA",
        ca_cert,
        "-CAkey",
        ca_key,
        "-CAserial",
        path_arg(&serial_path)?,
        "-CAcreateserial",
        "-out",
        path_arg(&cert_path)?,
        "-days",
        "1",
        "-sha256",
        "-extfile",
        path_arg(&extensions_path)?,
    ]);
    run_openssl(&openssl_args)?;
    let certificate_chain = fs::read_to_string(&cert_path)?;
    let dates = openssl_output(&[
        "x509",
        "-in",
        path_arg(&cert_path)?,
        "-noout",
        "-startdate",
        "-enddate",
    ])?;
    let (not_before, not_after) = parse_certificate_dates(&dates)?;
    let now = Utc::now();
    let refresh_at = now + (not_after - now) * 2 / 3;
    let time_format = |time: DateTime<Utc>| time.to_rfc3339_opts(SecondsFormat::Secs, true);
    let name = request.name_any();
    let requests: Api<DynamicObject> = Api::namespaced_with(client.clone(), &namespace, resource);
    requests
        .patch_status(
            &name,
            &PatchParams::default(),
            &Patch::Merge(json!({
                "status": {
                    "beginRefreshAt": time_format(refresh_at),
                    "certificateChain": certificate_chain,
                    "conditions": [{
                        "type": "Issued",
                        "status": "True",
                        "lastTransitionTime": time_format(now),
                        "reason": "Issued",
                        "message": "nodeactivator serving certificate issued"
                    }],
                    "notBefore": time_format(not_before),
                    "notAfter": time_format(not_after)
                }
            })),
        )
        .await
        .context("writing PodCertificateRequest status")?;
    info!(
        namespace,
        pod = pod_name,
        pod_uid,
        request = name,
        "issued unique nodeactivator serving certificate"
    );
    Ok(())
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("PodCertificateRequest spec has no {key}"))
}

fn parse_certificate_dates(output: &str) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let mut not_before = None;
    let mut not_after = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("notBefore=") {
            not_before = Some(parse_openssl_time(value)?);
        } else if let Some(value) = line.strip_prefix("notAfter=") {
            not_after = Some(parse_openssl_time(value)?);
        }
    }
    Ok((
        not_before.context("certificate has no notBefore date")?,
        not_after.context("certificate has no notAfter date")?,
    ))
}

fn parse_openssl_time(value: &str) -> Result<DateTime<Utc>> {
    let time = NaiveDateTime::parse_from_str(value, "%b %e %H:%M:%S %Y GMT")
        .with_context(|| format!("parsing OpenSSL certificate time {value:?}"))?;
    Ok(time.and_utc())
}

fn run_openssl(args: &[&str]) -> Result<()> {
    let output = Command::new("openssl")
        .args(args)
        .output()
        .context("running openssl")?;
    ensure!(
        output.status.success(),
        "openssl failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn openssl_output(args: &[&str]) -> Result<String> {
    let output = Command::new("openssl")
        .args(args)
        .output()
        .context("running openssl")?;
    ensure!(
        output.status.success(),
        "openssl failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).context("openssl output was not UTF-8")
}

fn path_arg(path: &Path) -> Result<&str> {
    path.to_str()
        .context("temporary certificate path is not UTF-8")
}

struct RequestWorkDir {
    path: PathBuf,
}

impl RequestWorkDir {
    fn create(pod_uid: &str, request_name: &str) -> Result<Self> {
        let path = env::temp_dir().join(format!(
            "nodeactivator-cert-{}-{}-{}",
            std::process::id(),
            pod_uid,
            request_name
        ));
        fs::create_dir_all(&path)
            .with_context(|| format!("creating certificate work directory {}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for RequestWorkDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openssl_certificate_dates() {
        let (not_before, not_after) = parse_certificate_dates(
            "notBefore=Oct 23 03:00:00 2026 GMT\nnotAfter=Oct 24 03:00:00 2026 GMT\n",
        )
        .unwrap();
        assert_eq!(not_before.to_rfc3339(), "2026-10-23T03:00:00+00:00");
        assert_eq!(not_after.to_rfc3339(), "2026-10-24T03:00:00+00:00");
    }
}
