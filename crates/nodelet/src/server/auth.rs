//! Request authentication for the kubelet-style server: bearer token via
//! the `TokenReview` API (the same mechanism a real kubelet's
//! `--authentication-token-webhook` uses against the apiserver). No
//! anonymous access — a request with no/invalid token is rejected, unlike
//! real kubelet's historical `--anonymous-auth=true` default.
//!
//! Authorization is deliberately `AlwaysAllow` once a token authenticates —
//! matching real kubelet's own historical default (`--authorization-mode=
//! AlwaysAllow`) rather than a from-scratch `SubjectAccessReview`
//! implementation. See docs/GAP_CLOSURE.md.

use anyhow::{Context, Result};
use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec};
use kube::Client;

/// Extract the token from an `Authorization: Bearer <token>` header value.
/// `None` for anything else (missing header, wrong scheme, empty token).
pub fn extract_bearer_token(header_value: Option<&str>) -> Option<&str> {
    let value = header_value?;
    let token = value.strip_prefix("Bearer ")?;
    (!token.is_empty()).then_some(token)
}

/// Ask the apiserver whether `token` is a valid credential, via the same
/// `TokenReview` subresource real kubelet's webhook authenticator uses.
/// Returns the authenticated username on success.
pub async fn authenticate(client: &Client, token: &str) -> Result<Option<String>> {
    let body = TokenReview {
        metadata: Default::default(),
        spec: TokenReviewSpec { token: Some(token.to_string()), ..Default::default() },
        status: None,
    };
    let bytes = serde_json::to_vec(&body).context("serializing TokenReview")?;
    let req = http::Request::builder()
        .method("POST")
        .uri("/apis/authentication.k8s.io/v1/tokenreviews")
        .header("Content-Type", "application/json")
        .body(bytes)
        .context("building TokenReview HTTP request")?;
    let resp: TokenReview = client.request(req).await.context("TokenReview API call")?;
    let status = resp.status.context("TokenReview response had no status")?;
    if status.authenticated != Some(true) {
        return Ok(None);
    }
    Ok(status.user.and_then(|u| u.username))
}

#[cfg(test)]
#[path = "auth_tests/extract_bearer_token.rs"]
mod tests_extract_bearer_token;
