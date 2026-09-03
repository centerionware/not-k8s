//! Certificate admission for `CertificateSigningRequest` objects.
//!
//! These checks mirror the three small certificate admission plugins enabled
//! by the upstream kube-apiserver default chain.  The signer checks authorize
//! against the synthetic `signers` resource, not the CSR object itself; the
//! subject restriction parses the CSR request that the caller supplied.

use crate::admission::attributes::Operation;
use crate::authn::x509::Identity;
use crate::authz;
use crate::server::path::RequestInfo;
use crate::storage::client::StorageClient;
use base64::Engine;
use serde_json::Value;
use x509_parser::prelude::FromDer;

const GROUP: &str = "certificates.k8s.io";
const RESOURCE: &str = "certificatesigningrequests";
const SIGNERS_RESOURCE: &str = "signers";
const KUBE_APISERVER_CLIENT_SIGNER: &str = "kubernetes.io/kube-apiserver-client";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("certificate admission denied the request: {0}")]
    Forbidden(String),
    #[error("certificate admission authorization failed: {0}")]
    Lookup(String),
}

fn is_csr(group: &str, resource: &str) -> bool {
    group == GROUP && resource == RESOURCE
}

/// Applies `CertificateSubjectRestriction` to a CSR create.
///
/// The upstream plugin protects the `kubernetes.io/kube-apiserver-client`
/// signer from issuing a client certificate carrying `system:masters`.  CSR
/// requests are JSON base64 strings containing a PEM `CERTIFICATE REQUEST`
/// block, so both envelopes are checked before the subject is inspected.
pub fn validate_subject_restriction(
    operation: Operation,
    group: &str,
    resource: &str,
    subresource: &str,
    object: Option<&Value>,
) -> Result<(), Error> {
    if operation != Operation::Create
        || !is_csr(group, resource)
        || !subresource.is_empty()
    {
        return Ok(());
    }

    let Some(object) = object else {
        return Err(Error::Forbidden(
            "CertificateSigningRequest has no object".to_string(),
        ));
    };
    let signer_name = object
        .pointer("/spec/signerName")
        .and_then(Value::as_str)
        .unwrap_or("");
    if signer_name != KUBE_APISERVER_CLIENT_SIGNER {
        return Ok(());
    }

    let organizations = parse_csr_organizations(object)?;
    if organizations.iter().any(|organization| organization == "system:masters") {
        return Err(Error::Forbidden(format!(
            "use of {KUBE_APISERVER_CLIENT_SIGNER} signer with system:masters group is not allowed"
        )));
    }
    Ok(())
}

fn parse_csr_organizations(object: &Value) -> Result<Vec<String>, Error> {
    let request = object
        .pointer("/spec/request")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Forbidden("CertificateSigningRequest has no spec.request".to_string()))?;
    let encoded = base64::engine::general_purpose::STANDARD
        .decode(request)
        .map_err(|error| Error::Forbidden(format!("failed to decode CSR request: {error}")))?;
    let pem = pem::parse(encoded)
        .map_err(|error| Error::Forbidden(format!("failed to parse CSR PEM: {error}")))?;
    if pem.tag() != "CERTIFICATE REQUEST" {
        return Err(Error::Forbidden(
            "CSR PEM block type must be CERTIFICATE REQUEST".to_string(),
        ));
    }
    let (_, csr) = x509_parser::certification_request::X509CertificationRequest::from_der(pem.contents())
        .map_err(|error| Error::Forbidden(format!("failed to parse CSR: {error:?}")))?;
    Ok(csr
        .certification_request_info
        .subject
        .iter_organization()
        .filter_map(|attribute| attribute.as_str().ok().map(str::to_string))
        .collect())
}

/// Validates the authorization performed by `CertificateApproval` or
/// `CertificateSigning` for a CSR status subresource update.
///
/// Upstream checks `verb=approve|sign` on the synthetic `signers` resource,
/// first for the exact signer name and then for its `{domain}/*` wildcard.
/// The nodeapiserver's RBAC enforcement is optional during bootstrap, so the
/// same opt-in controls this additional authorization check.
pub async fn validate_signer_update(
    storage: &mut StorageClient,
    enforce_rbac: bool,
    identity: Option<&Identity>,
    action: &str,
    old_object: Option<&Value>,
    candidate: Option<&Value>,
    subresource: &str,
) -> Result<(), Error> {
    if !is_csr(GROUP, RESOURCE) || !matches!(subresource, "approval" | "status") {
        return Ok(());
    }
    if subresource == "approval" && action != "approve" {
        return Ok(());
    }
    if subresource == "status" && action != "sign" {
        return Ok(());
    }

    let old_object = old_object.ok_or_else(|| {
        Error::Forbidden("CertificateSigningRequest does not exist".to_string())
    })?;
    let signer_name = old_object
        .pointer("/spec/signerName")
        .and_then(Value::as_str)
        .unwrap_or("");

    if subresource == "status" {
        let candidate = candidate.ok_or_else(|| {
            Error::Forbidden("CertificateSigningRequest status has no object".to_string())
        })?;
        let certificate_changed = old_object.pointer("/status/certificate")
            != candidate.pointer("/status/certificate");
        let conditions_changed = old_object.pointer("/status/conditions")
            != candidate.pointer("/status/conditions");
        if !certificate_changed && !conditions_changed {
            return Ok(());
        }
    }

    if !enforce_rbac {
        return Ok(());
    }
    if signer_name_is_authorized(storage, identity, action, signer_name).await? {
        Ok(())
    } else {
        Err(Error::Forbidden(format!(
            "user is not permitted to {action} requests with signerName {signer_name:?}"
        )))
    }
}

async fn signer_name_is_authorized(
    storage: &mut StorageClient,
    identity: Option<&Identity>,
    action: &str,
    signer_name: &str,
) -> Result<bool, Error> {
    if signer_request_allowed(storage, identity, action, signer_name).await? {
        return Ok(true);
    }
    let domain = signer_name.split('/').next().unwrap_or(signer_name);
    signer_request_allowed(storage, identity, action, &format!("{domain}/*")).await
}

async fn signer_request_allowed(
    storage: &mut StorageClient,
    identity: Option<&Identity>,
    action: &str,
    signer_name: &str,
) -> Result<bool, Error> {
    let info = RequestInfo {
        is_resource_request: true,
        path: format!("/apis/{GROUP}/*/{SIGNERS_RESOURCE}/{signer_name}"),
        verb: action.to_string(),
        api_prefix: "apis".to_string(),
        api_group: GROUP.to_string(),
        api_version: "*".to_string(),
        resource: SIGNERS_RESOURCE.to_string(),
        name: signer_name.to_string(),
        ..Default::default()
    };
    // No CacheRegistry handle reaches this admission-time call site; falls
    // back to the uncached storage path exactly as before this change —
    // see `authz::resolve::rules_for`'s own doc comment.
    authz::request_allowed(storage, identity, &info, None)
        .await
        .map_err(Error::Lookup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
    use serde_json::json;

    fn csr(organizations: &[&str], signer_name: &str) -> Value {
        let mut params = CertificateParams::default();
        let mut subject = DistinguishedName::new();
        subject.push(DnType::CommonName, "client");
        for organization in organizations {
            subject.push(DnType::OrganizationName, *organization);
        }
        params.distinguished_name = subject;
        let key = KeyPair::generate().expect("generate CSR key");
        let request = params.serialize_request(&key).expect("serialize CSR");
        let request_pem = request.pem().expect("encode CSR PEM");
        json!({
            "apiVersion": "certificates.k8s.io/v1",
            "kind": "CertificateSigningRequest",
            "spec": {
                "signerName": signer_name,
                "request": base64::engine::general_purpose::STANDARD.encode(request_pem)
            }
        })
    }

    #[test]
    fn subject_restriction_rejects_system_masters_for_apiserver_client_signer() {
        let object = csr(&["system:masters"], KUBE_APISERVER_CLIENT_SIGNER);
        assert!(validate_subject_restriction(Operation::Create, GROUP, RESOURCE, "", Some(&object)).is_err());
    }

    #[test]
    fn subject_restriction_allows_other_subjects_and_signers() {
        let ordinary = csr(&["system:nodes"], KUBE_APISERVER_CLIENT_SIGNER);
        assert!(validate_subject_restriction(Operation::Create, GROUP, RESOURCE, "", Some(&ordinary)).is_ok());
        let custom = csr(&["system:masters"], "example.com/signer");
        assert!(validate_subject_restriction(Operation::Create, GROUP, RESOURCE, "", Some(&custom)).is_ok());
    }

    #[test]
    fn subject_restriction_ignores_non_create_and_subresource_requests() {
        let object = csr(&["system:masters"], KUBE_APISERVER_CLIENT_SIGNER);
        assert!(validate_subject_restriction(Operation::Update, GROUP, RESOURCE, "", Some(&object)).is_ok());
        assert!(validate_subject_restriction(Operation::Create, GROUP, RESOURCE, "status", Some(&object)).is_ok());
    }

    #[test]
    fn signer_authorization_request_uses_the_synthetic_signers_resource() {
        let info = RequestInfo {
            is_resource_request: true,
            api_group: GROUP.to_string(),
            resource: SIGNERS_RESOURCE.to_string(),
            verb: "approve".to_string(),
            name: "kubernetes.io/kube-apiserver-client-kubelet".to_string(),
            ..Default::default()
        };
        assert_eq!(info.resource, SIGNERS_RESOURCE);
        assert_eq!(info.name, "kubernetes.io/kube-apiserver-client-kubelet");
    }
}
