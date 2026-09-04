use anyhow::{bail, Context, Result};
use k8s_openapi::api::core::v1::{Namespace, ServiceAccount};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use kube::Client;
use std::future::Future;
use std::time::{Duration, Instant};

pub(super) const API_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(super) struct E2eContext {
    pub(super) client: Client,
    pub(super) namespace: String,
}

impl E2eContext {
    pub(super) async fn create(client: Client) -> Result<Self> {
        // Keep the generated namespace short enough that a test Pod's
        // setHostnameAsFQDN value can fit Linux's 64-byte hostname limit.
        // The process id plus the low 32 bits of the monotonic-ish clock
        // value still make collisions across concurrent runners negligible,
        // while the full nanosecond value made otherwise valid FQDN tests
        // fail before the container could start.
        let namespace = format!(
            "nk-e2e-{}-{:08x}",
            std::process::id(),
            unique_suffix() as u32
        );
        let namespaces: Api<Namespace> = Api::all(client.clone());
        namespaces
            .create(
                &PostParams::default(),
                &Namespace {
                    metadata: ObjectMeta {
                        name: Some(namespace.clone()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .await
            .with_context(|| format!("creating e2e namespace {namespace}"))?;

        let service_accounts: Api<ServiceAccount> =
            Api::namespaced(client.clone(), &namespace);
        let context = Self { client, namespace };
        if let Err(error) = context
            .wait_until(
                "the e2e namespace's default ServiceAccount",
                Duration::from_secs(30),
                || {
                    let service_accounts = service_accounts.clone();
                    async move { Ok(service_accounts.get_opt("default").await?.is_some()) }
                },
            )
            .await
        {
            // A failed context setup used to leak its Namespace. Every
            // subsequent setup then added more terminating objects while the
            // namespace controller was already behind, turning one missed
            // ServiceAccount into a shard-wide cascade of timeouts.
            context.cleanup().await;
            return Err(error);
        }

        Ok(context)
    }

    pub(super) async fn wait_until<F, Fut>(
        &self,
        description: &str,
        timeout: Duration,
        mut check: F,
    ) -> Result<()>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<bool>>,
    {
        let deadline = Instant::now() + timeout;
        loop {
            match tokio::time::timeout(API_REQUEST_TIMEOUT, check()).await {
                Ok(result) => {
                    if result? {
                        return Ok(());
                    }
                }
                Err(_) => {}
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for {description}");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    pub(super) async fn cleanup(&self) {
        let namespaces: Api<Namespace> = Api::all(self.client.clone());
        if namespaces
            .delete(&self.namespace, &DeleteParams::default())
            .await
            .is_err()
        {
            return;
        }

        // Namespace deletion is asynchronous. Wait for the namespace to be
        // gone before starting another test, otherwise terminating Pods can
        // still consume the single runner node's pod budget and make the
        // next test fail with an unrelated scheduling error.
        let _ = self
            .wait_until(
                "the e2e namespace and its resources to be deleted",
                Duration::from_secs(30),
                || {
                    let namespaces = namespaces.clone();
                    let namespace = self.namespace.clone();
                    async move { Ok(namespaces.get_opt(&namespace).await?.is_none()) }
                },
            )
            .await;
    }
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

pub(super) fn labels(value: &str) -> ListParams {
    ListParams::default().labels(value)
}
