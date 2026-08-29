//! Optional HTTP `SubjectAccessReview` authorization.
//!
//! The request and response shapes are the standard
//! `authorization.k8s.io/v1` API. The client is intentionally independent of
//! the local RBAC resolver: a configured webhook is an additional authorizer,
//! so a denial or an unavailable webhook cannot silently become an allow. The
//! response preserves the upstream three-way result: `Allow` short-circuits
//! the local chain, `Deny` rejects the request, and `NoOpinion` lets the next
//! authorizer decide.

use crate::authn::x509::Identity;
use crate::server::path::RequestInfo;
use serde_json::{json, Value};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("authorization webhook request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("authorization webhook returned an invalid SubjectAccessReview: {0}")]
    InvalidResponse(String),
}

/// The three outcomes an authorization webhook can return.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    Allow,
    NoOpinion,
    Deny,
}

#[derive(Clone)]
pub struct WebhookAuthorizer {
    url: String,
    client: reqwest::Client,
}

impl WebhookAuthorizer {
    pub fn new(url: String) -> Result<Self, Error> {
        let parsed = reqwest::Url::parse(&url)
            .map_err(|error| Error::InvalidResponse(format!("invalid URL: {error}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(Error::InvalidResponse(
                "URL scheme must be http or https".to_string(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self { url, client })
    }

    /// Ask the configured authorizer about one parsed Kubernetes request.
    pub async fn authorize(
        &self,
        info: &RequestInfo,
        identity: Option<&Identity>,
    ) -> Result<Decision, Error> {
        let response = self
            .client
            .post(&self.url)
            .json(&build_review(info, identity))
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        parse_decision(&response)
    }
}

fn parse_decision(response: &Value) -> Result<Decision, Error> {
    let status = response
        .get("status")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::InvalidResponse("status was not an object".to_string()))?;
    let allowed = status
        .get("allowed")
        .map(|value| {
            value.as_bool().ok_or_else(|| {
                Error::InvalidResponse("status.allowed was not boolean".to_string())
            })
        })
        .transpose()?
        .unwrap_or(false);
    let denied = status
        .get("denied")
        .map(|value| {
            value.as_bool().ok_or_else(|| {
                Error::InvalidResponse("status.denied was not boolean".to_string())
            })
        })
        .transpose()?
        .unwrap_or(false);
    match (allowed, denied) {
        (true, true) => Err(Error::InvalidResponse(
            "status.allowed and status.denied were both true".to_string(),
        )),
        (true, false) => Ok(Decision::Allow),
        (false, true) => Ok(Decision::Deny),
        (false, false) => Ok(Decision::NoOpinion),
    }
}

fn build_review(info: &RequestInfo, identity: Option<&Identity>) -> Value {
    let anonymous_groups = ["system:unauthenticated".to_string()];
    let (user, groups) = match identity {
        Some(identity) => (identity.name.as_str(), identity.groups.as_slice()),
        None => ("system:anonymous", anonymous_groups.as_slice()),
    };
    let attributes = if info.is_resource_request {
        json!({
            "namespace": info.namespace,
            "verb": info.verb,
            "group": info.api_group,
            "version": info.api_version,
            "resource": info.resource,
            "subresource": info.subresource,
            "name": info.name,
        })
    } else {
        json!({
            "path": info.path,
            "verb": info.verb,
        })
    };
    let spec = if info.is_resource_request {
        json!({
            "user": user,
            "uid": identity.and_then(|identity| identity.uid.as_deref()),
            "groups": groups,
            "resourceAttributes": attributes,
        })
    } else {
        json!({
            "user": user,
            "uid": identity.and_then(|identity| identity.uid.as_deref()),
            "groups": groups,
            "nonResourceAttributes": attributes,
        })
    };
    json!({
        "apiVersion": "authorization.k8s.io/v1",
        "kind": "SubjectAccessReview",
        "spec": spec,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_http_and_https_endpoints() {
        assert!(WebhookAuthorizer::new("https://authz.example/review".to_string()).is_ok());
        assert!(WebhookAuthorizer::new("file:///tmp/review".to_string()).is_err());
    }

    #[test]
    fn request_info_keeps_resource_and_non_resource_attributes_distinct() {
        let resource = RequestInfo {
            is_resource_request: true,
            path: "/api/v1/namespaces/default/pods/demo".to_string(),
            verb: "get".to_string(),
            api_group: String::new(),
            api_version: "v1".to_string(),
            namespace: "default".to_string(),
            resource: "pods".to_string(),
            name: "demo".to_string(),
            ..Default::default()
        };
        let non_resource = RequestInfo {
            path: "/healthz".to_string(),
            verb: "get".to_string(),
            ..Default::default()
        };
        assert!(resource.is_resource_request);
        assert!(!non_resource.is_resource_request);
        assert_eq!(resource.resource, "pods");
        assert_eq!(non_resource.path, "/healthz");
    }

    #[test]
    fn review_body_uses_the_standard_resource_attributes_shape() {
        let info = RequestInfo {
            is_resource_request: true,
            api_group: "apps".to_string(),
            api_version: "v1".to_string(),
            namespace: "default".to_string(),
            resource: "deployments".to_string(),
            name: "demo".to_string(),
            verb: "get".to_string(),
            ..Default::default()
        };
        let review = build_review(&info, None);
        assert_eq!(review["apiVersion"], "authorization.k8s.io/v1");
        assert_eq!(review["spec"]["user"], "system:anonymous");
        assert_eq!(review["spec"]["groups"][0], "system:unauthenticated");
        assert_eq!(review["spec"]["resourceAttributes"]["group"], "apps");
        assert!(review["spec"].get("nonResourceAttributes").is_none());
    }

    #[test]
    fn parses_the_three_subject_access_review_decisions() {
        assert_eq!(
            parse_decision(&json!({"status": {"allowed": true}})).unwrap(),
            Decision::Allow
        );
        assert_eq!(
            parse_decision(&json!({"status": {"denied": true}})).unwrap(),
            Decision::Deny
        );
        assert_eq!(
            parse_decision(&json!({"status": {"allowed": false, "denied": false}})).unwrap(),
            Decision::NoOpinion
        );
        assert_eq!(
            parse_decision(&json!({"status": {}})).unwrap(),
            Decision::NoOpinion
        );
    }

    #[test]
    fn rejects_an_invalid_subject_access_review_decision() {
        assert!(parse_decision(&json!({"status": {"allowed": true, "denied": true}})).is_err());
        assert!(parse_decision(&json!({"status": {"allowed": "yes"}})).is_err());
        assert!(parse_decision(&json!({"status": {"denied": "yes"}})).is_err());
        assert!(parse_decision(&json!({})).is_err());
    }
}
