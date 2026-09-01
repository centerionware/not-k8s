//! Bounded asynchronous audit webhook delivery.
//!
//! Kubernetes' webhook audit backend sends `audit.k8s.io/v1` `EventList`
//! documents to a remote HTTP API.  This backend keeps request handling
//! independent from that network hop: audit events enter a bounded queue,
//! are coalesced into batches, and are retried with a short exponential
//! backoff.  A full queue drops the newest event and reports that fact in
//! the nodeapiserver log rather than allowing audit delivery to exhaust the
//! API server's request memory.

use reqwest::Client;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc;

const DEFAULT_BUFFER_SIZE: usize = 10_000;
const DEFAULT_BATCH_SIZE: usize = 400;
const DEFAULT_BATCH_WAIT: Duration = Duration::from_secs(30);
const MAX_ATTEMPTS: usize = 3;
const RETRY_BACKOFF: Duration = Duration::from_millis(100);

#[derive(Clone, Debug)]
pub struct AuditWebhook {
    sender: mpsc::Sender<Value>,
}

impl AuditWebhook {
    /// Creates a webhook backend using Kubernetes' default batch limits.
    pub fn new(url: &str) -> Result<Self, String> {
        validate_url(url)?;

        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| format!("building audit webhook client: {error}"))?;
        Ok(Self::spawn(client, url.to_string()))
    }

    /// Creates a webhook backend from the standard kubeconfig-shaped audit
    /// webhook configuration file. The selected cluster supplies the server
    /// and optional CA, while the selected user supplies optional client
    /// certificate credentials.
    pub fn from_kubeconfig(path: &Path) -> Result<Self, String> {
        let config = crate::webhook_config::load_kubeconfig(path)?;
        validate_url(&config.url)?;
        let mut builder = Client::builder().timeout(Duration::from_secs(10));
        if let Some(ca_pem) = config.ca_pem {
            let certificate = reqwest::Certificate::from_pem(&ca_pem)
                .map_err(|error| format!("loading audit webhook certificate authority: {error}"))?;
            builder = builder.add_root_certificate(certificate);
        }
        if let Some(identity_pem) = config.identity_pem {
            let identity = reqwest::Identity::from_pem(&identity_pem)
                .map_err(|error| format!("loading audit webhook client identity: {error}"))?;
            builder = builder.identity(identity);
        }
        let client = builder
            .build()
            .map_err(|error| format!("building audit webhook client: {error}"))?;
        Ok(Self::spawn(client, config.url))
    }

    fn spawn(client: Client, url: String) -> Self {
        let (sender, receiver) = mpsc::channel(DEFAULT_BUFFER_SIZE);
        tokio::spawn(run(client, url, receiver));
        Self { sender }
    }

    /// Queues one event without waiting on the remote backend.
    pub fn enqueue(&self, event: &Value) {
        match self.sender.try_send(event.clone()) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("nodeapiserver: audit webhook queue is full; dropping an event");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!("nodeapiserver: audit webhook worker is closed; dropping an event");
            }
        }
    }
}

fn validate_url(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| format!("invalid audit webhook URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("audit webhook URL scheme must be http or https".to_string());
    }
    if parsed.host_str().map_or(true, str::is_empty) {
        return Err("audit webhook URL must include a host".to_string());
    }
    Ok(())
}

async fn run(client: Client, url: String, mut receiver: mpsc::Receiver<Value>) {
    while let Some(first) = receiver.recv().await {
        let mut batch = vec![first];
        let mut closed = false;
        let deadline = tokio::time::sleep(DEFAULT_BATCH_WAIT);
        tokio::pin!(deadline);

        while batch.len() < DEFAULT_BATCH_SIZE {
            tokio::select! {
                event = receiver.recv() => match event {
                    Some(event) => batch.push(event),
                    None => {
                        closed = true;
                        break;
                    }
                },
                _ = &mut deadline => break,
            }
        }

        send_batch(&client, &url, &batch).await;
        if closed {
            return;
        }
    }
}

async fn send_batch(client: &Client, url: &str, events: &[Value]) {
    let payload = event_list(events);

    for attempt in 0..MAX_ATTEMPTS {
        match client.post(url).json(&payload).send().await {
            Ok(response) if response.status().is_success() => return,
            Ok(response)
                if (response.status().is_server_error()
                    || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS)
                    && attempt + 1 < MAX_ATTEMPTS =>
            {
                tokio::time::sleep(RETRY_BACKOFF * (attempt as u32 + 1)).await;
            }
            Ok(response) => {
                tracing::warn!(
                    status = %response.status(),
                    "nodeapiserver: audit webhook rejected an event batch"
                );
                return;
            }
            Err(error) if attempt + 1 < MAX_ATTEMPTS => {
                tracing::warn!(error = ?error, "nodeapiserver: audit webhook request failed; retrying");
                tokio::time::sleep(RETRY_BACKOFF * (attempt as u32 + 1)).await;
            }
            Err(error) => {
                tracing::warn!(error = ?error, "nodeapiserver: audit webhook request failed after retries");
                return;
            }
        }
    }
}

fn event_list(events: &[Value]) -> Value {
    json!({
        "apiVersion": "audit.k8s.io/v1",
        "kind": "EventList",
        "items": events,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_http_urls() {
        let error = validate_url("file:///tmp/audit").unwrap_err();
        assert!(error.contains("http or https"));
    }

    #[test]
    fn rejects_urls_without_a_host() {
        let error = validate_url("http://").unwrap_err();
        assert!(error.contains("host") || error.contains("invalid audit webhook URL"));
    }

    #[test]
    fn batches_events_in_the_kubernetes_audit_event_list_shape() {
        let payload = event_list(&[json!({"auditID": "one"}), json!({"auditID": "two"})]);
        assert_eq!(payload["apiVersion"], "audit.k8s.io/v1");
        assert_eq!(payload["kind"], "EventList");
        assert_eq!(payload["items"].as_array().map(Vec::len), Some(2));
    }

}
