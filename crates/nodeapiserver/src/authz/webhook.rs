//! Optional HTTP `SubjectAccessReview` authorization.
//!
//! The request and response shapes are the standard
//! `authorization.k8s.io/v1` API. The client is intentionally independent of
//! the local RBAC resolver: a configured webhook is an additional authorizer,
//! so a denial or an unavailable webhook cannot silently become an allow. The
//! response preserves the upstream three-way result: `Allow` short-circuits
//! the local chain, `Deny` rejects the request, and `NoOpinion` lets the next
//! authorizer decide. Transient transport and server failures are retried with
//! a small bounded backoff; a valid response or a non-retryable HTTP status
//! is returned immediately.

use crate::authn::x509::Identity;
use crate::server::path::RequestInfo;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;

const MAX_ATTEMPTS: usize = 3;
const RETRY_BACKOFF: Duration = Duration::from_millis(25);
const MAX_CONTROLLED_ATTR_CACHE_SIZE: usize = 10_000;
const MAX_CACHE_ENTRIES: usize = 8_192;
const DEFAULT_AUTHORIZED_TTL: Duration = Duration::from_secs(5 * 60);
const DEFAULT_UNAUTHORIZED_TTL: Duration = Duration::from_secs(30);

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
    cache: Arc<Mutex<HashMap<String, CachedDecision>>>,
    authorized_ttl: Duration,
    unauthorized_ttl: Duration,
}

#[derive(Clone, Copy)]
struct CachedDecision {
    decision: Decision,
    expires_at: std::time::Instant,
}

impl WebhookAuthorizer {
    pub fn new(url: String) -> Result<Self, Error> {
        Self::new_with_cache_ttls(url, DEFAULT_AUTHORIZED_TTL, DEFAULT_UNAUTHORIZED_TTL)
    }

    pub fn new_with_cache_ttls(url: String, authorized_ttl: Duration, unauthorized_ttl: Duration) -> Result<Self, Error> {
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
        Ok(Self { url, client, cache: Arc::new(Mutex::new(HashMap::new())), authorized_ttl, unauthorized_ttl })
    }

    /// Ask the configured authorizer about one parsed Kubernetes request.
    pub async fn authorize(
        &self,
        info: &RequestInfo,
        identity: Option<&Identity>,
    ) -> Result<Decision, Error> {
        let review = build_review(info, identity);
        let cache_key = cache_key(&review);
        if let Some(key) = cache_key.as_deref() {
            if let Some(decision) = self.cached_decision(key) {
                return Ok(decision);
            }
        }
        for attempt in 0..MAX_ATTEMPTS {
            let response = match self.client.post(&self.url).json(&review).send().await {
                Ok(response) if retryable_status(response.status()) && attempt + 1 < MAX_ATTEMPTS => {
                    tokio::time::sleep(RETRY_BACKOFF * (attempt as u32 + 1)).await;
                    continue;
                }
                Ok(response) => response,
                Err(_error) if attempt + 1 < MAX_ATTEMPTS => {
                    tokio::time::sleep(RETRY_BACKOFF * (attempt as u32 + 1)).await;
                    continue;
                }
                Err(error) => return Err(Error::Request(error)),
            };
            if !response.status().is_success() {
                return Err(Error::InvalidResponse(format!("webhook returned HTTP {}", response.status())));
            }
            let decision = parse_decision(&response.json::<Value>().await?)?;
            if let Some(key) = cache_key.as_deref() {
                self.cache_decision(key, decision);
            }
            return Ok(decision);
        }
        unreachable!("authorization webhook attempts are bounded above")
    }

    fn cached_decision(&self, key: &str) -> Option<Decision> {
        let mut cache = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = cache.get(key).copied()?;
        if entry.expires_at > std::time::Instant::now() {
            Some(entry.decision)
        } else {
            cache.remove(key);
            None
        }
    }

    fn cache_decision(&self, key: &str, decision: Decision) {
        let ttl = match decision {
            Decision::Allow => self.authorized_ttl,
            Decision::Deny | Decision::NoOpinion => self.unauthorized_ttl,
        };
        if ttl.is_zero() {
            return;
        }
        let mut cache = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache.len() >= MAX_CACHE_ENTRIES && !cache.contains_key(key) {
            if let Some(oldest_key) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| key.clone())
            {
                cache.remove(&oldest_key);
            }
        }
        cache.insert(key.to_string(), CachedDecision { decision, expires_at: std::time::Instant::now() + ttl });
    }
}

fn cache_key(review: &Value) -> Option<String> {
    let key = serde_json::to_string(review).ok()?;
    (key.len() <= MAX_CONTROLLED_ATTR_CACHE_SIZE).then_some(key)
}

fn retryable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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

    #[tokio::test]
    async fn retries_transient_webhook_failures_before_returning_the_decision() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = first.read(&mut request).await.unwrap();
            first.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            let (mut second, _) = listener.accept().await.unwrap();
            let _ = second.read(&mut request).await.unwrap();
            second.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 27\r\nConnection: close\r\n\r\n{\"status\":{\"allowed\":true}}").await.unwrap();
        });
        let authorizer = WebhookAuthorizer::new(format!("http://{address}/authorize")).unwrap();
        let decision = authorizer.authorize(&RequestInfo::default(), None).await.unwrap();
        assert_eq!(decision, Decision::Allow);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn caches_a_valid_decision_and_refetches_after_its_ttl_expires() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut connection, _) = listener.accept().await.unwrap();
                let mut request = [0; 1024];
                let _ = connection.read(&mut request).await.unwrap();
                connection
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 27\r\nConnection: close\r\n\r\n{\"status\":{\"allowed\":true}}")
                    .await
                    .unwrap();
            }
        });
        let authorizer = WebhookAuthorizer::new_with_cache_ttls(
            format!("http://{address}/authorize"),
            Duration::from_millis(20),
            Duration::from_millis(20),
        )
        .unwrap();
        let info = RequestInfo { verb: "get".to_string(), path: "/api/v1/nodes".to_string(), ..Default::default() };
        assert_eq!(authorizer.authorize(&info, None).await.unwrap(), Decision::Allow);
        assert_eq!(authorizer.authorize(&info, None).await.unwrap(), Decision::Allow);
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(authorizer.authorize(&info, None).await.unwrap(), Decision::Allow);
        server.await.unwrap();
    }

    #[test]
    fn skips_cache_keys_with_unbounded_requester_attributes() {
        let review = json!({"spec": {"user": "x".repeat(MAX_CONTROLLED_ATTR_CACHE_SIZE + 1)}});
        assert!(cache_key(&review).is_none());
    }

    #[test]
    fn retries_server_errors_and_rate_limits_but_not_client_errors() {
        assert!(retryable_status(reqwest::StatusCode::SERVICE_UNAVAILABLE));
        assert!(retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(!retryable_status(reqwest::StatusCode::BAD_REQUEST));
    }
}
