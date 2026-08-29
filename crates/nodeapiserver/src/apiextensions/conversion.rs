//! CRD conversion-webhook transport.
//!
//! A CRD with more than one served version can nominate one storage version
//! and a webhook that converts objects to or from it. The wire contract is
//! `apiextensions.k8s.io/{v1,v1beta1}` `ConversionReview`, not an admission
//! review: the request carries a desired API version and a batch of raw
//! objects, and the response must return the same number of converted
//! objects in the same order.

use super::registry::ConversionWebhook;
use crate::admission::webhook;
use crate::storage::client::StorageClient;
use serde_json::{json, Value};
use std::time::Duration;

const CONVERSION_API_GROUP: &str = "apiextensions.k8s.io";
const CONVERSION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("conversion webhook setup failed: {0}")]
    Webhook(#[from] webhook::Error),
    #[error("conversion webhook returned HTTP {0}")]
    Http(reqwest::StatusCode),
    #[error("conversion webhook returned invalid JSON: {0}")]
    Json(#[from] reqwest::Error),
    #[error("conversion webhook response is invalid: {0}")]
    InvalidResponse(String),
}

/// Convert a batch of objects to `desired_version` using the CRD's webhook.
/// A caller should invoke this only when the desired version differs from the
/// CRD's storage version; doing so makes the no-op path explicit and prevents
/// an unnecessary round trip on ordinary storage-version requests.
pub async fn convert(
    storage: &mut StorageClient,
    group: &str,
    configuration: &ConversionWebhook,
    desired_version: &str,
    objects: Vec<Value>,
) -> Result<Vec<Value>, Error> {
    let review_version = configuration
        .review_versions
        .iter()
        .find(|version| matches!(version.as_str(), "v1" | "v1beta1"))
        .ok_or_else(|| {
            Error::InvalidResponse(
                "conversionReviewVersions contains no supported version".to_string(),
            )
        })?;
    let review_api_version = format!("{CONVERSION_API_GROUP}/{}", review_version);
    let uid = uuid::Uuid::new_v4().to_string();
    let expected_len = objects.len();
    let desired_api_version = if group.is_empty() {
        desired_version.to_string()
    } else {
        format!("{group}/{desired_version}")
    };
    let wrapper = json!({"clientConfig": configuration.client_config});
    let endpoint = webhook::endpoint(storage, &wrapper, "crd conversion").await?;
    let client = webhook::build_client(
        &wrapper,
        endpoint.resolve,
        CONVERSION_TIMEOUT,
        "crd conversion",
    )?;
    let payload = json!({
        "apiVersion": review_api_version.clone(),
        "kind": "ConversionReview",
        "request": {
            "uid": uid.clone(),
            "desiredAPIVersion": desired_api_version.clone(),
            "objects": objects,
        },
    });
    let response = client
        .post(&endpoint.url)
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(Error::Http(response.status()));
    }
    let response_body: Value = response.json().await?;
    parse_response(
        &response_body,
        &uid,
        &desired_api_version,
        expected_len,
        &review_api_version,
    )
}

fn parse_response(
    body: &Value,
    uid: &str,
    desired_api_version: &str,
    expected_len: usize,
    review_api_version: &str,
) -> Result<Vec<Value>, Error> {
    if body.get("apiVersion").and_then(Value::as_str) != Some(review_api_version) {
        return Err(Error::InvalidResponse(
            "response has the wrong ConversionReview apiVersion".to_string(),
        ));
    }
    if body.get("kind").and_then(Value::as_str) != Some("ConversionReview") {
        return Err(Error::InvalidResponse(
            "response kind is not ConversionReview".to_string(),
        ));
    }
    let response = body
        .get("response")
        .ok_or_else(|| Error::InvalidResponse("response is missing".to_string()))?;
    if response.get("uid").and_then(Value::as_str) != Some(uid) {
        return Err(Error::InvalidResponse(
            "response UID does not match request UID".to_string(),
        ));
    }
    let result = response
        .get("result")
        .ok_or_else(|| Error::InvalidResponse("response.result is missing".to_string()))?;
    if result.get("status").and_then(Value::as_str) != Some("Success") {
        let message = result
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("conversion webhook failed");
        return Err(Error::InvalidResponse(message.to_string()));
    }
    let objects = response
        .get("convertedObjects")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            Error::InvalidResponse("response.convertedObjects is missing".to_string())
        })?;
    if objects.len() != expected_len {
        return Err(Error::InvalidResponse(format!(
            "response.convertedObjects has {} objects, expected {expected_len}",
            objects.len()
        )));
    }
    for object in objects {
        if object.get("apiVersion").and_then(Value::as_str) != Some(desired_api_version) {
            return Err(Error::InvalidResponse(
                "a converted object has the wrong apiVersion".to_string(),
            ));
        }
    }
    Ok(objects.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_response_requires_matching_uid_version_and_count() {
        let body = json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "ConversionReview",
            "response": {
                "uid": "request-1",
                "result": {"status": "Success"},
                "convertedObjects": [{"apiVersion": "example.com/v1", "kind": "Widget"}]
            }
        });
        let objects = parse_response(
            &body,
            "request-1",
            "example.com/v1",
            1,
            "apiextensions.k8s.io/v1",
        )
        .expect("valid response");
        assert_eq!(objects.len(), 1);
    }

    #[test]
    fn conversion_response_rejects_a_failed_result() {
        let body = json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "ConversionReview",
            "response": {
                "uid": "request-1",
                "result": {"status": "Failure", "message": "unsupported"},
                "convertedObjects": []
            }
        });
        let error = parse_response(
            &body,
            "request-1",
            "example.com/v1",
            0,
            "apiextensions.k8s.io/v1",
        )
        .expect_err("failed result must be rejected");
        assert!(error.to_string().contains("unsupported"));
    }
}
