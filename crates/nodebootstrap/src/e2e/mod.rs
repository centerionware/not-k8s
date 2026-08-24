//! Bootstrap-native end-to-end checks.
//!
//! The runner owns registration, filtering, and CI shard assignment. Test
//! implementations live under `tests/`, split by the Kubernetes subsystem
//! they exercise so each file remains focused as the shell suite is migrated.

use anyhow::{bail, Context, Result};
use k8s_openapi::api::core::v1::{Endpoints, Namespace, Node};
use kube::api::{Api, ListParams};
use kube::Client;
use std::error::Error;
use std::fmt;
use std::net::IpAddr;
use std::path::Path;
use std::time::Instant;

#[path = "tests/batch.rs"]
mod batch;
#[path = "tests/context.rs"]
mod context;
#[path = "tests/daemonset.rs"]
mod daemonset;
#[path = "tests/disruption.rs"]
mod disruption;
#[path = "tests/deployment.rs"]
mod deployment;
#[path = "tests/ephemeral_containers.rs"]
mod ephemeral_containers;
#[path = "tests/endpoint_slice.rs"]
mod endpoint_slice;
#[path = "tests/garbage_collection.rs"]
mod garbage_collection;
#[path = "tests/generic_ephemeral_volume.rs"]
mod generic_ephemeral_volume;
#[path = "tests/hooks.rs"]
mod hooks;
#[path = "tests/node_status.rs"]
mod node_status;
#[path = "tests/namespace.rs"]
mod namespace;
#[path = "tests/networking.rs"]
mod networking;
#[path = "tests/lifecycle.rs"]
mod lifecycle;
#[path = "tests/pods.rs"]
mod pods;
#[path = "tests/pod_resources.rs"]
mod pod_resources;
#[path = "tests/probes.rs"]
mod probes;
#[path = "tests/readiness_gates.rs"]
mod readiness_gates;
#[path = "tests/replicaset.rs"]
mod replicaset;
#[path = "tests/resource_quota.rs"]
mod resource_quota;
#[path = "tests/resources.rs"]
mod resources;
#[path = "tests/runtime_class.rs"]
mod runtime_class;
#[path = "tests/scheduler.rs"]
mod scheduler;
#[path = "tests/security.rs"]
mod security;
#[path = "tests/service_proxy.rs"]
mod service_proxy;
#[path = "tests/statefulset.rs"]
mod statefulset;
#[path = "tests/storage.rs"]
mod storage;
#[path = "tests/volumes.rs"]
mod volumes;

use context::E2eContext;

const CSI_DRA_SHARDS: usize = 2;

#[derive(Debug)]
struct SkipTest(String);

impl fmt::Display for SkipTest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for SkipTest {}

pub(super) fn skip_test(reason: impl Into<String>) -> anyhow::Error {
    SkipTest(reason.into()).into()
}

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
    TestCase {
        name: "apiserver_serves_resources",
        group: TestGroup::General,
    },
    TestCase {
        name: "node_is_ready",
        group: TestGroup::General,
    },
    TestCase {
        name: "kubernetes_service_has_reachable_endpoint",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_basic_pod_runs",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_init_containers_run_before_app_container",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_native_sidecar_container_starts_before_app_container_and_keeps_running",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_native_sidecar_container_restarts_on_crash",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_init_container_failure_blocks_app_container_under_restart_policy_never",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_crashing_container_restarts_and_increments_restart_count",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_pod_stays_not_ready_until_its_readiness_gate_condition_is_set",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_runtime_class_handler_is_honored",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_disruption_controller_computes_pdb_status",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_kubectl_debug_adds_and_starts_an_ephemeral_container",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_pod_mounts_a_generic_ephemeral_volume",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_node_is_ready_with_capacity_advertised",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_pressure_conditions_are_present_and_normally_false",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_node_reports_a_real_kernel_and_os_image",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_node_status_reports_runtime_handlers",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_node_gets_a_pod_cidr_allocated",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_job_controller_runs_pods_to_completion",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_job_controller_fails_after_backoff_limit",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_cronjob_controller_creates_a_job_on_schedule",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_ttl_after_finished_controller_deletes_expired_jobs",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_daemonset_places_a_pod_directly",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_deployment_creates_replicaset_and_rolls_update",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_replicaset_creates_and_scales_pods",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_statefulset_creates_ordinal_pods_and_scales_down_highest_first",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_statefulset_with_a_volume_claim_template_creates_an_accepted_pod",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_namespace_controller_deletes_contents_before_finalizing",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_resourcequota_used_pods_tracks_actual_pod_count",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_memory_limit_is_enforced_via_cgroup",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_cpu_limit_is_enforced_via_cgroup",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_besteffort_pod_gets_no_cgroup_limit",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_endpointslice_is_produced_for_a_selected_pod",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_garbage_collector_cascades_deployment_delete_to_replicaset_and_pods",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_host_network_pod_uses_the_node_network_namespace",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_host_port_reaches_the_container_on_the_node_ip",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_pod_resources_socket_is_created_on_a_cri_node",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_places_an_ordinary_pod",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_honours_a_matching_node_selector",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_leaves_an_impossible_selector_pending",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_restart_policy_never_exit_zero_is_succeeded",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_restart_policy_never_nonzero_exit_is_failed",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_terminated_container_reports_its_exit_code",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_guaranteed_pod_reports_guaranteed_qos",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_container_status_id_has_runtime_scheme",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_crash_loop_backoff_reports_waiting_reason",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_image_pull_policy_never_fails_when_image_is_absent",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_readiness_probe_gates_ready_condition",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_liveness_probe_failure_restarts_container",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_startup_probe_gates_readiness_until_server_starts",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_wrong_port_readiness_probe_keeps_pod_not_ready",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_configmap_and_secret_volumes_are_materialized",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_downward_api_volume_writes_pod_metadata",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_projected_volume_merges_configmap_and_downward_api",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_service_account_token_projected_volume_mints_a_token",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_host_aliases_are_written_to_etc_hosts",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_read_only_root_filesystem_blocks_writes",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_writable_root_filesystem_allows_writes",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_poststart_hook_runs_before_container_exit",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_termination_message_path_is_read_back_into_status",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_clusterip_service_routes_to_its_backend_pod",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeport_service_is_reachable_on_the_node_ip",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_pv_binder_binds_a_static_pv_and_protection_finalizers_gate_deletion",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_pv_binder_requests_dynamic_provisioning_from_storage_class",
        group: TestGroup::CsiDra,
    },
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
    let client = Client::try_default().await.context(
        "loading the Kubernetes client for bootstrap e2e; set KUBECONFIG or bootstrap the cluster first",
    )?;
    let context = E2eContext::create(client).await?;

    if let Some(shard) = shard {
        println!("bootstrap e2e: {} test(s), shard {shard}", selected.len());
    } else {
        println!("bootstrap e2e: {} test(s)", selected.len());
    }
    let mut failures = Vec::new();
    let mut passed = 0;
    let mut skipped = 0;
    for name in selected {
        let started = Instant::now();
        print!("▶ {name} ... ");
        match run_test(name, &context).await {
            Ok(()) => {
                passed += 1;
                println!("PASS ({}ms)", started.elapsed().as_millis());
            }
            Err(error) => {
                if let Some(skip) = error.downcast_ref::<SkipTest>() {
                    skipped += 1;
                    println!("SKIP ({}; {}ms)", skip, started.elapsed().as_millis());
                } else {
                    println!("FAIL ({}ms)", started.elapsed().as_millis());
                    eprintln!("    {error:#}");
                    failures.push(name);
                }
            }
        }
    }

    context.cleanup().await;
    if failures.is_empty() {
        println!("Results: {passed} passed, {skipped} skipped, 0 failed");
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
        tracing::info!(
            path = %candidate.display(),
            "using nodebootstrap admin kubeconfig for e2e"
        );
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
        let matches_any = TESTS
            .iter()
            .any(|test| patterns.iter().any(|pattern| test.name.contains(pattern)));
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
                    let selected = assigned_to_shard(test.group, general_position, shard);
                    general_position += 1;
                    selected
                }
                TestGroup::CsiDra => {
                    let selected = assigned_to_shard(test.group, csi_dra_position, shard);
                    csi_dra_position += 1;
                    selected
                }
            },
        };
        if selected_for_shard
            && (only.is_none() || patterns.iter().any(|pattern| test.name.contains(pattern)))
        {
            selected.push(test.name);
        }
    }
    Ok(selected)
}

fn assigned_to_shard(group: TestGroup, position: usize, shard: Shard) -> bool {
    match group {
        TestGroup::General => position % shard.total == shard.index - 1,
        TestGroup::CsiDra => {
            shard.index <= CSI_DRA_SHARDS && position % CSI_DRA_SHARDS == shard.index - 1
        }
    }
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
    let index = index
        .parse::<usize>()
        .with_context(|| format!("invalid shard index in --shard={value}"))?;
    let total = total
        .parse::<usize>()
        .with_context(|| format!("invalid shard total in --shard={value}"))?;
    anyhow::ensure!(
        total > 0 && index > 0 && index <= total,
        "invalid --shard={value}; expected 1 <= N <= total"
    );
    anyhow::ensure!(
        total == 5,
        "invalid --shard={value}; CI uses exactly five e2e shards"
    );
    Ok(Shard { index, total })
}

fn test_names() -> Vec<&'static str> {
    TESTS.iter().map(|test| test.name).collect()
}

async fn run_test(name: &str, context: &E2eContext) -> Result<()> {
    match name {
        "apiserver_serves_resources" => apiserver_serves_resources(context.client.clone()).await,
        "node_is_ready" => node_is_ready(context.client.clone()).await,
        "kubernetes_service_has_reachable_endpoint" => {
            kubernetes_service_has_reachable_endpoint(context.client.clone()).await
        }
        "test_basic_pod_runs" => pods::basic_pod_runs(context).await,
        "test_init_containers_run_before_app_container" => {
            pods::init_containers_run_before_app_container(context).await
        }
        "test_native_sidecar_container_starts_before_app_container_and_keeps_running" => {
            pods::native_sidecar_starts_before_app_container(context).await
        }
        "test_native_sidecar_container_restarts_on_crash" => {
            pods::native_sidecar_restarts_on_crash(context).await
        }
        "test_init_container_failure_blocks_app_container_under_restart_policy_never" => {
            pods::init_failure_blocks_app(context).await
        }
        "test_crashing_container_restarts_and_increments_restart_count" => {
            pods::crashing_container_restarts(context).await
        }
        "test_pod_stays_not_ready_until_its_readiness_gate_condition_is_set" => {
            readiness_gates::pod_stays_not_ready_until_its_readiness_gate_condition_is_set(context).await
        }
        "test_runtime_class_handler_is_honored" => {
            runtime_class::runtime_class_handler_is_honored(context).await
        }
        "test_disruption_controller_computes_pdb_status" => {
            disruption::disruption_controller_computes_pdb_status(context).await
        }
        "test_kubectl_debug_adds_and_starts_an_ephemeral_container" => {
            ephemeral_containers::kubectl_debug_adds_and_starts_an_ephemeral_container(context).await
        }
        "test_pod_mounts_a_generic_ephemeral_volume" => {
            generic_ephemeral_volume::pod_mounts_a_generic_ephemeral_volume(context).await
        }
        "test_node_is_ready_with_capacity_advertised" => {
            node_status::node_is_ready_with_capacity_advertised(context).await
        }
        "test_pressure_conditions_are_present_and_normally_false" => {
            node_status::pressure_conditions_are_present(context).await
        }
        "test_node_reports_a_real_kernel_and_os_image" => {
            node_status::node_reports_real_kernel_and_os_image(context).await
        }
        "test_node_status_reports_runtime_handlers" => {
            node_status::node_status_reports_runtime_handlers(context).await
        }
        "test_node_gets_a_pod_cidr_allocated" => node_status::node_gets_a_pod_cidr(context).await,
        "test_job_controller_runs_pods_to_completion" => {
            batch::job_controller_runs_pods_to_completion(context).await
        }
        "test_job_controller_fails_after_backoff_limit" => {
            batch::job_controller_fails_after_backoff_limit(context).await
        }
        "test_cronjob_controller_creates_a_job_on_schedule" => {
            batch::cronjob_controller_creates_a_job_on_schedule(context).await
        }
        "test_ttl_after_finished_controller_deletes_expired_jobs" => {
            batch::ttl_after_finished_controller_deletes_expired_jobs(context).await
        }
        "test_daemonset_places_a_pod_directly" => daemonset::daemonset_places_a_pod_directly(context).await,
        "test_deployment_creates_replicaset_and_rolls_update" => {
            deployment::deployment_creates_replicaset_and_rolls_update(context).await
        }
        "test_replicaset_creates_and_scales_pods" => {
            replicaset::replicaset_creates_and_scales_pods(context).await
        }
        "test_statefulset_creates_ordinal_pods_and_scales_down_highest_first" => {
            statefulset::statefulset_creates_ordinal_pods_and_scales_down_highest_first(context).await
        }
        "test_statefulset_with_a_volume_claim_template_creates_an_accepted_pod" => {
            statefulset::statefulset_with_a_volume_claim_template_creates_an_accepted_pod(context).await
        }
        "test_namespace_controller_deletes_contents_before_finalizing" => {
            namespace::namespace_controller_deletes_contents_before_finalizing(context).await
        }
        "test_resourcequota_used_pods_tracks_actual_pod_count" => {
            resource_quota::resourcequota_used_pods_tracks_actual_pod_count(context).await
        }
        "test_memory_limit_is_enforced_via_cgroup" => {
            resources::memory_limit_is_enforced_via_cgroup(context).await
        }
        "test_cpu_limit_is_enforced_via_cgroup" => {
            resources::cpu_limit_is_enforced_via_cgroup(context).await
        }
        "test_besteffort_pod_gets_no_cgroup_limit" => {
            resources::besteffort_pod_gets_no_cgroup_limit(context).await
        }
        "test_endpointslice_is_produced_for_a_selected_pod" => {
            endpoint_slice::endpointslice_is_produced_for_a_selected_pod(context).await
        }
        "test_garbage_collector_cascades_deployment_delete_to_replicaset_and_pods" => {
            garbage_collection::garbage_collector_cascades_deployment_delete_to_replicaset_and_pods(context).await
        }
        "test_host_network_pod_uses_the_node_network_namespace" => {
            networking::host_network_pod_uses_the_node_network_namespace(context).await
        }
        "test_host_port_reaches_the_container_on_the_node_ip" => {
            networking::host_port_reaches_the_container_on_the_node_ip(context).await
        }
        "test_pod_resources_socket_is_created_on_a_cri_node" => {
            pod_resources::pod_resources_socket_is_created_on_a_cri_node(context).await
        }
        "test_scheduler_places_an_ordinary_pod" => {
            scheduler::scheduler_places_an_ordinary_pod(context).await
        }
        "test_scheduler_honours_a_matching_node_selector" => {
            scheduler::scheduler_honours_a_matching_node_selector(context).await
        }
        "test_scheduler_leaves_an_impossible_selector_pending" => {
            scheduler::scheduler_leaves_an_impossible_selector_pending(context).await
        }
        "test_restart_policy_never_exit_zero_is_succeeded" => {
            lifecycle::restart_policy_never_exit_zero_is_succeeded(context).await
        }
        "test_restart_policy_never_nonzero_exit_is_failed" => {
            lifecycle::restart_policy_never_nonzero_exit_is_failed(context).await
        }
        "test_terminated_container_reports_its_exit_code" => {
            lifecycle::terminated_container_reports_its_exit_code(context).await
        }
        "test_guaranteed_pod_reports_guaranteed_qos" => {
            lifecycle::guaranteed_pod_reports_guaranteed_qos(context).await
        }
        "test_container_status_id_has_runtime_scheme" => {
            lifecycle::container_status_id_has_runtime_scheme(context).await
        }
        "test_crash_loop_backoff_reports_waiting_reason" => {
            lifecycle::crash_loop_backoff_reports_waiting_reason(context).await
        }
        "test_image_pull_policy_never_fails_when_image_is_absent" => {
            lifecycle::image_pull_policy_never_fails_when_image_is_absent(context).await
        }
        "test_readiness_probe_gates_ready_condition" => {
            probes::readiness_probe_gates_ready_condition(context).await
        }
        "test_liveness_probe_failure_restarts_container" => {
            probes::liveness_probe_failure_restarts_container(context).await
        }
        "test_startup_probe_gates_readiness_until_server_starts" => {
            probes::startup_probe_gates_readiness_until_server_starts(context).await
        }
        "test_wrong_port_readiness_probe_keeps_pod_not_ready" => {
            probes::wrong_port_readiness_probe_keeps_pod_not_ready(context).await
        }
        "test_configmap_and_secret_volumes_are_materialized" => {
            volumes::configmap_and_secret_volumes_are_materialized(context).await
        }
        "test_downward_api_volume_writes_pod_metadata" => {
            volumes::downward_api_volume_writes_pod_metadata(context).await
        }
        "test_projected_volume_merges_configmap_and_downward_api" => {
            volumes::projected_volume_merges_configmap_and_downward_api(context).await
        }
        "test_service_account_token_projected_volume_mints_a_token" => {
            volumes::service_account_token_projected_volume_mints_a_token(context).await
        }
        "test_host_aliases_are_written_to_etc_hosts" => {
            volumes::host_aliases_are_written_to_etc_hosts(context).await
        }
        "test_read_only_root_filesystem_blocks_writes" => {
            security::read_only_root_filesystem_blocks_writes(context).await
        }
        "test_writable_root_filesystem_allows_writes" => {
            security::writable_root_filesystem_allows_writes(context).await
        }
        "test_poststart_hook_runs_before_container_exit" => {
            hooks::poststart_hook_runs_before_container_exit(context).await
        }
        "test_termination_message_path_is_read_back_into_status" => {
            hooks::termination_message_path_is_read_back_into_status(context).await
        }
        "test_clusterip_service_routes_to_its_backend_pod" => {
            service_proxy::clusterip_service_routes_to_its_backend_pod(context).await
        }
        "test_nodeport_service_is_reachable_on_the_node_ip" => {
            service_proxy::nodeport_service_is_reachable_on_the_node_ip(context).await
        }
        "test_pv_binder_binds_a_static_pv_and_protection_finalizers_gate_deletion" => {
            storage::pv_binder_binds_a_static_pv_and_protection_finalizers_gate_deletion(context).await
        }
        "test_pv_binder_requests_dynamic_provisioning_from_storage_class" => {
            storage::pv_binder_requests_dynamic_provisioning_from_storage_class(context).await
        }
        other => bail!("unknown bootstrap e2e test {other}"),
    }
}

async fn apiserver_serves_resources(client: Client) -> Result<()> {
    let api: Api<Namespace> = Api::all(client);
    let namespaces = api.list(&ListParams::default()).await.context("listing namespaces")?;
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
            .is_some_and(|conditions| {
                conditions
                    .iter()
                    .any(|condition| condition.type_ == "Ready" && condition.status == "True")
            })
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

    let reachable = addresses
        .iter()
        .filter_map(|address| address.parse::<IpAddr>().ok())
        .any(|ip| !ip.is_loopback() && !ip.is_unspecified());
    anyhow::ensure!(
        reachable,
        "default/kubernetes has no non-loopback, non-unspecified endpoint (addresses: {addresses:?})"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_filter_selects_all_registered_bootstrap_checks() {
        assert_eq!(select_tests(None, None).unwrap(), test_names());
    }

    #[test]
    fn only_matches_test_name_substrings_and_comma_separates() {
        assert_eq!(
            select_tests(Some("node_is_ready,kubernetes_service"), None).unwrap(),
            vec![
                "node_is_ready",
                "kubernetes_service_has_reachable_endpoint",
                "test_node_is_ready_with_capacity_advertised"
            ]
        );
    }

    #[test]
    fn an_unknown_only_pattern_is_an_error() {
        assert!(select_tests(Some("does_not_exist"), None).is_err());
    }

    #[test]
    fn general_tests_are_round_robined_across_five_shards() {
        let shards: Vec<_> = (1..=5)
            .map(|index| {
                let shard = format!("{index}/5");
                select_tests(None, Some(&shard)).unwrap()
            })
            .collect();
        let general_names: Vec<_> = TESTS
            .iter()
            .filter(|test| test.group == TestGroup::General)
            .map(|test| test.name)
            .collect();
        let selected_count: usize = shards
            .iter()
            .flatten()
            .filter(|name| general_names.contains(name))
            .count();
        assert_eq!(
            selected_count,
            TESTS
                .iter()
                .filter(|test| test.group == TestGroup::General)
                .count()
        );
        assert!(
            shards
                .iter()
                .all(|shard| shard.windows(2).all(|pair| pair[0] != pair[1]))
        );
    }

    #[test]
    fn shard_parser_requires_the_five_way_ci_layout() {
        assert_eq!(
            parse_shard("2/5").unwrap(),
            Shard { index: 2, total: 5 }
        );
        assert!(parse_shard("0/5").is_err());
        assert!(parse_shard("1/4").is_err());
    }

    #[test]
    fn csi_and_dra_tests_only_use_the_first_two_shards() {
        let shard_one = Shard { index: 1, total: 5 };
        let shard_two = Shard { index: 2, total: 5 };
        let shard_three = Shard { index: 3, total: 5 };
        assert!(assigned_to_shard(TestGroup::CsiDra, 0, shard_one));
        assert!(assigned_to_shard(TestGroup::CsiDra, 1, shard_two));
        assert!(!assigned_to_shard(TestGroup::CsiDra, 0, shard_three));
        assert!(!assigned_to_shard(TestGroup::CsiDra, 1, shard_three));
    }
}
