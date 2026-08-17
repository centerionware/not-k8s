//! serviceaccount-controller: ensures every namespace has a `default`
//! ServiceAccount. Pure event — a namespace either has one or it doesn't,
//! nothing to poll.
//!
//! # Why this exists despite Group C being explicitly out of scope for this PR
//!
//! Confirmed live, the hard way, twice: with `CONTROLLER_MANAGER=nodecontroller`
//! and no `default` ServiceAccount ever created, the apiserver's own
//! `ServiceAccount` admission plugin (loaded by default — confirmed in the
//! same CI run's log) rejects *every* pod that doesn't name one explicitly —
//! first `bootstrap-source.sh`'s own demo-pod smoke test, then
//! `deploy/lib/test/harness.sh`'s own namespace-setup preflight, which
//! `die()`s outright rather than let the whole suite fail on misleading
//! secondary errors. That isn't a Group A concern in the abstract; it's a
//! hard block on testing Group A at all. This is the minimum slice of Group
//! C needed to unblock that — not the rest of it (no finalizer-driven
//! namespace deletion, no root-ca-cert-publisher, no clusterrole
//! aggregation — see docs/CONTROLLER_MANAGER.md, Group C, for what's still
//! actually missing).

use anyhow::Result;
use crate::workqueue::KeyedWorkQueue;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Namespace, ServiceAccount};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, PostParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::HashMap;

const DEFAULT_SA_NAME: &str = "default";

fn is_terminating(ns: &Namespace) -> bool {
    ns.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Terminating")
}

async fn ensure_default_service_account(
    client: &Client,
    ns: &str,
    service_accounts: &HashMap<String, ServiceAccount>,
) {
    let api: Api<ServiceAccount> = Api::namespaced(client.clone(), ns);
    if service_accounts.contains_key(&format!("{ns}/{DEFAULT_SA_NAME}")) {
        return; // already there — the common case, every reconcile after the first
    }
    let sa = ServiceAccount {
        metadata: ObjectMeta {
            name: Some(DEFAULT_SA_NAME.to_string()),
            namespace: Some(ns.to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    match api.create(&PostParams::default(), &sa).await {
        Ok(_) => tracing::info!(namespace = %ns, "created the default ServiceAccount"),
        // Someone else (another reconcile of the same event, a restart racing
        // itself) created it first — not an error, the outcome we wanted.
        Err(kube::Error::Api(e)) if e.code == 409 => {}
        Err(e) => tracing::warn!(namespace = %ns, error = ?e, "failed to create the default ServiceAccount"),
    }
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let mut namespaces: HashMap<String, Namespace> = HashMap::new();
    let mut service_accounts: HashMap<String, ServiceAccount> = HashMap::new();
    let queue: KeyedWorkQueue<String> = KeyedWorkQueue::default();
    let mut stream = crate::watch::watch_namespaces(&client);
    let mut sa_stream = crate::watch::watch_service_accounts(&client);
    loop {
        tokio::select! {
            ev = stream.next() => match ev {
                Some(Ok(Event::Apply(ns))) | Some(Ok(Event::InitApply(ns))) => {
                    let name = ns.name_any();
                    namespaces.insert(name.clone(), ns);
                    queue.enqueue(name);
                }
                Some(Ok(Event::Delete(ns))) => { namespaces.remove(&ns.name_any()); }
                Some(Ok(Event::Init | Event::InitDone)) => {}
                Some(Err(e)) => tracing::warn!(error = ?e, "namespace watch error in serviceaccount-controller"),
                None => return Ok(()),
            },
            ev = sa_stream.next() => match ev {
                Some(Ok(Event::Apply(sa))) | Some(Ok(Event::InitApply(sa))) => {
                    let ns = sa.namespace().unwrap_or_default();
                    service_accounts.insert(format!("{ns}/{}", sa.name_any()), sa);
                }
                Some(Ok(Event::Delete(sa))) => {
                    let ns = sa.namespace().unwrap_or_default();
                    service_accounts.remove(&format!("{ns}/{}", sa.name_any()));
                    if sa.name_any() == DEFAULT_SA_NAME { queue.enqueue(ns); }
                }
                Some(Ok(Event::Init | Event::InitDone)) => {}
                Some(Err(e)) => tracing::warn!(error = ?e, "serviceaccount watch error in serviceaccount-controller"),
                None => return Ok(()),
            },
            ns = queue.pop() => {
                if let Some(namespace) = namespaces.get(&ns) {
                    if !is_terminating(namespace) {
                        ensure_default_service_account(&client, &ns, &service_accounts).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::NamespaceStatus;

    fn ns(phase: Option<&str>) -> Namespace {
        Namespace {
            status: Some(NamespaceStatus { phase: phase.map(str::to_string), ..Default::default() }),
            ..Default::default()
        }
    }

    #[test]
    fn a_terminating_namespace_is_skipped() {
        assert!(is_terminating(&ns(Some("Terminating"))));
    }

    #[test]
    fn an_active_namespace_is_not_terminating() {
        assert!(!is_terminating(&ns(Some("Active"))));
    }

    #[test]
    fn a_namespace_with_no_status_yet_is_not_treated_as_terminating() {
        assert!(!is_terminating(&Namespace::default()));
    }
}
