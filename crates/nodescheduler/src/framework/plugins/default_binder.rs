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
use k8s_openapi::api::core::v1::{Binding, ObjectReference};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

pub const NAME: &str = "DefaultBinder";

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
        match self.client.request::<serde_json::Value>(req).await {
            Ok(_) => Status::success(),
            Err(e) => Status::error(NAME, format!("binding {} to {node}: {e}", pod.key())),
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

    // Binding itself is not unit tested: it is one apiserver call with no
    // branching to exercise, and a mock of the apiserver would only assert
    // that this file calls the function it visibly calls. What actually needs
    // proving — that a bound pod ends up running on the node the scheduler
    // chose — is a property of the real cluster, and it is covered by
    // deploy/lib/test/cases/scheduler.sh. That split is the house rule from
    // CLAUDE.md: unit tests for logic, e2e against real infrastructure for
    // anything that talks to it.
}
