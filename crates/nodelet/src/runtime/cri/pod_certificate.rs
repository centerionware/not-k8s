use super::*;
use base64::Engine;
use chrono::{DateTime, Utc};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::PostParams;
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use rustls::SignatureScheme;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::SigningKey;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Component, Path};
use tokio::sync::OnceCell;

const POD_CERTIFICATE_SIGNER: &str = "nodeactivator.notk8s.io/serving";
const CERTIFICATE_REQUEST_PLURAL: &str = "podcertificaterequests";
static POD_CERTIFICATE_RESOURCE: OnceCell<ApiResource> = OnceCell::const_new();

impl CriRuntime {
    pub(crate) async fn write_pod_certificate_projection(
        &self,
        volume_dir: &Path,
        pod: &Pod,
        id: &PodId,
        source_index: usize,
        projection: &k8s_openapi::api::core::v1::PodCertificateProjection,
    ) -> Result<()> {
        anyhow::ensure!(
            projection.signer_name == POD_CERTIFICATE_SIGNER,
            "unsupported podCertificate signer {:?}",
            projection.signer_name
        );
        anyhow::ensure!(
            projection.key_type == "ECDSAP256",
            "nodelet currently supports ECDSAP256 podCertificate keys"
        );
        let expires = projection.max_expiration_seconds.unwrap_or(86_400);
        anyhow::ensure!(
            (3_600..=7_862_400).contains(&expires),
            "podCertificate maxExpirationSeconds is outside the Kubernetes range"
        );
        let bundle_path = projection.credential_bundle_path.as_deref();
        let key_path_in_volume = projection.key_path.as_deref();
        let chain_path_in_volume = projection.certificate_chain_path.as_deref();
        anyhow::ensure!(
            match bundle_path {
                Some(_) => key_path_in_volume.is_none() && chain_path_in_volume.is_none(),
                None => key_path_in_volume.is_some() && chain_path_in_volume.is_some(),
            },
            "podCertificate requires credentialBundlePath or both keyPath and certificateChainPath"
        );
        anyhow::ensure!(
            key_path_in_volume.is_none() || key_path_in_volume != chain_path_in_volume,
            "podCertificate keyPath and certificateChainPath must differ"
        );

        let state_dir = volume_dir
            .parent()
            .context("projected volume has no parent directory")?
            .join(".pod-certificates")
            .join(format!("{}-{source_index}", id.uid));
        std::fs::create_dir_all(&state_dir)
            .context("creating private PodCertificate state directory")?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))
            .context("restricting private PodCertificate state directory")?;
        let key_path = state_dir.join("key.pem");
        let key_pair = match std::fs::read_to_string(&key_path) {
            Ok(pem) => rcgen::KeyPair::from_pem(&pem)
                .context("loading persistent PodCertificate private key")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let key = rcgen::KeyPair::generate()
                    .context("generating per-Pod ECDSA P-256 private key")?;
                write_private_key(&key_path, key.serialize_pem().as_bytes())?;
                key
            }
            Err(error) => {
                return Err(error).context("reading persistent PodCertificate private key");
            }
        };

        let resource = pod_certificate_resource(&self.client).await?.clone();
        let requests: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), &id.namespace, &resource);
        let key_digest = Sha256::digest(key_pair.public_key_der());
        let key_tag = key_digest[..6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let uid_tag = id
            .uid
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(12)
            .collect::<String>()
            .to_ascii_lowercase();
        let request_name = format!("pc-{uid_tag}-{source_index}-{key_tag}");

        let node_name = pod
            .spec
            .as_ref()
            .and_then(|spec| spec.node_name.as_deref())
            .filter(|name| !name.is_empty())
            .context("podCertificate Pod is not assigned to a node")?;
        let nodes: Api<k8s_openapi::api::core::v1::Node> = Api::all(self.client.clone());
        let node = nodes
            .get(node_name)
            .await
            .context("reading PodCertificate node identity")?;
        let node_uid = node
            .metadata
            .uid
            .context("PodCertificate Node has no UID")?;
        let service_accounts: Api<ServiceAccount> =
            Api::namespaced(self.client.clone(), &id.namespace);
        let service_account = service_accounts
            .get(&id.service_account_name)
            .await
            .context("reading PodCertificate ServiceAccount identity")?;
        let service_account_uid = service_account
            .metadata
            .uid
            .context("PodCertificate ServiceAccount has no UID")?;

        let request = match requests
            .get_opt(&request_name)
            .await
            .context("reading PodCertificateRequest")?
        {
            Some(request) => request,
            None => {
                let api_version = resource.api_version.as_str();
                let spec = if api_version.ends_with("/v1") {
                    let csr = rcgen::CertificateParams::default()
                        .serialize_request(&key_pair)
                        .context("creating PodCertificate PKCS#10 request")?;
                    json!({
                        "signerName": POD_CERTIFICATE_SIGNER,
                        "podName": id.name,
                        "podUID": id.uid,
                        "serviceAccountName": id.service_account_name,
                        "serviceAccountUID": service_account_uid,
                        "nodeName": node_name,
                        "nodeUID": node_uid,
                        "maxExpirationSeconds": expires,
                        "stubPKCS10Request": base64::engine::general_purpose::STANDARD.encode(csr.der().as_ref()),
                    })
                } else {
                    let key_der = key_pair.serialize_der();
                    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));
                    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key_der)
                        .context("loading PodCertificate key for alpha proof-of-possession")?;
                    let signer = signing_key
                        .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
                        .context("selecting PodCertificate alpha proof-of-possession signature")?;
                    let proof = signer
                        .sign(id.uid.as_bytes())
                        .context("signing PodCertificate alpha proof-of-possession")?;
                    json!({
                        "signerName": POD_CERTIFICATE_SIGNER,
                        "podName": id.name,
                        "podUID": id.uid,
                        "serviceAccountName": id.service_account_name,
                        "serviceAccountUID": service_account_uid,
                        "nodeName": node_name,
                        "nodeUID": node_uid,
                        "maxExpirationSeconds": expires,
                        "pkixPublicKey": base64::engine::general_purpose::STANDARD.encode(key_pair.public_key_der()),
                        "proofOfPossession": base64::engine::general_purpose::STANDARD.encode(proof),
                    })
                };
                let mut object = DynamicObject::new(&request_name, &resource)
                    .within(&id.namespace)
                    .data(json!({ "spec": spec }));
                object.metadata.owner_references = Some(vec![OwnerReference {
                    api_version: "v1".to_string(),
                    block_owner_deletion: Some(false),
                    controller: Some(false),
                    kind: "Pod".to_string(),
                    name: id.name.clone(),
                    uid: id.uid.clone(),
                }]);
                match requests.create(&PostParams::default(), &object).await {
                    Ok(created) => created,
                    Err(kube::Error::Api(error)) if error.code == 409 => requests
                        .get(&request_name)
                        .await
                        .context("reading concurrently created PodCertificateRequest")?,
                    Err(error) => return Err(error).context("creating PodCertificateRequest"),
                }
            }
        };

        let status = request.data.get("status").unwrap_or(&Value::Null);
        let certificate_chain = status.get("certificateChain").and_then(Value::as_str);
        let Some(certificate_chain) = certificate_chain else {
            anyhow::bail!("PodCertificateRequest {request_name} is awaiting its signer")
        };
        let refresh = status
            .get("beginRefreshAt")
            .and_then(Value::as_str)
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|time| time.with_timezone(&Utc));
        if refresh.is_some_and(|time| time <= Utc::now()) {
            // Keep the old bundle in place while a fresh key and request are
            // issued. The request name is tied to the new public key, so the
            // old key is never reused for a rotated certificate.
            std::fs::remove_file(&key_path)
                .context("retiring PodCertificate key at refresh time")?;
            anyhow::bail!("PodCertificate refresh is due; requesting a new key and certificate")
        }

        let key_pem = key_pair.serialize_pem();
        if let Some(bundle) = &projection.credential_bundle_path {
            let path = safe_projection_path(volume_dir, bundle)?;
            let mut contents = key_pem.into_bytes();
            if !contents.ends_with(b"\n") {
                contents.push(b'\n');
            }
            contents.extend_from_slice(certificate_chain.as_bytes());
            write_volume_file(&path, &contents)
                .context("writing PodCertificate credential bundle")?;
        } else {
            let key_path =
                safe_projection_path(volume_dir, projection.key_path.as_deref().unwrap())?;
            let cert_path = safe_projection_path(
                volume_dir,
                projection.certificate_chain_path.as_deref().unwrap(),
            )?;
            write_volume_file(&key_path, key_pem.as_bytes())
                .context("writing PodCertificate private key projection")?;
            write_volume_file(&cert_path, certificate_chain.as_bytes())
                .context("writing PodCertificate certificate chain projection")?;
        }
        // Retain the old object until the new request has been issued; a
        // later reconciliation will garbage collect it without interrupting
        // the mounted credential during signer latency.
        Ok(())
    }
}

async fn pod_certificate_resource(client: &kube::Client) -> Result<&'static ApiResource> {
    POD_CERTIFICATE_RESOURCE
        .get_or_try_init(|| async {
            for version in ["v1", "v1alpha1"] {
                let group_version = format!("certificates.k8s.io/{version}");
                if let Ok(resources) = client.list_api_group_resources(&group_version).await {
                    if resources
                        .resources
                        .iter()
                        .any(|resource| resource.name == CERTIFICATE_REQUEST_PLURAL)
                    {
                        return Ok(ApiResource::from_gvk_with_plural(
                            &GroupVersionKind::gvk(
                                "certificates.k8s.io",
                                version,
                                "PodCertificateRequest",
                            ),
                            CERTIFICATE_REQUEST_PLURAL,
                        ));
                    }
                }
            }
            anyhow::bail!("API server does not serve certificates.k8s.io PodCertificateRequests")
        })
        .await
}

fn safe_projection_path(root: &Path, value: &str) -> Result<std::path::PathBuf> {
    let path = Path::new(value);
    anyhow::ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "podCertificate paths must be non-empty relative paths without parent traversal"
    );
    Ok(root.join(path))
}

fn write_private_key(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path.parent().context("private key path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options
        .open(path)
        .context("creating private PodCertificate key")?;
    file.write_all(bytes)
        .context("persisting private PodCertificate key")?;
    file.sync_all()
        .context("syncing private PodCertificate key")?;
    Ok(())
}
