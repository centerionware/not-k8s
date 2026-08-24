//! Bootstrap-native end-to-end checks.
//!
//! This is the 0.7.1 migration seam from the shell e2e suite: checks live in
//! the bootstrap applet, use the Kubernetes API directly, and do not assume
//! k3s-specific flags, paths, services, or command wrappers. Each migrated
//! shell case becomes another entry here, with the metadata used
//! by CI to keep CSI/DRA setup together instead of scattering it across every
//! runner.

use anyhow::{bail, Context, Result};
use k8s_openapi::api::core::v1::{Endpoints, Namespace, Node};
use kube::api::{Api, ListParams};
use kube::Client;
use std::net::IpAddr;
use std::path::Path;
use std::time::Instant;

const CSI_DRA_SHARDS: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestGroup {
    General,
    CsiDra,
}

#[derive(Clone, Copy, Debug)]
struct TestCase {
    name: &'static str,
    group: TestGroup,
}

const TESTS: &[TestCase] = &[
    TestCase { name: "apiserver_serves_resources", group: TestGroup::General },
    TestCase { name: "node_is_ready", group: TestGroup::General },
    TestCase { name: "kubernetes_service_has_reachable_endpoint", group: TestGroup::General },
];

/// Run the selected bootstrap-native checks without re-running installation
/// or re-executing through sudo. This mode is deliberately safe to invoke on
/// an already-running cluster as an ordinary user.
pub fn run(only: Option<&str>, shard: Option<&str>) -> Result<()> {
    select_kubeconfig()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the bootstrap e2e runtime")?;
    runtime.block_on(run_async(only, shard))
}

async fn run_async(only: Option<&str>, shard: Option<&str>) -> Result<()> {
    let selected = select_tests(only, shard)?;
    if selected.is_empty() {
        println!("bootstrap e2e: no tests selected for this shard");
        return Ok(());
    }
    let client = Client::try_default()
        .await
        .context("loading the Kubernetes client for bootstrap e2e; set KUBECONFIG or bootstrap the cluster first")?;

    if let Some(shard) = shard {
        println!("bootstrap e2e: {} test(s), shard {shard}", selected.len());
    } else {
        println!("bootstrap e2e: {} test(s)", selected.len());
    }
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

fn select_tests(only: Option<&str>, shard: Option<&str>) -> Result<Vec<&'static str>> {
    let shard = shard.map(parse_shard).transpose()?;
    let patterns: Vec<&str> = only
        .unwrap_or_default()
        .split(',')
        .filter(|pattern| !pattern.is_empty())
        .collect();

    if let Some(only) = only {
        let matches_any = TESTS.iter().any(|test| patterns.iter().any(|pattern| test.name.contains(pattern)));
        if !matches_any {
            bail!(
                "--only={only} selected no bootstrap e2e tests; available tests: {}",
                test_names().join(", ")
            );
        }
    }

    let mut general_position = 0;
    let mut csi_dra_position = 0;
    let mut selected = Vec::new();
    for test in TESTS {
        let selected_for_shard = match shard {
            None => true,
            Some(shard) => match test.group {
                TestGroup::General => {
                    let selected = general_position % shard.total == shard.index - 1;
                    general_position += 1;
                    selected
                }
                TestGroup::CsiDra => {
                    let selected = shard.index <= CSI_DRA_SHARDS
                        && csi_dra_position % CSI_DRA_SHARDS == shard.index - 1;
                    csi_dra_position += 1;
                    selected
                }
            },
        };
        if selected_for_shard && (only.is_none() || patterns.iter().any(|pattern| test.name.contains(pattern))) {
            selected.push(test.name);
        }
    }
    Ok(selected)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Shard {
    index: usize,
    total: usize,
}

fn parse_shard(value: &str) -> Result<Shard> {
    let (index, total) = value
        .split_once('/')
        .with_context(|| format!("invalid --shard={value}; expected N/5"))?;
    let index = index.parse::<usize>().with_context(|| format!("invalid shard index in --shard={value}"))?;
    let total = total.parse::<usize>().with_context(|| format!("invalid shard total in --shard={value}"))?;
    anyhow::ensure!(total > 0 && index > 0 && index <= total, "invalid --shard={value}; expected 1 <= N <= total");
    anyhow::ensure!(total == 5, "invalid --shard={value}; CI uses exactly five e2e shards");
    Ok(Shard { index, total })
}

fn test_names() -> Vec<&'static str> {
    TESTS.iter().map(|test| test.name).collect()
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
        assert_eq!(select_tests(None, None).unwrap(), test_names());
    }

    #[test]
    fn only_matches_test_name_substrings_and_comma_separates() {
        assert_eq!(
            select_tests(Some("node_is_ready,kubernetes_service"), None).unwrap(),
            vec!["node_is_ready", "kubernetes_service_has_reachable_endpoint"]
        );
    }

    #[test]
    fn an_unknown_only_pattern_is_an_error() {
        assert!(select_tests(Some("does_not_exist"), None).is_err());
    }

    #[test]
    fn general_tests_are_round_robined_across_five_shards() {
        assert_eq!(select_tests(None, Some("1/5")).unwrap(), vec!["apiserver_serves_resources"]);
        assert_eq!(select_tests(None, Some("2/5")).unwrap(), vec!["node_is_ready"]);
        assert_eq!(select_tests(None, Some("3/5")).unwrap(), vec!["kubernetes_service_has_reachable_endpoint"]);
        assert!(select_tests(None, Some("4/5")).unwrap().is_empty());
    }

    #[test]
    fn shard_parser_requires_the_five_way_ci_layout() {
        assert_eq!(parse_shard("2/5").unwrap(), Shard { index: 2, total: 5 });
        assert!(parse_shard("0/5").is_err());
        assert!(parse_shard("1/4").is_err());
    }
}
