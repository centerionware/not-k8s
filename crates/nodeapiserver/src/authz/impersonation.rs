//! HTTP impersonation headers, authorized as the original caller before
//! replacing the request identity. Kubernetes clients use this for
//! controller-specific ServiceAccount credentials.

use crate::authn::x509::Identity;
use crate::cacher::CacheRegistry;
use crate::server::path::RequestInfo;
use crate::storage::client::StorageClient;
use http::HeaderMap;

/// Resolve Kubernetes' user/group impersonation headers. Every requested
/// identity attribute is checked against the original caller's RBAC grants.
pub async fn apply(
    headers: &HeaderMap,
    original: Option<&Identity>,
    storage: &mut StorageClient,
    cache: &CacheRegistry,
) -> Result<Option<Identity>, String> {
    if headers.contains_key("impersonate-uid")
        || headers
            .keys()
            .any(|name| name.as_str().starts_with("impersonate-extra-"))
    {
        return Err("Impersonate-Uid and Impersonate-Extra headers are not supported".into());
    }
    let Some(user) = one_header(headers, "impersonate-user")? else {
        if headers.contains_key("impersonate-group") {
            return Err("Impersonate-Group requires Impersonate-User".into());
        }
        return Ok(original.cloned());
    };
    let mut effective = original
        .cloned()
        .ok_or_else(|| "an authenticated identity is required for impersonation".to_string())?;

    let (resource, namespace, name) = impersonation_subject(user)?;
    authorize_impersonation(original, storage, cache, resource, namespace, name).await?;

    let mut groups = Vec::new();
    for value in headers.get_all("impersonate-group") {
        let group = value
            .to_str()
            .map_err(|_| "Impersonate-Group is not valid UTF-8".to_string())?;
        if group.is_empty() {
            return Err("Impersonate-Group cannot be empty".into());
        }
        authorize_impersonation(original, storage, cache, "groups", "", group).await?;
        if !groups.iter().any(|existing| existing == group) {
            groups.push(group.to_string());
        }
    }
    if !groups.iter().any(|group| group == "system:authenticated") {
        groups.push("system:authenticated".to_string());
    }
    effective.name = user.to_string();
    effective.groups = groups;
    effective.uid = None;
    effective.extra.clear();
    effective.credential_id = (String::new(), Vec::new());
    Ok(Some(effective))
}

fn impersonation_subject(user: &str) -> Result<(&'static str, &str, &str), String> {
    if let Some(sa) = user.strip_prefix("system:serviceaccount:") {
        let (namespace, name) = sa.split_once(':').ok_or_else(|| {
            "Impersonate-User contains a malformed ServiceAccount identity".to_string()
        })?;
        if namespace.is_empty() || name.is_empty() || name.contains(':') {
            return Err("Impersonate-User contains a malformed ServiceAccount identity".into());
        }
        Ok(("serviceaccounts", namespace, name))
    } else {
        Ok(("users", "", user))
    }
}

fn one_header<'a>(headers: &'a HeaderMap, name: &'static str) -> Result<Option<&'a str>, String> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(format!("{name} must appear at most once"));
    }
    let value = value
        .to_str()
        .map_err(|_| format!("{name} is not valid UTF-8"))?;
    if value.is_empty() {
        return Err(format!("{name} cannot be empty"));
    }
    Ok(Some(value))
}

async fn authorize_impersonation(
    original: Option<&Identity>,
    storage: &mut StorageClient,
    cache: &CacheRegistry,
    resource: &str,
    namespace: &str,
    name: &str,
) -> Result<(), String> {
    let mut request = RequestInfo::default();
    request.is_resource_request = true;
    request.verb = "impersonate".into();
    request.resource = resource.into();
    request.namespace = namespace.into();
    request.name = name.into();
    if !super::request_allowed(storage, original, &request, Some(cache)).await? {
        return Err(format!(
            "caller is not authorized to impersonate {resource}/{name}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn one_header_rejects_duplicates_and_empty_values() {
        let mut headers = HeaderMap::new();
        headers.append("impersonate-user", HeaderValue::from_static("alice"));
        headers.append("impersonate-user", HeaderValue::from_static("bob"));
        assert!(one_header(&headers, "impersonate-user").is_err());
        headers.remove("impersonate-user");
        headers.insert("impersonate-user", HeaderValue::from_static(""));
        assert!(one_header(&headers, "impersonate-user").is_err());
    }

    #[test]
    fn parses_service_account_identity_without_accepting_extra_segments() {
        let valid = "system:serviceaccount:kube-system:replicaset-controller";
        assert_eq!(
            impersonation_subject(valid).unwrap(),
            ("serviceaccounts", "kube-system", "replicaset-controller")
        );
        let malformed = "system:serviceaccount:kube-system:replicaset-controller:extra";
        assert!(impersonation_subject(malformed).is_err());
    }
}
