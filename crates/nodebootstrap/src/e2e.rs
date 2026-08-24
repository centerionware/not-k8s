//! Bootstrap-native end-to-end checks.
//!
//! This is the 0.7.1 migration seam from the shell e2e suite: checks live in
//! the bootstrap applet, use the Kubernetes API directly, and do not assume
//! k3s-specific flags, paths, services, or command wrappers. The test list is
//! intentionally small in this first slice; each migrated shell case should
//! become another entry here rather than growing a second shell-only harness.

use anyhow::{bail, Context, Result};
use k8s_openapi::api::core::v1::{Endpoints, Namespace, Node};
use kube::api::{Api, ListParams};
use kube::Client;
use std::net::IpAddr;
use std::path::Path;
use std::time::Instant;

const TESTS: &[&str] = &[
    "apiserver_serves_resources",
    "node_is_ready",
    "kubernetes_service_has_reachable_endpoint",
];

/// Run the selected bootstrap-native checks without re-running installation
/// or re-executing through sudo. This mode is deliberately safe to invoke on
/// an already-running cluster as an ordinary user.
pub fn run(only: Option<&str>) -> Result<()> {
    select_kubeconfig()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the bootstrap e2e runtime")?;
    runtime.block_on(run_async(only))
}

async fn run_async(only: Option<&str>) -> Result<()> {
    let selected = select_tests(only)?;
    let client = Client::try_default()
        .await
        .context("loading the Kubernetes client for bootstrap e2e; set KUBECONFIG or bootstrap the cluster first")?;

    println!("bootstrap e2e: {} test(s)", selected.len());
    let mut failures = Vec::new();
    let mut passed = 0;
    for name in selected {
        let started = Instant::now();
        print!("▶ {name} ... ");
        match run_test(name, client.clone()).await {
            Ok(()) => {
                passed += 1;
                println!("PASS ({}ms)", started.elapsed().as_millis());
            }
            Err(error) => {
                println!("FAIL ({}ms)", started.elapsed().as_millis());
                eprintln!("    {error:#}");
                failures.push(name);
            }
        }
    }

    if failures.is_empty() {
        println!("Results: {passed} passed, 0 failed");
        Ok(())
    } else {
        bail!(
            "bootstrap e2e failed: {} test(s): {}",
            failures.len(),
            failures.join(", ")
        )
    }
}

/// Prefer an explicitly supplied kubeconfig. A nodebootstrap-created cluster
/// has a stable fallback path, so `./bootstrap --e2e` works immediately after
/// installation without requiring the caller to export an implementation-
/// specific k3s path.
fn select_kubeconfig() -> Result<()> {
    if std::env::var_os("KUBECONFIG").is_some_and(|value| !value.is_empty()) {
        return Ok(());
    }

    let cfg = crate::config::Config::from_env()?;
    let candidate = cfg.kubeconfig_dir().join("admin.kubeconfig");
    if Path::new(&candidate).is_file() {
        std::env::set_var("KUBECONFIG", &candidate);
        tracing::info!(path = %candidate.display(), "using nodebootstrap admin kubeconfig for e2e");
    }
    Ok(())
}

fn select_tests(only: Option<&str>) -> Result<Vec<&'static str>> {
    let Some(only) = only else {
        return Ok(TESTS.to_vec());
    };
    let patterns: Vec<&str> = only.split(',').filter(|pattern| !pattern.is_empty()).collect();
    let selected: Vec<&'static str> = TESTS
        .iter()
        .copied()
        .filter(|name| patterns.iter().any(|pattern| name.contains(pattern)))
        .collect();
    if selected.is_empty() {
        bail!("--only={only} selected no bootstrap e2e tests; available tests: {}", TESTS.join(", "));
    }
    Ok(selected)
}

async fn run_test(name: &str, client: Client) -> Result<()> {
    match name {
        "apiserver_serves_resources" => apiserver_serves_resources(client).await,
        "node_is_ready" => node_is_ready(client).await,
        "kubernetes_service_has_reachable_endpoint" => kubernetes_service_has_reachable_endpoint(client).await,
        other => bail!("unknown bootstrap e2e test {other}"),
    }
}

async fn apiserver_serves_resources(client: Client) -> Result<()> {
    let api: Api<Namespace> = Api::all(client);
    let namespaces = api
        .list(&ListParams::default())
        .await
        .context("listing namespaces")?;
    anyhow::ensure!(!namespaces.items.is_empty(), "the apiserver returned no namespaces");
    Ok(())
}

async fn node_is_ready(client: Client) -> Result<()> {
    let api: Api<Node> = Api::all(client);
    let nodes = api.list(&ListParams::default()).await.context("listing nodes")?;
    anyhow::ensure!(!nodes.items.is_empty(), "the apiserver returned no nodes");

    let ready = nodes.items.iter().filter(|node| {
        node.status
            .as_ref()
            .and_then(|status| status.conditions.as_ref())
            .is_some_and(|conditions| conditions.iter().any(|condition| condition.type_ == "Ready" && condition.status == "True"))
    });
    let ready_count = ready.count();
    anyhow::ensure!(ready_count > 0, "no node reported status.conditions[Ready]=True");
    Ok(())
}

async fn kubernetes_service_has_reachable_endpoint(client: Client) -> Result<()> {
    let api: Api<Endpoints> = Api::namespaced(client, "default");
    let endpoints = api
        .get("kubernetes")
        .await
        .context("reading default/kubernetes Endpoints")?;

    let mut addresses = Vec::new();
    for subset in endpoints.subsets.unwrap_or_default() {
        for address in subset.addresses.unwrap_or_default() {
            addresses.push(address.ip);
        }
    }

    let reachable = addresses.iter().filter_map(|address| address.parse::<IpAddr>().ok()).any(|ip| !ip.is_loopback() && !ip.is_unspecified());
    anyhow::ensure!(reachable, "default/kubernetes has no non-loopback, non-unspecified endpoint (addresses: {addresses:?})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_filter_selects_the_initial_bootstrap_checks() {
        assert_eq!(select_tests(None).unwrap(), TESTS.to_vec());
    }

    #[test]
    fn only_matches_test_name_substrings_and_comma_separates() {
        assert_eq!(
            select_tests(Some("node_is_ready,kubernetes_service")).unwrap(),
            vec!["node_is_ready", "kubernetes_service_has_reachable_endpoint"]
        );
    }

    #[test]
    fn an_unknown_only_pattern_is_an_error() {
        assert!(select_tests(Some("does_not_exist")).is_err());
    }
}
