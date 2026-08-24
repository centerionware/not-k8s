use anyhow::{bail, Context, Result};
use k8s_openapi::api::core::v1::{Namespace, ServiceAccount};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use kube::Client;
use std::future::Future;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(super) struct E2eContext {
    pub(super) client: Client,
    pub(super) namespace: String,
}

impl E2eContext {
    pub(super) async fn create(client: Client) -> Result<Self> {
        let namespace = format!(
            "nodebootstrap-e2e-{}-{}",
            std::process::id(),
            unique_suffix()
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
        context
            .wait_until(
                "the e2e namespace's default ServiceAccount",
                Duration::from_secs(30),
                || {
                    let service_accounts = service_accounts.clone();
                    async move { Ok(service_accounts.get_opt("default").await?.is_some()) }
                },
            )
            .await?;

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
            if check().await? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for {description}");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    pub(super) async fn cleanup(&self) {
        let namespaces: Api<Namespace> = Api::all(self.client.clone());
        let _ = namespaces
            .delete(&self.namespace, &DeleteParams::default())
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
