//! ephemeral-volume-controller / resourceclaim-controller (Group H,
//! DRA-adjacent): creates a `ResourceClaim` from a Pod's
//! `spec.resourceClaims[].resourceClaimTemplateName` entries and records
//! the generated name in `pod.status.resourceClaimStatuses`. Pairs
//! directly with nodelet's existing DRA consumer side
//! (`runtime/cri/claims.rs`'s `resource_claim_object_name()` reads exactly
//! that status field to find the claim to fetch) — without this
//! controller, a Pod using the ergonomic "give me my own claim from this
//! template" shorthand never gets one, and nodelet has nothing to resolve.
//!
//! # Why raw requests, not a typed `kube::Api`
//!
//! Same reason nodelet's own `claims.rs` gives for `RawResourceClaim`:
//! k8s-openapi's generated `resource.k8s.io` types are now available (the
//! workspace bumped to `v1_34`, see `CLAUDE.md`), but the raw-request copies
//! haven't been retired yet — separate follow-up work, not a rider on the
//! version bump. Rather than duplicate nodelet's hand-written partial
//! struct (which only models the *status* fields nodelet reads),
//! this controller treats a `ResourceClaimTemplate`'s `spec.spec` as an
//! opaque `serde_json::Value` and copies it verbatim into the new
//! `ResourceClaim`'s own `spec` — this controller never needs to
//! understand a claim's device requests/selectors, only pass them through,
//! so an opaque blob is the more robust choice: it can't drift out of sync
//! with the real DRA API schema the way a hand-modeled subset could.
//!
//! # Scope of this slice
//!
//! **Deterministic naming (`{pod}-{pod-claim-name}`), not upstream's
//! random-suffixed name** — same create-race discipline this crate applies
//! everywhere else (see `deployment.rs`'s own history for the bug this
//! avoids): a create racing another reconcile of the same Pod lands on the
//! same name, so the loser's create fails `AlreadyExists` and is treated
//! as "already handled," not silently duplicated.
//!
//! **No explicit cleanup of the generated `ResourceClaim`** — it carries an
//! owner reference back to the Pod, so `garbage-collector-controller`
//! (Group D, generic across every discovered kind including
//! `resource.k8s.io/v1/ResourceClaim` — confirmed live in its own logs)
//! already cascades this correctly; a second deletion path here would be
//! redundant.
//!
//! **No `deviceClassName` validation or admission-equivalent checks** —
//! this controller only copies the template's spec through; a
//! malformed/unsatisfiable template is the DRA driver's problem to reject
//! at allocation time, not this controller's to pre-validate.

use anyhow::{Context, Result};
use crate::workqueue::KeyedWorkQueue;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Pod, PodResourceClaim, PodResourceClaimStatus};
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::HashMap;

const API_PREFIX: &str = "/apis/resource.k8s.io/v1";

/// Deterministic `ResourceClaim` object name for a given Pod's own
/// pod-claim entry — see module doc for why not a random suffix.
pub fn generated_claim_name(pod_name: &str, pod_claim_name: &str) -> String {
    format!("{pod_name}-{pod_claim_name}")
}

/// Which of `resource_claims` still need a `ResourceClaim` object created —
/// template-based entries (`resourceClaimTemplateName` set,
/// `resourceClaimName` unset) not already resolved in `statuses`. Pure,
/// unit-testable without any live Pod/ResourceClaim objects.
pub fn claims_needing_creation<'a>(
    resource_claims: &'a [PodResourceClaim],
    statuses: &[PodResourceClaimStatus],
) -> Vec<&'a PodResourceClaim> {
    resource_claims
        .iter()
        .filter(|c| c.resource_claim_name.is_none() && c.resource_claim_template_name.is_some())
        .filter(|c| !statuses.iter().any(|s| s.name == c.name))
        .collect()
}

async fn create_or_get_resource_claim(client: &Client, namespace: &str, body: &serde_json::Value) -> Result<(), kube::Error> {
    let bytes = serde_json::to_vec(body).expect("serializing ResourceClaim body");
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("{API_PREFIX}/namespaces/{namespace}/resourceclaims"))
        .header("Content-Type", "application/json")
        .body(bytes)
        .expect("building ResourceClaim POST request");
    match client.request::<serde_json::Value>(req).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(ref e)) if e.is_already_exists() => Ok(()),
        Err(e) => Err(e),
    }
}

fn owner_reference(pod: &Pod) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "name": pod.name_any(),
        "uid": pod.uid().unwrap_or_default(),
        "controller": true,
        "blockOwnerDeletion": true,
    })
}

async fn ensure_resource_claim(
    client: &Client,
    namespace: &str,
    pod: &Pod,
    pod_claim: &PodResourceClaim,
    templates: &HashMap<String, serde_json::Value>,
) -> Result<String> {
    let claim_name = generated_claim_name(&pod.name_any(), &pod_claim.name);
    let template_name = pod_claim.resource_claim_template_name.as_deref().context("missing resourceClaimTemplateName")?;
    let template = templates
        .get(&format!("{namespace}/{template_name}"))
        .cloned()
        .context("ResourceClaimTemplate is not in the informer cache")?;
    let template_metadata = template.pointer("/spec/metadata").cloned().unwrap_or(serde_json::json!({}));
    let claim_spec = template.pointer("/spec/spec").cloned().unwrap_or(serde_json::json!({}));

    let mut metadata = template_metadata;
    if !metadata.is_object() {
        metadata = serde_json::json!({});
    }
    let meta_obj = metadata.as_object_mut().expect("metadata is an object");
    meta_obj.insert("name".to_string(), serde_json::json!(claim_name));
    meta_obj.insert("namespace".to_string(), serde_json::json!(namespace));
    meta_obj.insert("ownerReferences".to_string(), serde_json::json!([owner_reference(pod)]));

    let body = serde_json::json!({
        "apiVersion": "resource.k8s.io/v1",
        "kind": "ResourceClaim",
        "metadata": metadata,
        "spec": claim_spec,
    });
    create_or_get_resource_claim(client, namespace, &body).await.context("creating ResourceClaim")?;
    Ok(claim_name)
}

async fn reconcile_pod(
    client: &Client,
    pod: &Pod,
    templates: &HashMap<String, serde_json::Value>,
) {
    let namespace = ns_of(pod);
    let name = pod.name_any();
    let pod_api: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    let Some(resource_claims) = pod.spec.as_ref().and_then(|s| s.resource_claims.clone()) else { return };
    if resource_claims.is_empty() {
        return;
    }
    let existing = pod.status.as_ref().and_then(|s| s.resource_claim_statuses.clone()).unwrap_or_default();
    let pending = claims_needing_creation(&resource_claims, &existing);
    if pending.is_empty() {
        return;
    }

    let mut statuses = existing;
    for pod_claim in pending {
        match ensure_resource_claim(client, &namespace, pod, pod_claim, templates).await {
            Ok(claim_name) => {
                statuses.push(PodResourceClaimStatus { name: pod_claim.name.clone(), resource_claim_name: Some(claim_name) });
            }
            Err(e) => {
                tracing::warn!(namespace = %namespace, pod = %name, pod_claim = %pod_claim.name, error = ?e, "failed to ensure ResourceClaim for pod-claim");
            }
        }
    }

    let patch = serde_json::json!({ "status": { "resourceClaimStatuses": statuses } });
    if let Err(e) = pod_api.patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch)).await {
        tracing::warn!(namespace = %namespace, pod = %name, error = ?e, "failed to patch Pod.status.resourceClaimStatuses");
    }
}

fn ns_of<K: ResourceExt>(obj: &K) -> String {
    obj.namespace().unwrap_or_default()
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let mut pods: std::collections::HashMap<String, Pod> = std::collections::HashMap::new();
    let mut templates: HashMap<String, serde_json::Value> = HashMap::new();
    let queue = KeyedWorkQueue::default();
    let mut pod_stream = crate::watch::watch_pods(&client);
    let mut template_stream = crate::watch::watch_resource_claim_templates(&client);
    loop {
        tokio::select! {
            ev = pod_stream.next() => match ev {
                Some(Ok(Event::Apply(pod))) | Some(Ok(Event::InitApply(pod))) => {
                    let key = format!("{}/{}", ns_of(&pod), pod.name_any());
                    pods.insert(key.clone(), pod);
                    queue.enqueue(key);
                }
                Some(Ok(Event::Delete(pod))) => { pods.remove(&format!("{}/{}", ns_of(&pod), pod.name_any())); }
                Some(Ok(Event::Init | Event::InitDone)) => {}
                Some(Err(e)) => tracing::warn!(error = ?e, "pod watch error in resourceclaim-controller"),
                None => return Ok(()),
            },
            ev = template_stream.next() => match ev {
                Some(Ok(Event::Apply(template))) | Some(Ok(Event::InitApply(template))) => {
                    let ns = template.namespace().unwrap_or_default();
                    let name = template.name_any();
                    let key = format!("{ns}/{name}");
                    if let Ok(value) = serde_json::to_value(&template) {
                        templates.insert(key, value);
                    }
                    for (pod_key, pod) in &pods {
                        if ns_of(pod) == ns && pod.spec.as_ref().and_then(|s| s.resource_claims.as_ref()).into_iter().flatten().any(|claim| claim.resource_claim_template_name.as_deref() == Some(name.as_str())) {
                            queue.enqueue(pod_key.clone());
                        }
                    }
                }
                Some(Ok(Event::Delete(template))) => {
                    templates.remove(&format!("{}/{}", template.namespace().unwrap_or_default(), template.name_any()));
                }
                Some(Ok(Event::Init | Event::InitDone)) => {}
                Some(Err(e)) => tracing::warn!(error = ?e, "ResourceClaimTemplate watch error in resourceclaim-controller"),
                None => return Ok(()),
            },
            key = queue.pop() => {
                if let Some(pod) = pods.get(&key).cloned() {
                    reconcile_pod(&client, &pod, &templates).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod_claim(name: &str, template: Option<&str>, direct: Option<&str>) -> PodResourceClaim {
        PodResourceClaim {
            name: name.to_string(),
            resource_claim_template_name: template.map(str::to_string),
            resource_claim_name: direct.map(str::to_string),
        }
    }

    #[test]
    fn a_template_based_claim_with_no_status_needs_creation() {
        let claims = vec![pod_claim("gpu", Some("gpu-template"), None)];
        let pending = claims_needing_creation(&claims, &[]);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].name, "gpu");
    }

    #[test]
    fn a_direct_reference_never_needs_creation() {
        let claims = vec![pod_claim("gpu", None, Some("existing-claim"))];
        assert!(claims_needing_creation(&claims, &[]).is_empty());
    }

    #[test]
    fn an_already_resolved_claim_is_skipped() {
        let claims = vec![pod_claim("gpu", Some("gpu-template"), None)];
        let statuses = vec![PodResourceClaimStatus { name: "gpu".to_string(), resource_claim_name: Some("pod-gpu".to_string()) }];
        assert!(claims_needing_creation(&claims, &statuses).is_empty());
    }

    #[test]
    fn generated_name_is_deterministic_and_pod_scoped() {
        assert_eq!(generated_claim_name("my-pod", "gpu"), "my-pod-gpu");
        assert_ne!(generated_claim_name("pod-a", "gpu"), generated_claim_name("pod-b", "gpu"));
    }
}
