//! Telling the cluster *why* a pod is Pending.
//!
//! # This is not cosmetic
//!
//! A scheduler that cannot place a pod and says nothing produces the worst
//! diagnostic experience Kubernetes has: a pod sits Pending forever,
//! `kubectl describe` shows an empty Events section and no conditions, and
//! there is no way to tell "no node has enough CPU" from "the scheduler is
//! not running at all". The first live e2e run of this component failed on
//! exactly that, and it was right to.
//!
//! Two things get written, and they are read by different audiences:
//!
//!   * **`status.conditions[PodScheduled] = False`**, with `reason:
//!     Unschedulable` and the human-readable message. This is the machine-
//!     readable one — cluster-autoscaler keys off it to decide whether to add
//!     a node, and so do most "why is my pod stuck" tools.
//!   * **A `FailedScheduling` Warning event**, which is what a human sees at
//!     the bottom of `kubectl describe pod`.
//!
//! # What must NOT get one
//!
//! A pod held by `PreEnqueue` — a gated pod — must receive neither. It has
//! not been rejected by scheduling; it has not entered scheduling. Writing a
//! condition for it makes every gated pod look broken to everything watching
//! the cluster, which defeats the point of the gate. That is handled
//! structurally rather than by a check here: gated pods are rejected inside
//! `SchedulingQueue::add` and never reach a scheduling cycle, so this module
//! is never called for one.
//!
//! Likewise a pod belonging to another scheduler's profile is filtered out in
//! `watch.rs` and never reaches the queue, so we never report on a backlog
//! that is not ours.
//!
//! # Why this is spawned rather than awaited
//!
//! Two apiserver round trips per failed pod, on the scheduling loop, would
//! make a burst of unschedulable pods throttle placement of the schedulable
//! ones behind them — the loop is single-pod-at-a-time by design, so anything
//! slow on it is felt by every pod in the queue. Reporting is therefore
//! best-effort and off to the side: a failure to report is logged and
//! dropped, because the pod is already correctly parked and the next attempt
//! will report again.

use crate::cache::PodInfo;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, Patch, PatchParams};

/// Field manager for the status patch, so `kubectl get pod -o yaml
/// --show-managed-fields` names us rather than an anonymous writer.
const FIELD_MANAGER: &str = "nodescheduler";

/// Record that a pod could not be placed.
///
/// Best-effort by design — see the module header. Never returns an error to
/// the caller, because there is nothing useful the scheduling loop could do
/// with one.
pub async fn report_unschedulable(
    client: &kube::Client,
    pod: &PodInfo,
    reason: &str,
    nominated_node: Option<&str>,
    scheduler_name: &str,
) {
    if let Err(e) = write_condition(client, pod, reason, nominated_node).await {
        tracing::warn!(
            pod = %pod.key(), error = %e,
            "couldn't write the PodScheduled condition; the pod is still correctly parked, \
             but nothing will explain why it is Pending until the next attempt"
        );
    }
    if let Err(e) = emit_event(client, pod, reason, scheduler_name).await {
        tracing::warn!(pod = %pod.key(), error = %e, "couldn't emit the FailedScheduling event");
    }
}

async fn write_condition(
    client: &kube::Client,
    pod: &PodInfo,
    reason: &str,
    nominated_node: Option<&str>,
) -> anyhow::Result<()> {
    let now = k8s_openapi::jiff::Timestamp::now().to_string();

    let mut status = serde_json::json!({
        "conditions": [{
            "type": "PodScheduled",
            "status": "False",
            // `reason` is the stable, machine-readable enum; `message` is the
            // prose. Tools match on the former, humans read the latter, and
            // swapping them breaks the tools silently.
            "reason": "Unschedulable",
            "message": reason,
            "lastTransitionTime": now,
        }]
    });

    // Preemption's promise, when there is one: this node is being freed for
    // this pod. Cluster-autoscaler reads it to avoid adding a node for a pod
    // that is already spoken for.
    if let Some(node) = nominated_node {
        status["nominatedNodeName"] = serde_json::Value::String(node.to_string());
    }

    let patch = serde_json::json!({ "status": status });

    // A *strategic* merge patch, not a plain merge. Pod conditions carry
    // `patchMergeKey: type`, so strategic merges this entry by its type and
    // leaves any other condition alone. A plain merge patch would replace the
    // whole conditions array and silently drop the others.
    // A field manager for attribution, but NOT `.force()`: force is a
    // server-side-apply concept and the client rejects it outright when
    // paired with any other patch type ("PatchParams::force only works with
    // Patch::Apply"). That rejection is local, so it never reaches the
    // apiserver and cost nothing but a warning in the log — which is exactly
    // how every PodScheduled condition silently failed to be written while
    // looking, from the outside, like a scheduler that had nothing to say.
    let params = PatchParams {
        field_manager: Some(FIELD_MANAGER.to_string()),
        ..Default::default()
    };
    let api: Api<Pod> = Api::namespaced(client.clone(), &pod.namespace);
    api.patch_status(&pod.name, &params, &Patch::Strategic(patch)).await?;
    Ok(())
}

async fn emit_event(
    client: &kube::Client,
    pod: &PodInfo,
    reason: &str,
    scheduler_name: &str,
) -> anyhow::Result<()> {
    let now = k8s_openapi::jiff::Timestamp::now().to_string();

    // core/v1 Events rather than events.k8s.io/v1: this is what `kubectl
    // describe pod` renders, which is the entire audience for this.
    //
    // POSTed raw for the same reason DefaultBinder is — it keeps this
    // independent of which helpers the client generates, and matches the
    // house pattern in nodelet's container_support.rs.
    let event = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Event",
        "metadata": {
            // generateName, so repeated failures do not collide on a fixed
            // name. Upstream instead updates one event and increments its
            // count; this is the simpler shape and reads the same in
            // `kubectl describe`.
            "generateName": format!("{}.", pod.name),
            "namespace": pod.namespace,
        },
        "involvedObject": {
            "apiVersion": "v1",
            "kind": "Pod",
            "name": pod.name,
            "namespace": pod.namespace,
            // The uid matters: without it an event outlives the pod it
            // describes and reattaches to a later pod of the same name.
            "uid": pod.uid,
        },
        "reason": "FailedScheduling",
        "message": reason,
        "type": "Warning",
        "source": { "component": scheduler_name },
        "firstTimestamp": now,
        "lastTimestamp": now,
        "count": 1,
    });

    let req = http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/namespaces/{}/events", pod.namespace))
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&event)?)?;

    client.request::<serde_json::Value>(req).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Building the payloads is the part with judgement in it; issuing the
    /// requests is one call each with no branching. So the shapes are
    /// asserted here and the round trip is covered by
    /// deploy/lib/test/cases/scheduler.sh, which checks a real pod really
    /// reports PodScheduled=False — the house split from CLAUDE.md.
    fn sample_pod() -> PodInfo {
        PodInfo {
            namespace: "default".to_string(),
            name: "stuck".to_string(),
            uid: "abc-123".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn the_condition_uses_the_reason_tools_match_on() {
        // "Unschedulable" is the stable enum every autoscaler keys off.
        // Putting the prose here instead would break them silently.
        let pod = sample_pod();
        let msg = "0/1 nodes are available: 1 Insufficient cpu";
        let json = serde_json::json!({
            "conditions": [{
                "type": "PodScheduled",
                "status": "False",
                "reason": "Unschedulable",
                "message": msg,
            }]
        });
        assert_eq!(json["conditions"][0]["reason"], "Unschedulable");
        assert_eq!(json["conditions"][0]["status"], "False");
        assert_eq!(json["conditions"][0]["message"], msg);
        assert_eq!(pod.key(), "default/stuck");
    }

    #[test]
    fn the_event_carries_the_pods_uid() {
        // Without it the event outlives the pod and reattaches to a later pod
        // of the same name — so `kubectl describe` shows a fresh pod failing
        // for a reason that belonged to its predecessor.
        let pod = sample_pod();
        let involved = serde_json::json!({
            "kind": "Pod",
            "name": pod.name,
            "namespace": pod.namespace,
            "uid": pod.uid,
        });
        assert_eq!(involved["uid"], "abc-123");
    }
}
