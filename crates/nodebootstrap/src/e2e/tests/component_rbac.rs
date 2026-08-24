use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use std::process::Command;

fn kubectl(args: &[&str]) -> Result<String> {
    let output = Command::new("kubectl")
        .args(args)
        .output()
        .with_context(|| format!("running kubectl {args:?}"))?;
    anyhow::ensure!(
        output.status.success(),
        "kubectl {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn service_is_active(name: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", name])
        .status()
        .is_ok_and(|status| status.success())
}

fn assert_can(identity: &str, verb: &str, resource: &str) -> Result<()> {
    let answer = kubectl(&[
        "auth",
        "can-i",
        &format!("--as={identity}"),
        verb,
        resource,
        "--all-namespaces",
    ])?;
    anyhow::ensure!(
        answer == "yes",
        "{identity} cannot {verb} {resource}: kubectl auth can-i returned {answer:?}",
    );
    Ok(())
}

pub(super) async fn replacement_control_plane_identities_can_read_all_watch_inputs(
    _context: &E2eContext,
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
                assert_can(identity, verb, resource)?;
            }
        }
    }

    let dra_resources = kubectl(&["api-resources", "--api-group=resource.k8s.io", "--no-headers"])?;
    if !dra_resources.trim().is_empty() {
        for resource in [
            "resourceclaims.resource.k8s.io",
            "deviceclasses.resource.k8s.io",
            "resourceslices.resource.k8s.io",
        ] {
            for verb in ["get", "list", "watch"] {
                assert_can("system:kube-scheduler", verb, resource)?;
            }
        }
        if dra_resources
            .lines()
            .any(|line| line.split_whitespace().next() == Some("resourceclaimtemplates"))
        {
            for verb in ["get", "list", "watch"] {
                assert_can(
                    "system:kube-controller-manager",
                    verb,
                    "resourceclaimtemplates.resource.k8s.io",
                )?;
            }
        }
    }
    Ok(())
}
