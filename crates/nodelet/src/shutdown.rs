//! Real kubelet's Graceful Node Shutdown feature: without this, an edge
//! device that gets powered off (or a `systemctl poweroff`/`reboot`) just
//! has every container SIGKILLed mid-flight by whatever the OS does on its
//! way down — no `preStop`, no drain, no chance for a database container to
//! flush. This holds a systemd-logind shutdown-delay inhibitor lock and, on
//! the `PrepareForShutdown` signal, drives each pod through the same
//! graceful-termination path `PodRuntime::remove_pod` already gives a
//! normal delete (`preStop` + a bounded `StopContainer` timeout), within a
//! fixed time budget, before releasing the lock so shutdown actually
//! proceeds.
//!
//! Opt-in, matching upstream: `Config::shutdown_grace_period_seconds` is `0`
//! (disabled) unless `NODELET_SHUTDOWN_GRACE_PERIOD_SECS` is set — `run()`
//! is a no-op in that case, not even connecting to D-Bus.
//!
//! **Needs live-cluster/live-systemd validation.** This was written and
//! unit-tested for its pure scheduling logic (which pods go first, how the
//! time budget splits, how each pod's grace period gets capped to fit) but
//! the D-Bus glue (`Connection::system()`, the `Inhibit` call, the
//! `PrepareForShutdown` signal stream) has never been exercised against a
//! real systemd-logind — there's no session/system bus in the environment
//! that built this. See `docs/GAP_CLOSURE.md`'s round 9 notes for how to
//! spot-check it manually (`loginctl list-inhibitors` while nodelet is
//! running, then `systemctl poweroff`/`reboot` and watch pod termination
//! order in the logs before the box actually goes down).

use crate::config::Config;
use crate::runtime::PodRuntime;
use futures::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};
use kube::Client;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};
use zbus::zvariant::OwnedFd;
use zbus::{Connection, Proxy};

const LOGIND_SERVICE: &str = "org.freedesktop.login1";
const LOGIND_PATH: &str = "/org/freedesktop/login1";
const LOGIND_MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";

/// No-op when disabled (the default) — doesn't even touch D-Bus, so nodelet
/// on a non-systemd host or in a container without a system bus mounted
/// behaves exactly as before this feature existed.
// `inhibitor` is only ever held for its Drop side effect (closing the fd
// releases the lock) — every assignment to it is intentionally "unread" by
// the time the function returns or loops around.
#[allow(unused_assignments)]
pub async fn run(client: Client, runtime: Arc<dyn PodRuntime>, cfg: Config) {
    if cfg.shutdown_grace_period_seconds == 0 {
        return;
    }

    let connection = match Connection::system().await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = ?e, "graceful node shutdown: couldn't connect to the system D-Bus bus (no systemd-logind, or no access to it) — feature disabled for this run");
            return;
        }
    };
    let proxy = match Proxy::new(&connection, LOGIND_SERVICE, LOGIND_PATH, LOGIND_MANAGER_IFACE).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = ?e, "graceful node shutdown: couldn't build a proxy to systemd-logind — feature disabled for this run");
            return;
        }
    };
    let mut shutdown_signal = match proxy.receive_signal("PrepareForShutdown").await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = ?e, "graceful node shutdown: couldn't subscribe to logind's PrepareForShutdown signal — feature disabled for this run");
            return;
        }
    };

    let mut inhibitor = acquire_inhibitor(&proxy).await;
    if inhibitor.is_none() {
        warn!("graceful node shutdown: couldn't acquire the shutdown-delay inhibitor lock (needs root, or a permissive logind polkit policy) — shutdown will proceed without giving pods a chance to terminate cleanly until this succeeds");
    }

    info!(
        grace_period_secs = cfg.shutdown_grace_period_seconds,
        critical_grace_period_secs = cfg.shutdown_grace_period_critical_seconds,
        "graceful node shutdown: watching for PrepareForShutdown"
    );

    while let Some(msg) = shutdown_signal.next().await {
        let active: bool = match msg.body().deserialize() {
            Ok(v) => v,
            Err(e) => {
                warn!(error = ?e, "graceful node shutdown: couldn't parse PrepareForShutdown signal body; ignoring");
                continue;
            }
        };

        if active {
            info!("graceful node shutdown: PrepareForShutdown(true) received — terminating pods before releasing the inhibitor lock");
            terminate_all_pods_for_shutdown(&client, &runtime, &cfg).await;
            // Dropping the held fd closes it, which is exactly what tells
            // logind this inhibitor is done — shutdown can now proceed.
            drop(inhibitor.take());
        } else {
            // Shutdown was cancelled (e.g. another inhibitor blocked it, or
            // a user aborted it) — re-arm for the next attempt.
            info!("graceful node shutdown: PrepareForShutdown(false) received — shutdown was cancelled, re-acquiring the inhibitor lock");
            inhibitor = acquire_inhibitor(&proxy).await;
        }
    }
}

async fn acquire_inhibitor(proxy: &Proxy<'_>) -> Option<OwnedFd> {
    match proxy
        .call::<_, _, OwnedFd>(
            "Inhibit",
            &(
                "shutdown",
                "nodelet",
                "nodelet needs time to gracefully terminate pods before host shutdown",
                "delay",
            ),
        )
        .await
    {
        Ok(fd) => Some(fd),
        Err(e) => {
            warn!(error = ?e, "graceful node shutdown: Inhibit call to systemd-logind failed");
            None
        }
    }
}

async fn terminate_all_pods_for_shutdown(client: &Client, runtime: &Arc<dyn PodRuntime>, cfg: &Config) {
    let api: Api<Pod> = Api::all(client.clone());
    let params = ListParams::default().fields(&format!("spec.nodeName={}", cfg.node_name));
    let pods = match api.list(&params).await {
        Ok(list) => list.items,
        Err(e) => {
            warn!(error = ?e, "graceful node shutdown: failed to list pods; nothing will be gracefully terminated");
            return;
        }
    };

    let (non_critical, critical) = split_by_criticality(&pods);
    let (non_critical_budget, critical_budget) =
        budget_split(cfg.shutdown_grace_period_seconds, cfg.shutdown_grace_period_critical_seconds);

    info!(
        non_critical = non_critical.len(),
        critical = critical.len(),
        non_critical_budget,
        critical_budget,
        "graceful node shutdown: terminating pods"
    );
    // Non-critical pods first — matches real kubelet's ordering, and gives
    // ordinary workloads first crack at a clean exit while critical
    // system-cluster-critical/system-node-critical pods (kube-system
    // add-ons) keep serving as long as possible.
    terminate_pods(client, runtime, non_critical, non_critical_budget).await;
    terminate_pods(client, runtime, critical, critical_budget).await;
}

/// Splits `pods` into (non-critical, critical) — anything already being
/// deleted is dropped from both, there's nothing left for this pass to do
/// for it. Reuses `eviction::is_critical`'s exact
/// `system-node-critical`/`system-cluster-critical` priority-class check —
/// same "essential workload" definition as node-pressure eviction.
fn split_by_criticality(pods: &[Pod]) -> (Vec<&Pod>, Vec<&Pod>) {
    pods.iter()
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .partition(|p| !crate::eviction::is_critical(p))
}

/// Splits the total shutdown budget into (non-critical, critical) shares.
/// `critical_secs` is clamped to `total_secs` — a misconfigured critical
/// budget larger than the total must not produce a negative non-critical
/// share.
fn budget_split(total_secs: u64, critical_secs: u64) -> (u64, u64) {
    let critical = critical_secs.min(total_secs);
    let non_critical = total_secs.saturating_sub(critical);
    (non_critical, critical)
}

/// A pod's own `terminationGracePeriodSeconds` (defaulting to 30, same as
/// every other pod-deletion path in this codebase) capped to whatever's
/// actually left in the shutdown budget — a pod asking for a 5-minute grace
/// period doesn't get it if the whole node only has 30 seconds before power
/// loss.
fn capped_grace_period(pod_grace_seconds: Option<i64>, budget_secs: u64) -> i64 {
    let pod_grace = pod_grace_seconds.unwrap_or(30).max(0);
    pod_grace.min(budget_secs as i64)
}

async fn terminate_pods(client: &Client, runtime: &Arc<dyn PodRuntime>, pods: Vec<&Pod>, budget_secs: u64) {
    if pods.is_empty() || budget_secs == 0 {
        return;
    }
    let tasks = pods.into_iter().map(|pod| {
        let client = client.clone();
        let runtime = runtime.clone();
        let pod = pod.clone();
        async move { terminate_one_pod(&client, &runtime, &pod, budget_secs).await }
    });
    // A budget deadline around the whole batch, not per-pod — one pod
    // hanging in a slow preStop shouldn't get to consume the entire
    // remaining shutdown window if the others already finished quickly, but
    // the batch as a whole still can't run past what's left before power
    // loss.
    if tokio::time::timeout(Duration::from_secs(budget_secs), futures::future::join_all(tasks)).await.is_err() {
        warn!(budget_secs, "graceful node shutdown: time budget expired before every pod finished terminating");
    }
}

async fn terminate_one_pod(client: &Client, runtime: &Arc<dyn PodRuntime>, pod: &Pod, budget_secs: u64) {
    let (Some(ns), Some(name)) = (pod.metadata.namespace.as_deref(), pod.metadata.name.as_deref()) else {
        return;
    };

    let pod_api: Api<Pod> = Api::namespaced(client.clone(), ns);
    // Best-effort: surface why before teardown actually lands, same pattern
    // as eviction.rs. A failed status patch shouldn't block termination —
    // there may be no time left to retry it before power loss anyway.
    let status_patch = serde_json::json!({
        "status": {
            "phase": "Failed",
            "reason": "Terminated",
            "message": "Pod was terminated in response to imminent node shutdown.",
        }
    });
    let _ = pod_api.patch_status(name, &PatchParams::default(), &Patch::Merge(&status_patch)).await;

    // Drive the runtime directly rather than deleting the object and
    // waiting for the watch controller to notice — there's no time to
    // spare for a watch round-trip when shutdown is imminent. Cap the
    // pod's own terminationGracePeriodSeconds to what's actually left in
    // this shutdown's budget so remove_pod()'s preStop + StopContainer
    // timeout can't run longer than the node actually has.
    let mut capped = pod.clone();
    if let Some(spec) = capped.spec.as_mut() {
        spec.termination_grace_period_seconds =
            Some(capped_grace_period(spec.termination_grace_period_seconds, budget_secs));
    }
    if let Err(e) = runtime.remove_pod(&capped).await {
        warn!(pod = %format!("{ns}/{name}"), error = ?e, "graceful node shutdown: failed to terminate pod");
    }

    // Best-effort cleanup of the apiserver object too — the node's about to
    // lose power, not necessarily reboot into a fresh apiserver, so leaving
    // a stale Pod object behind would otherwise linger until something else
    // garbage-collects it.
    let dp = DeleteParams { grace_period_seconds: Some(0), ..Default::default() };
    let _ = pod_api.delete(name, &dp).await;
}

#[cfg(test)]
#[path = "shutdown_tests/split_by_criticality.rs"]
mod tests_split_by_criticality;
#[cfg(test)]
#[path = "shutdown_tests/budget_split.rs"]
mod tests_budget_split;
#[cfg(test)]
#[path = "shutdown_tests/capped_grace_period.rs"]
mod tests_capped_grace_period;
