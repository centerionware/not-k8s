//! namespace-controller (Group C): finalizer-driven namespace deletion.
//!
//! A Namespace is not actually gone when its delete request returns. The
//! apiserver marks it `Terminating` and waits for the `kubernetes` finalizer
//! in `Namespace.spec.finalizers` to be removed. The namespace controller's
//! job is to delete every namespaced object first, then finalize the
//! Namespace through the dedicated `/finalize` subresource.
//!
//! The resource list is discovered rather than hard-coded. That includes
//! namespaced CRD instances, which is the important correctness property
//! here: deleting a Namespace must not leave an object of a newly installed
//! kind behind. A shared CRD informer refreshes the list when an installed
//! CRD changes, so installing a CRD never requires restarting nodecontroller.
//!
//! Cleanup is retried only for Namespaces that are already terminating. This
//! is an honest, low-frequency timer: objects with their own finalizers can
//! remain present after the delete request, and no Namespace event is
//! guaranteed when those finalizers eventually make progress.

use anyhow::{Context, Result};
use futures::StreamExt;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{Api, DeleteParams, DynamicObject, ListParams, PostParams};
use kube::discovery::{verbs, ApiCapabilities, ApiResource, Discovery, Scope};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use k8s_openapi::api::core::v1::{Namespace, NamespaceSpec};
use std::collections::HashMap;
use std::time::Duration;

const NAMESPACE_FINALIZER: &str = "kubernetes";
const RETRY_PERIOD: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct CleanupResource {
    api_resource: ApiResource,
}

fn is_terminating(namespace: &Namespace) -> bool {
    namespace.metadata.deletion_timestamp.is_some()
}

fn has_finalizer(finalizers: &Option<Vec<String>>, target: &str) -> bool {
    finalizers.as_ref().is_some_and(|items| items.iter().any(|item| item == target))
}

fn without_finalizer(finalizers: &Option<Vec<String>>, target: &str) -> Vec<String> {
    finalizers
        .as_ref()
        .into_iter()
        .flatten()
        .filter(|item| item.as_str() != target)
        .cloned()
        .collect()
}

fn is_cleanup_resource(_resource: &ApiResource, capabilities: &ApiCapabilities) -> bool {
    capabilities.scope == Scope::Namespaced
        && capabilities.supports_operation(verbs::LIST)
        && capabilities.supports_operation(verbs::DELETE)
}

fn discover_cleanup_resources(discovery: &Discovery) -> Vec<CleanupResource> {
    discovery
        .groups()
        .flat_map(|group| group.recommended_resources())
        .filter(|(resource, capabilities)| is_cleanup_resource(resource, capabilities))
        .map(|(api_resource, _)| CleanupResource { api_resource })
        .collect()
}

fn spawn_discovery_refresh(
    client: &Client,
    sender: &tokio::sync::mpsc::Sender<Discovery>,
) {
    let client = client.clone();
    let sender = sender.clone();
    tokio::spawn(async move {
        let discovery = crate::watch::discover_api(&client, "namespace-controller").await;
        let _ = sender.send(discovery).await;
    });
}

/// Delete everything visible in `namespace` for every discovered namespaced
/// resource. Returns true when another pass is needed, either because an
/// object still exists or because one of the list/delete calls failed.
async fn delete_namespace_contents(
    client: &Client,
    namespace: &str,
    resources: &[CleanupResource],
) -> bool {
    let mut needs_retry = false;
    let delete_params = DeleteParams {
        propagation_policy: Some(kube::api::PropagationPolicy::Background),
        ..Default::default()
    };

    for resource in resources {
        let api: Api<DynamicObject> =
            Api::namespaced_with(client.clone(), namespace, &resource.api_resource);
        let objects = match api.list(&ListParams::default()).await {
            Ok(objects) => objects,
            Err(kube::Error::Api(status)) if status.is_not_found() => continue,
            Err(error) => {
                needs_retry = true;
                tracing::warn!(
                    namespace,
                    kind = %resource.api_resource.kind,
                    api_version = %resource.api_resource.api_version,
                    error = ?error,
                    "namespace-controller failed to list namespaced objects"
                );
                continue;
            }
        };

        for object in objects.items {
            let name = object.name_any();
            match api.delete(&name, &delete_params).await {
                Ok(_) => {
                    needs_retry = true;
                    tracing::debug!(
                        namespace,
                        kind = %resource.api_resource.kind,
                        name = %name,
                        "namespace-controller requested deletion of namespaced object"
                    );
                }
                Err(kube::Error::Api(status)) if status.is_not_found() => {}
                Err(error) => {
                    needs_retry = true;
                    tracing::warn!(
                        namespace,
                        kind = %resource.api_resource.kind,
                        name = %name,
                        error = ?error,
                        "namespace-controller failed to delete namespaced object"
                    );
                }
            }
        }
    }

    needs_retry
}

async fn finalize_namespace(client: &Client, namespace: &Namespace) {
    let name = namespace.name_any();
    let finalizers = namespace
        .spec
        .as_ref()
        .map(|spec| without_finalizer(&spec.finalizers, NAMESPACE_FINALIZER))
        .unwrap_or_default();
    let mut finalized = namespace.clone();
    finalized.spec = Some(NamespaceSpec {
        finalizers: Some(finalizers),
    });

    let api: Api<Namespace> = Api::all(client.clone());
    if let Err(error) = api
        .replace_subresource("finalize", &name, &PostParams::default(), &finalized)
        .await
    {
        tracing::warn!(namespace = %name, error = ?error, "namespace-controller failed to remove the kubernetes finalizer");
    } else {
        tracing::info!(namespace = %name, "namespace-controller finalized Namespace");
    }
}

async fn reconcile_namespace(
    client: &Client,
    namespace: &Namespace,
    resources: &[CleanupResource],
) {
    let Some(spec) = namespace.spec.as_ref() else {
        return;
    };
    if !is_terminating(namespace) || !has_finalizer(&spec.finalizers, NAMESPACE_FINALIZER) {
        return;
    }

    if delete_namespace_contents(client, &namespace.name_any(), resources).await {
        return;
    }
    finalize_namespace(client, namespace).await;
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let discovery = crate::watch::discover_api(&client, "namespace-controller").await;
    let mut resources = discover_cleanup_resources(&discovery);
    tracing::info!(
        kind_count = resources.len(),
        "namespace-controller discovered namespaced resource kinds"
    );

    let mut namespaces: HashMap<String, Namespace> = HashMap::new();
    let mut crds: HashMap<String, CustomResourceDefinition> = HashMap::new();
    let queue: crate::workqueue::KeyedWorkQueue<String> = Default::default();
    let mut stream = crate::watch::watch_namespaces(&client);
    let mut crd_stream = crate::watch::watch_custom_resource_definitions(&client);
    let (refresh_sender, mut refresh_receiver) = tokio::sync::mpsc::channel(1);
    let mut refresh_in_flight = false;
    let mut refresh_pending = false;
    let mut retry = tokio::time::interval_at(
        tokio::time::Instant::now() + RETRY_PERIOD,
        RETRY_PERIOD,
    );
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            ev = stream.next() => match ev {
                Some(Ok(Event::Apply(namespace))) | Some(Ok(Event::InitApply(namespace))) => {
                    let name = namespace.name_any();
                    namespaces.insert(name.clone(), namespace);
                    queue.enqueue(name);
                }
                Some(Ok(Event::Delete(namespace))) => {
                    namespaces.remove(&namespace.name_any());
                }
                Some(Ok(Event::Init | Event::InitDone)) => {}
                Some(Err(error)) => tracing::warn!(error = ?error, "namespace watch error in namespace-controller"),
                None => return Ok(()),
            },
            ev = crd_stream.next() => match ev {
                Some(Ok(Event::Init)) => crds.clear(),
                Some(Ok(Event::InitApply(crd))) => {
                    crds.insert(crd.name_any(), crd);
                }
                Some(Ok(Event::Apply(crd))) => {
                    let name = crd.name_any();
                    let changed = crds
                        .get(&name)
                        .is_none_or(|previous| previous.spec != crd.spec);
                    crds.insert(name, crd);
                    if changed {
                        if refresh_in_flight {
                            refresh_pending = true;
                        } else {
                            refresh_in_flight = true;
                            spawn_discovery_refresh(&client, &refresh_sender);
                        }
                    }
                }
                Some(Ok(Event::Delete(crd))) => {
                    if crds.remove(&crd.name_any()).is_some() {
                        if refresh_in_flight {
                            refresh_pending = true;
                        } else {
                            refresh_in_flight = true;
                            spawn_discovery_refresh(&client, &refresh_sender);
                        }
                    }
                }
                Some(Ok(Event::InitDone)) => {}
                Some(Err(error)) => tracing::warn!(error = ?error, "CRD watch error in namespace-controller"),
                None => return Ok(()),
            },
            discovery = refresh_receiver.recv() => {
                let Some(discovery) = discovery else { return Ok(()) };
                refresh_in_flight = false;
                resources = discover_cleanup_resources(&discovery);
                tracing::info!(
                    kind_count = resources.len(),
                    "namespace-controller refreshed namespaced resource kinds after a CRD change"
                );
                for namespace in namespaces.values() {
                    if is_terminating(namespace) {
                        queue.enqueue(namespace.name_any());
                    }
                }
                if refresh_pending {
                    refresh_pending = false;
                    refresh_in_flight = true;
                    spawn_discovery_refresh(&client, &refresh_sender);
                }
            }
            name = queue.pop() => {
                if let Some(namespace) = namespaces.get(&name) {
                    reconcile_namespace(&client, namespace, &resources).await;
                }
            }
            _ = retry.tick() => {
                for namespace in namespaces.values() {
                    if is_terminating(namespace) {
                        queue.enqueue(namespace.name_any());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn namespace(finalizers: Option<Vec<&str>>, terminating: bool) -> Namespace {
        Namespace {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                deletion_timestamp: terminating.then(|| crate::k8s_time::from_chrono(crate::k8s_time::now())),
                ..Default::default()
            },
            spec: Some(NamespaceSpec {
                finalizers: finalizers.map(|items| items.into_iter().map(str::to_string).collect()),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn only_terminating_namespaces_need_cleanup() {
        assert!(!is_terminating(&namespace(Some(vec![NAMESPACE_FINALIZER]), false)));
        assert!(is_terminating(&namespace(Some(vec![NAMESPACE_FINALIZER]), true)));
    }

    #[test]
    fn finalizer_helpers_preserve_unrelated_finalizers() {
        let finalizers = Some(vec!["other".to_string(), NAMESPACE_FINALIZER.to_string()]);
        assert!(has_finalizer(&finalizers, NAMESPACE_FINALIZER));
        assert_eq!(without_finalizer(&finalizers, NAMESPACE_FINALIZER), vec!["other"]);
    }

    #[test]
    fn cleanup_discovery_requires_namespaced_list_and_delete() {
        let resource = ApiResource::from_gvk(&kube::core::GroupVersionKind::gvk("", "v1", "ConfigMap"));
        let capabilities = ApiCapabilities {
            scope: Scope::Namespaced,
            subresources: Vec::new(),
            operations: vec![verbs::LIST.to_string(), verbs::DELETE.to_string()],
        };
        assert!(is_cleanup_resource(&resource, &capabilities));

        let mut no_delete = capabilities.clone();
        no_delete.operations.retain(|operation| operation != verbs::DELETE);
        assert!(!is_cleanup_resource(&resource, &no_delete));
    }
}
