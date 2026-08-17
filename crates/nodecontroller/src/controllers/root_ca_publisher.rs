//! root-ca-cert-publisher-controller (Group C, Tier 0 despite the plan's
//! original "rest of Group C" deferral — pulled forward the same way
//! `serviceaccount-controller` was, confirmed load-bearing the hard way):
//! writes the `kube-root-ca.crt` ConfigMap into every namespace, the CA
//! bundle a Pod's default projected service-account-token volume mounts
//! alongside the token itself so anything inside the Pod building an
//! in-cluster `kubernetes.io/serviceaccount` client can actually verify
//! the apiserver's TLS certificate.
//!
//! # Why this is Tier 0 despite the plan's original Tier-2-adjacent framing
//!
//! Confirmed live in CI while verifying Group G's `attach-detach-controller`/
//! `persistentvolume-binder-controller`: the real CSI `external-provisioner`
//! sidecar builds its Kubernetes client the same in-cluster way any
//! properly-written controller does — from its own projected token volume's
//! `ca.crt`. Without this controller, that file is never created in any
//! namespace, so `external-provisioner` logs
//! `"Expected to load root CA config from
//! /var/run/secrets/kubernetes.io/serviceaccount/ca.crt ... no such file or
//! directory"` and never becomes a working client — no error surfaced to
//! the Pod's own status (it starts and stays Running), just a client that
//! can never actually reach the apiserver. `csi_pvc.sh`/`csi_attach.sh`'s
//! PVCs initially sat `Pending` forever with **zero** provisioning events.
//! This was one necessary fix, but not the final Group G root cause: once the
//! sidecar became a healthy client, live testing exposed the binder's missing
//! dynamic-provisioning and bind-completion annotations. Nodelet's own logs
//! carried the same
//! `WARN projected volume: failed to fetch ConfigMap source
//! configmap=kube-root-ca.crt` for every Pod's own default token the whole
//! time, previously dismissed as cosmetic since most workloads in this
//! project's e2e suite never build their own in-cluster client — CSI
//! sidecars are the first thing here that does.
//!
//! # Scope of this slice
//!
//! **The CA is read once at startup from nodecontroller's own ambient
//! kubeconfig** (`kube::config::Kubeconfig::read()`, the same
//! `$KUBECONFIG`-driven pattern this crate's own `kube::Client::try_default()`
//! already relies on — see `docs/CONTROLLER_MANAGER.md`'s note that this
//! crate deliberately doesn't solve cert bootstrap itself), not from a
//! `--root-ca-file` flag upstream also supports. If the kubeconfig's
//! cluster entry has neither `certificate-authority-data` nor
//! `certificate-authority`, this controller logs a warning and does
//! nothing rather than guessing — every namespace stays without the
//! ConfigMap, the same degraded state as before this controller existed.
//!
//! **No CA rotation support** — the CA is read once at process start; a
//! rotated cluster CA needs a nodecontroller restart to be picked up and
//! republished. Real, and named: upstream's own controller re-reads on a
//! configurable resync too, not truly "live," so this is a difference of
//! degree (once vs. periodic), not of kind.
//!
//! **No `kube-root-ca.crt` finalizer/protection** — deleting the ConfigMap
//! by hand just gets it recreated on the next Namespace reconcile (any
//! Namespace watch event re-checks every known namespace), not blocked.

use anyhow::{Context, Result};
use crate::workqueue::KeyedWorkQueue;
use futures::StreamExt;
use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, PostParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::{BTreeMap, HashMap};

pub const CONFIGMAP_NAME: &str = "kube-root-ca.crt";
const CA_KEY: &str = "ca.crt";

fn is_terminating(ns: &k8s_openapi::api::core::v1::Namespace) -> bool {
    ns.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Terminating")
}

/// Reads the CA bundle from the ambient kubeconfig's current-context
/// cluster entry — inline `certificate-authority-data` (base64) preferred,
/// falling back to a `certificate-authority` file path. `None` if neither
/// is present, so the caller can log and simply not run rather than guess.
fn load_root_ca_pem() -> Result<Option<Vec<u8>>> {
    let kubeconfig = kube::config::Kubeconfig::read().context("reading kubeconfig for root-ca-cert-publisher-controller")?;
    let context_name = match &kubeconfig.current_context {
        Some(c) => c.clone(),
        None => return Ok(None),
    };
    let Some(context) = kubeconfig.contexts.iter().find(|c| c.name == context_name).and_then(|c| c.context.as_ref()) else {
        return Ok(None);
    };
    let Some(cluster) = kubeconfig.clusters.iter().find(|c| c.name == context.cluster).and_then(|c| c.cluster.as_ref()) else {
        return Ok(None);
    };
    if let Some(data) = &cluster.certificate_authority_data {
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD.decode(data).context("decoding certificate-authority-data")?;
        return Ok(Some(decoded));
    }
    if let Some(path) = &cluster.certificate_authority {
        let bytes = std::fs::read(path).with_context(|| format!("reading certificate-authority file {path}"))?;
        return Ok(Some(bytes));
    }
    Ok(None)
}

async fn reconcile_namespace(
    client: &Client,
    namespace: &str,
    ca_pem: &str,
    configmaps: &HashMap<String, ConfigMap>,
) {
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    if let Some(existing) = configmaps.get(&format!("{namespace}/{CONFIGMAP_NAME}")) {
        if existing.data.as_ref().and_then(|d| d.get(CA_KEY)).map(String::as_str) == Some(ca_pem) {
            return;
        }
    }
    let mut data = BTreeMap::new();
    data.insert(CA_KEY.to_string(), ca_pem.to_string());
    let cm = ConfigMap {
        metadata: ObjectMeta { name: Some(CONFIGMAP_NAME.to_string()), namespace: Some(namespace.to_string()), ..Default::default() },
        data: Some(data),
        ..Default::default()
    };
    match api.create(&PostParams::default(), &cm).await {
        Ok(_) => tracing::info!(namespace = %namespace, "root-ca-cert-publisher-controller published kube-root-ca.crt"),
        Err(kube::Error::Api(ref e)) if e.is_already_exists() => {
            // Raced another reconcile of the same namespace, or the
            // ConfigMap already existed with different data than expected
            // above (e.g. an operator hand-edited it) — patch it to the
            // real CA rather than erroring.
            let patch = serde_json::json!({ "data": { CA_KEY: ca_pem } });
            if let Err(e) = api.patch(CONFIGMAP_NAME, &Default::default(), &kube::api::Patch::Merge(&patch)).await {
                tracing::warn!(namespace = %namespace, error = ?e, "failed to update kube-root-ca.crt ConfigMap after a create race");
            }
        }
        Err(e) => tracing::warn!(namespace = %namespace, error = ?e, "failed to create kube-root-ca.crt ConfigMap"),
    }
}

pub async fn run(client: Client, _cfg: &crate::config::Config) -> Result<()> {
    let Some(ca_bytes) = load_root_ca_pem()? else {
        tracing::warn!("root-ca-cert-publisher-controller found no CA data in the ambient kubeconfig — not publishing kube-root-ca.crt anywhere");
        return Ok(());
    };
    let ca_pem = String::from_utf8(ca_bytes).context("cluster CA data is not valid UTF-8 PEM")?;

    let mut namespaces: HashMap<String, k8s_openapi::api::core::v1::Namespace> = HashMap::new();
    let mut configmaps: HashMap<String, ConfigMap> = HashMap::new();
    let queue: KeyedWorkQueue<String> = KeyedWorkQueue::default();
    let mut ns_stream = crate::watch::watch_namespaces(&client);
    let mut cm_stream = crate::watch::watch_config_maps(&client);
    loop {
        tokio::select! {
            ev = ns_stream.next() => match ev {
                Some(Ok(Event::Apply(ns))) | Some(Ok(Event::InitApply(ns))) => {
                    let name = ns.name_any();
                    namespaces.insert(name.clone(), ns);
                    queue.enqueue(name);
                }
                Some(Ok(Event::Delete(ns))) => { namespaces.remove(&ns.name_any()); }
                Some(Ok(Event::Init | Event::InitDone)) => {}
                Some(Err(e)) => tracing::warn!(error = ?e, "namespace watch error in root-ca-cert-publisher-controller"),
                None => return Ok(()),
            },
            ev = cm_stream.next() => match ev {
                Some(Ok(Event::Apply(cm))) | Some(Ok(Event::InitApply(cm))) => {
                    let ns = cm.namespace().unwrap_or_default();
                    let key = format!("{ns}/{}", cm.name_any());
                    configmaps.insert(key, cm.clone());
                    if cm.name_any() == CONFIGMAP_NAME { queue.enqueue(ns); }
                }
                Some(Ok(Event::Delete(cm))) => {
                    let ns = cm.namespace().unwrap_or_default();
                    configmaps.remove(&format!("{ns}/{}", cm.name_any()));
                    if cm.name_any() == CONFIGMAP_NAME { queue.enqueue(ns); }
                }
                Some(Ok(Event::Init | Event::InitDone)) => {}
                Some(Err(e)) => tracing::warn!(error = ?e, "ConfigMap watch error in root-ca-cert-publisher-controller"),
                None => return Ok(()),
            },
            namespace = queue.pop() => {
                if let Some(ns) = namespaces.get(&namespace) {
                    if !is_terminating(ns) {
                        reconcile_namespace(&client, &namespace, &ca_pem, &configmaps).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_the_well_known_name() {
        assert_eq!(CONFIGMAP_NAME, "kube-root-ca.crt");
        assert_eq!(CA_KEY, "ca.crt");
    }
}
