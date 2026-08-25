use super::context::E2eContext;
use super::skip_test;
use anyhow::Result;
use http::Request;
use kube::discovery;
use serde_json::json;
use std::process::Command;

async fn assert_can(
    client: &kube::Client,
    identity: &str,
    verb: &str,
    resource: &str,
) -> Result<()> {
    let (resource, group) = resource
        .split_once('.')
        .map_or((resource, ""), |(resource, group)| (resource, group));
    let request = Request::builder()
        .method("POST")
        .uri("/apis/authorization.k8s.io/v1/subjectaccessreviews")
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&json!({
            "apiVersion": "authorization.k8s.io/v1",
            "kind": "SubjectAccessReview",
            "spec": {
                "user": identity,
                "resourceAttributes": {"group": group, "resource": resource, "verb": verb}
            }
        }))?)?;
    let review: serde_json::Value = client.request(request).await?;
    anyhow::ensure!(
        review.pointer("/status/allowed") == Some(&serde_json::Value::Bool(true)),
        "{identity} cannot {verb} {resource}: SubjectAccessReview denied it: {review}"
    );
    Ok(())
}

fn service_is_active(name: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", name])
        .status()
        .is_ok_and(|status| status.success())
}

pub(super) async fn replacement_control_plane_identities_can_read_all_watch_inputs(
    context: &E2eContext,
) -> Result<()> {
    if !service_is_active("nodescheduler") {
        return Err(skip_test(
            "nodescheduler is not running; replacement-scheduler RBAC is not enabled",
        ));
    }
    if !service_is_active("nodecontroller") {
        return Err(skip_test(
            "nodecontroller is not running; replacement-controller RBAC is not enabled",
        ));
    }

    let scheduler_resources = [
        "persistentvolumes",
        "persistentvolumeclaims",
        "storageclasses.storage.k8s.io",
        "csinodes.storage.k8s.io",
        "csidrivers.storage.k8s.io",
        "csistoragecapacities.storage.k8s.io",
        "volumeattachments.storage.k8s.io",
    ];
    let controller_resources = [
        "persistentvolumes",
        "persistentvolumeclaims",
        "storageclasses.storage.k8s.io",
        "volumeattachments.storage.k8s.io",
    ];
    for (identity, resources) in [
        ("system:kube-scheduler", &scheduler_resources[..]),
        ("system:kube-controller-manager", &controller_resources[..]),
    ] {
        for resource in resources {
            for verb in ["get", "list", "watch"] {
                assert_can(&context.client, identity, verb, resource).await?;
            }
        }
    }

    let dra_group = discovery::group(&context.client, "resource.k8s.io").await.ok();
    if let Some(dra_group) = dra_group {
        let dra_resources = dra_group.recommended_resources();
        for resource in [
            "resourceclaims.resource.k8s.io",
            "deviceclasses.resource.k8s.io",
            "resourceslices.resource.k8s.io",
        ] {
            for verb in ["get", "list", "watch"] {
                assert_can(&context.client, "system:kube-scheduler", verb, resource).await?;
            }
        }
        if dra_resources
            .iter()
            .any(|(resource, _)| resource.plural == "resourceclaimtemplates")
        {
            for verb in ["get", "list", "watch"] {
                assert_can(
                    &context.client,
                    "system:kube-controller-manager",
                    verb,
                    "resourceclaimtemplates.resource.k8s.io",
                )
                .await?;
            }
        }
    }
    Ok(())
}
