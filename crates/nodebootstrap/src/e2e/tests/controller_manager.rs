use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{ConfigMap, Namespace, Node, Pod, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use serde_json::{json, Value};
use std::process::Command;
use std::time::Duration;

fn nodecontroller_is_active() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", "nodecontroller"])
        .status()
        .is_ok_and(|status| status.success())
        || Command::new("pgrep")
            .args(["-x", "nodecontroller"])
            .status()
            .is_ok_and(|status| status.success())
}

fn require_nodecontroller() -> Result<()> {
    if nodecontroller_is_active() {
        Ok(())
    } else {
        Err(skip_test(
            "nodecontroller is not active; bootstrap with --controller-manager=nodecontroller to exercise the replacement controller manager",
        ))
    }
}

fn systemctl(action: &str, unit: &str) -> Result<()> {
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .context("checking the e2e runner's uid")?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let mut command = if uid == "0" {
        let mut command = Command::new("systemctl");
        command.args([action, unit]);
        command
    } else {
        let mut command = Command::new("sudo");
        command.args(["systemctl", action, unit]);
        command
    };
    let output = command
        .output()
        .with_context(|| format!("running systemctl {action} {unit}"))?;
    anyhow::ensure!(
        output.status.success(),
        "systemctl {action} {unit} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

async fn node_has_taint(nodes: &Api<Node>, name: &str, key: &str) -> Result<bool> {
    let node = nodes.get(name).await?;
    Ok(serde_json::to_value(node)?
        .pointer("/spec/taints")
        .and_then(Value::as_array)
        .is_some_and(|taints| {
            taints.iter().any(|taint| {
                taint.get("key").and_then(Value::as_str) == Some(key)
            })
        }))
}

async fn node_ready(nodes: &Api<Node>, name: &str) -> Result<bool> {
    let node = nodes.get(name).await?;
    Ok(serde_json::to_value(node)?
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .is_some_and(|conditions| {
            conditions.iter().any(|condition| {
                condition.get("type").and_then(Value::as_str) == Some("Ready")
                    && condition.get("status").and_then(Value::as_str) == Some("True")
            })
        }))
}

pub(super) async fn node_gets_a_pod_cidr_allocated(context: &E2eContext) -> Result<()> {
    require_nodecontroller()?;
    let nodes: Api<Node> = Api::all(context.client.clone());
    let name = format!("nodecontroller-cidr-{}", std::process::id());
    let node: Node = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Node",
        "metadata": {"name": name}
    }))?;
    nodes.create(&PostParams::default(), &node).await?;
    let result = context
        .wait_until("disposable Node to receive a PodCIDR", Duration::from_secs(60), || {
            let nodes = nodes.clone();
            async move {
                Ok(nodes
                    .get(&name)
                    .await?
                    .spec
                    .and_then(|spec| spec.pod_cidr)
                    .is_some_and(|cidr| !cidr.is_empty()))
            }
        })
        .await;
    let _ = nodes.delete(&name, &DeleteParams::default()).await;
    result
}

pub(super) async fn node_is_tainted_unreachable_after_heartbeat_loss_and_recovers(
    context: &E2eContext,
) -> Result<()> {
    require_nodecontroller()?;
    if !Command::new("systemctl")
        .args(["list-unit-files", "nodelet.service"])
        .status()
        .is_ok_and(|status| status.success())
    {
        return Err(skip_test(
            "nodelet.service is unavailable; heartbeat-loss recovery needs systemd to stop and start nodelet",
        ));
    }
    let nodes: Api<Node> = Api::all(context.client.clone());
    let name = nodes
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .find_map(|node| node.metadata.name)
        .context("the cluster has no Node to monitor")?;
    let taint_key = "node.kubernetes.io/unreachable";

    systemctl("stop", "nodelet.service")?;
    let tainted = context
        .wait_until(
            "node to receive the unreachable taint after its heartbeat expires",
            Duration::from_secs(90),
            || {
                let nodes = nodes.clone();
                async move { node_has_taint(&nodes, &name, taint_key).await }
            },
        )
        .await;
    let started = systemctl("start", "nodelet.service");
    tainted?;
    started?;

    context
        .wait_until("node to become Ready after nodelet restarts", Duration::from_secs(120), || {
            let nodes = nodes.clone();
            async move { node_ready(&nodes, &name).await }
        })
        .await?;
    context
        .wait_until("unreachable taint to clear after heartbeat recovery", Duration::from_secs(60), || {
            let nodes = nodes.clone();
            async move { Ok(!node_has_taint(&nodes, &name, taint_key).await?) }
        })
        .await
}

pub(super) async fn namespace_controller_deletes_contents_before_finalizing(
    context: &E2eContext,
) -> Result<()> {
    require_nodecontroller()?;
    let namespaces: Api<Namespace> = Api::all(context.client.clone());
    let name = format!("namespace-controller-{}", std::process::id());
    let namespace: Namespace = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {"name": name}
    }))?;
    namespaces.create(&PostParams::default(), &namespace).await?;
    let configmaps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &name);
    let configmap: ConfigMap = ConfigMap {
        metadata: ObjectMeta {
            name: Some("must-be-cleaned".to_string()),
            ..Default::default()
        },
        data: Some(std::collections::BTreeMap::from([(
            "proof".to_string(),
            "namespace-controller".to_string(),
        )])),
        ..Default::default()
    };
    configmaps
        .create(&PostParams::default(), &configmap)
        .await?;
    context
        .wait_until("namespace contents to exist before deletion", Duration::from_secs(30), || {
            let configmaps = configmaps.clone();
            async move { Ok(configmaps.get_opt("must-be-cleaned").await?.is_some()) }
        })
        .await?;
    namespaces.delete(&name, &DeleteParams::default()).await?;

    let contents_gone = context
        .wait_until("namespace controller to remove the ConfigMap", Duration::from_secs(120), || {
            let configmaps = configmaps.clone();
            async move { Ok(configmaps.get_opt("must-be-cleaned").await?.is_none()) }
        })
        .await;
    let namespace_gone = context
        .wait_until("namespace controller to remove the Namespace", Duration::from_secs(120), || {
            let namespaces = namespaces.clone();
            async move { Ok(namespaces.get_opt(&name).await?.is_none()) }
        })
        .await;
    contents_gone?;
    namespace_gone
}

fn endpoint_has_address(slice: &EndpointSlice, address: &str, ready: bool) -> Result<bool> {
    Ok(serde_json::to_value(slice)?
        .get("endpoints")
        .and_then(Value::as_array)
        .is_some_and(|endpoints| {
            endpoints.iter().any(|endpoint| {
                endpoint
                    .get("addresses")
                    .and_then(Value::as_array)
                    .is_some_and(|addresses| {
                        addresses.iter().any(|value| value.as_str() == Some(address))
                    })
                    && endpoint
                        .pointer("/conditions/ready")
                        .and_then(Value::as_bool)
                        == Some(ready)
            })
        }))
}

pub(super) async fn endpointslice_is_produced_for_a_selected_pod(
    context: &E2eContext,
) -> Result<()> {
    require_nodecontroller()?;
    let pods: Api<Pod> = Api::namespaced(context.client.clone(), &context.namespace);
    let services: Api<Service> = Api::namespaced(context.client.clone(), &context.namespace);
    let slices: Api<EndpointSlice> = Api::namespaced(context.client.clone(), &context.namespace);
    let service_name = "es-test-svc";
    let pod_name = "es-test-pod";
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": pod_name, "labels": {"app": service_name}},
        "spec": {"containers": [{"name": "app", "image": "busybox:latest", "command": ["sleep", "3600"]}]}
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    context
        .wait_until("EndpointSlice test Pod to reach Running with an IP", Duration::from_secs(90), || {
            let pods = pods.clone();
            async move {
                let pod = pods.get(pod_name).await?;
                let value = serde_json::to_value(pod)?;
                Ok(value.pointer("/status/phase").and_then(Value::as_str) == Some("Running")
                    && value.pointer("/status/podIP").and_then(Value::as_str).is_some_and(|ip| !ip.is_empty()))
            }
        })
        .await?;
    let pod_ip = serde_json::to_value(pods.get(pod_name).await?)?
        .pointer("/status/podIP")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("EndpointSlice test Pod has no PodIP")?;
    let service: Service = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {"name": service_name},
        "spec": {"selector": {"app": service_name}, "ports": [{"port": 80, "targetPort": 80}]}
    }))?;
    services.create(&PostParams::default(), &service).await?;
    let selector = format!("kubernetes.io/service-name={service_name}");
    context
        .wait_until("EndpointSlice to contain the selected Pod", Duration::from_secs(60), || {
            let slices = slices.clone();
            let pod_ip = pod_ip.clone();
            async move {
                Ok(slices
                    .list(&ListParams::default().labels(&selector))
                    .await?
                    .items
                    .iter()
                    .any(|slice| endpoint_has_address(slice, &pod_ip, true).unwrap_or(false)))
            }
        })
        .await?;
    pods.delete(pod_name, &DeleteParams::default()).await?;
    context
        .wait_until("EndpointSlice to drop the deleted Pod", Duration::from_secs(60), || {
            let slices = slices.clone();
            let pod_ip = pod_ip.clone();
            async move {
                Ok(!slices
                    .list(&ListParams::default().labels(&selector))
                    .await?
                    .items
                    .iter()
                    .any(|slice| endpoint_has_address(slice, &pod_ip, true).unwrap_or(false)))
            }
        })
        .await
}
