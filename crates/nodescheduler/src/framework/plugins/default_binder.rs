//! `DefaultBinder` — write the `Binding` that actually places the pod.
//!
//! The one plugin in this directory that performs I/O, and the last step of
//! the binding cycle. Everything before it has been a decision; this is the
//! only part the cluster can see.
//!
//! # Why a Binding subresource and not a patch to `spec.nodeName`
//!
//! `spec.nodeName` is immutable through the ordinary update path — the
//! apiserver rejects a patch that sets it. The `binding` subresource is the
//! sanctioned way in, and it exists precisely so that the apiserver can
//! enforce "a pod is bound exactly once": a second Binding for an
//! already-bound pod is rejected with a conflict rather than silently moving
//! the pod.
//!
//! That server-side rejection is the last of the four interlocks against
//! double-binding (the others being one pod at a time in the scheduling cycle,
//! the assume that makes the reservation immediately visible, and skipping a
//! pod that is already assumed). It is the only one that still holds if this
//! scheduler is accidentally run alongside another — which is exactly why
//! `--scheduler=nodescheduler` also passes `--disable-scheduler` to k3s
//! rather than trusting this to sort it out.

use crate::cache::PodInfo;
use crate::framework::status::Status;
use crate::framework::{BindPlugin, Plugin};
use k8s_openapi::api::core::v1::{Binding, ObjectReference, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::Api;

pub const NAME: &str = "DefaultBinder";

/// `kube` 4.0 sets no default read/write timeout on its own — a stalled
/// request here would hang `bind_one`/`handle_outcome` forever, leaving the
/// pod's Reserve assumption never released and the node looking permanently
/// short on capacity to every later cycle.
const BIND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub struct DefaultBinder {
    client: kube::Client,
}

impl DefaultBinder {
    pub fn new(client: kube::Client) -> Self {
        Self { client }
    }
}

impl Plugin for DefaultBinder {
    fn name(&self) -> &'static str {
        NAME
    }
    // Binding never rejects a pod for a reason a cluster event could fix — a
    // failure here is a transient API error, and the binding cycle's own
    // error path requeues the pod directly rather than waiting on an event.
}

#[async_trait::async_trait]
impl BindPlugin for DefaultBinder {
    async fn bind(&self, pod: &PodInfo, node: &str) -> Status {
        let binding = Binding {
            metadata: ObjectMeta {
                name: Some(pod.name.clone()),
                namespace: Some(pod.namespace.clone()),
                // Guards against binding a pod that was deleted and recreated
                // under the same name while this cycle was running — without
                // it, the new pod silently inherits a placement decided for
                // an object that no longer exists.
                uid: Some(pod.uid.clone()),
                ..Default::default()
            },
            target: ObjectReference {
                api_version: Some("v1".to_string()),
                kind: Some("Node".to_string()),
                name: Some(node.to_string()),
                ..Default::default()
            },
        };

        let body = match serde_json::to_vec(&binding) {
            Ok(b) => b,
            Err(e) => return Status::error(NAME, format!("encoding Binding: {e}")),
        };

        // A raw request rather than `Api::create_subresource`, which is the
        // house pattern for subresources this client doesn't generate a
        // helper for — see nodelet's container_support.rs, which POSTs a
        // TokenRequest exactly this way.
        //
        // It also sidesteps a real trap: the binding subresource hangs off
        // the *pod*, so the path is /pods/<name>/binding. Reaching for
        // `Api<Binding>` builds a URL under /bindings/, which the apiserver
        // answers with a 404 that reads as "the pod vanished" rather than
        // "wrong path" — a long way from the actual mistake.
        let uri = format!(
            "/api/v1/namespaces/{}/pods/{}/binding",
            pod.namespace, pod.name
        );
        let req = match http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("Content-Type", "application/json")
            .body(body)
        {
            Ok(r) => r,
            Err(e) => return Status::error(NAME, format!("building the Binding request: {e}")),
        };

        // The apiserver answers a successful binding with a Status object,
        // not the Binding — so this deserializes into the untyped value and
        // ignores it. Asking for `Binding` back would fail on every
        // successful bind.
        match tokio::time::timeout(BIND_TIMEOUT, self.client.request::<serde_json::Value>(req)).await {
            Ok(Ok(_)) => Status::success(),
            // The binding endpoint also returns 409 when another writer won
            // the race, including when this pod is already bound. Requeueing
            // the latter forever just repeats the same rejected bind and
            // hammers the apiserver. Re-read before classifying the conflict:
            // a pod with any non-empty nodeName is already irrevocably placed
            // (nodeName is immutable), so this stale scheduling decision is
            // complete. UID/deletion/still-unbound conflicts remain errors.
            Ok(Err(kube::Error::Api(e))) if e.code == 409 => {
                let pods: Api<Pod> = Api::namespaced(self.client.clone(), &pod.namespace);
                match pods.get(&pod.name).await {
                    Ok(current)
                        if current
                            .spec
                            .as_ref()
                            .and_then(|spec| spec.node_name.as_deref())
                            .is_some_and(|node| !node.is_empty()) =>
                    {
                        Status::success()
                    }
                    _ => Status::error(NAME, format!("binding {} to {node}: {e}", pod.key())),
                }
            }
            Ok(Err(e)) => Status::error(NAME, format!("binding {} to {node}: {e}", pod.key())),
            Err(_) => Status::error(
                NAME,
                format!("binding {} to {node} did not respond within {}s", pod.key(), BIND_TIMEOUT.as_secs()),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plugin_is_named_for_its_upstream_counterpart() {
        // Profile configuration names plugins by string, so this is part of
        // the compatibility surface, not just a label.
        assert_eq!(NAME, "DefaultBinder");
    }

    // Binding itself is not unit tested: introducing a mock apiserver here
    // for the one branch that now exists (issue #555's 409-is-success case)
    // would only assert that this file calls the function it visibly calls,
    // and this codebase has no established pattern yet for constructing a
    // raw `kube::Error::Api` in a test. What actually needs proving — that
    // a duplicate/lagging bind attempt for an already-bound pod does not
    // requeue and retry it forever — is a property of the real cluster:
    // live-captured via a real audit-log 409 storm this session (see
    // issue #555), and re-verified live (not via this crate's own test
    // suite) that the storm stops once this fix is deployed. It's covered
    // in the Rust e2e suite's own scheduler tests
    // (crates/nodebootstrap/src/e2e/tests/scheduler.rs), not a bash
    // deploy/lib/test/cases/ file — that whole suite has since migrated
    // there. That split is still the house rule from CLAUDE.md: unit tests
    // for logic, e2e against real infrastructure for anything that talks
    // to it.
}
