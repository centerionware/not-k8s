//! Bootstrap-native end-to-end checks.
//!
//! The runner owns registration, filtering, and CI shard assignment. Test
//! implementations live under `tests/`, split by the Kubernetes subsystem
//! they exercise so each file remains focused as the shell suite is migrated.

use anyhow::{bail, Context, Result};
use k8s_openapi::api::core::v1::{Endpoints, Namespace, Node};
use kube::api::{Api, ListParams};
use kube::{Client, Config as KubeConfig};
use std::error::Error;
use std::fmt;
use std::io::Write;
use std::net::IpAddr;
use std::path::Path;
use std::time::{Duration, Instant};

#[path = "tests/batch.rs"]
mod batch;
#[path = "tests/bootstrap.rs"]
mod bootstrap;
#[path = "tests/build_layout.rs"]
mod build_layout;
#[path = "tests/context.rs"]
mod context;
#[path = "tests/cgroup.rs"]
mod cgroup;
#[path = "tests/cert_manager.rs"]
mod cert_manager;
#[path = "tests/component_rbac.rs"]
mod component_rbac;
#[path = "tests/config_file.rs"]
mod config_file;
#[path = "tests/credential_provider.rs"]
mod credential_provider;
#[path = "tests/controller_manager.rs"]
mod controller_manager;
#[path = "tests/csi.rs"]
mod csi;
#[path = "tests/daemonset.rs"]
mod daemonset;
#[path = "tests/disruption.rs"]
mod disruption;
#[path = "tests/deployment.rs"]
mod deployment;
#[path = "tests/datastore.rs"]
mod datastore;
#[path = "tests/datastore_cluster.rs"]
mod datastore_cluster;
#[path = "tests/datastore_apiserver.rs"]
mod datastore_apiserver;
#[path = "tests/device_plugins.rs"]
mod device_plugins;
#[path = "tests/dra.rs"]
mod dra;
#[path = "tests/ephemeral_containers.rs"]
mod ephemeral_containers;
#[path = "tests/endpoint_slice.rs"]
mod endpoint_slice;
#[path = "tests/eviction.rs"]
mod eviction;
#[path = "tests/garbage_collection.rs"]
mod garbage_collection;
#[path = "tests/generic_ephemeral_volume.rs"]
mod generic_ephemeral_volume;
#[path = "tests/hooks.rs"]
mod hooks;
#[path = "tests/host_recovery.rs"]
mod host_recovery;
#[path = "tests/node_status.rs"]
mod node_status;
#[path = "tests/namespace.rs"]
mod namespace;
#[path = "tests/networking.rs"]
mod networking;
#[path = "tests/lifecycle.rs"]
mod lifecycle;
#[path = "tests/metrics.rs"]
mod metrics;
#[path = "tests/pods.rs"]
mod pods;
#[path = "tests/process.rs"]
mod process;
#[path = "tests/pod_resources.rs"]
mod pod_resources;
#[path = "tests/pod_status.rs"]
mod pod_status;
#[path = "tests/probes.rs"]
mod probes;
#[path = "tests/readiness_gates.rs"]
mod readiness_gates;
#[path = "tests/replicaset.rs"]
mod replicaset;
#[path = "tests/resource_quota.rs"]
mod resource_quota;
#[path = "tests/resource_managers.rs"]
mod resource_managers;
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
#[path = "tests/static_pods.rs"]
mod static_pods;
#[path = "tests/storage.rs"]
mod storage;
#[path = "tests/streaming.rs"]
mod streaming;
#[path = "tests/termination.rs"]
mod termination;
#[path = "tests/topology.rs"]
mod topology;
#[path = "tests/volumes.rs"]
mod volumes;
#[path = "tests/watch_recovery.rs"]
mod watch_recovery;

pub(super) mod grpc {
    #[allow(dead_code)]
    pub mod authpb {
        tonic::include_proto!("authpb");
    }
    pub mod mvccpb {
        tonic::include_proto!("mvccpb");
    }
    #[allow(dead_code)]
    pub mod etcdserverpb {
        tonic::include_proto!("etcdserverpb");
    }
    pub mod peer {
        tonic::include_proto!("notk8s.nodestore.peer.v1");
    }
    pub mod podresources {
        tonic::include_proto!("v1");
    }
}

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
        name: "external_cni_mode_disables_flannel",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_bootstrap_persists_installation_flags",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodelet_service_has_cluster_dns_configured",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_configured_service_cidrs_are_used",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_coredns_is_a_healthy_deployment",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_target_is_serving",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_enforces_node_restriction",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_applies_core_defaults",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_rejects_invalid_builtin_schema_constraints",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_rejects_invalid_metadata_keys",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_rejects_invalid_batch_names",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_rejects_invalid_workload_names",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_rejects_privileged_csr_subject",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_applies_pure_admission_to_apply",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_honors_always_pull_images",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_applies_storage_admission_to_apply",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_adds_extended_resource_tolerations",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_defaults_ingress_class",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_defaults_storage_class_on_apply",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_adds_storage_protection_finalizer",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_applies_runtime_class_admission",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_applies_priority_admission",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_rejects_unsupported_pvc_resize",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_applies_namespace_node_selector",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_serializes_resource_quota_creates",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_taints_new_nodes_not_ready",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_binds_a_pod_through_binding_subresource",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_advertises_subresources",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_serves_workload_scale_subresource",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_reconciles_managed_fields_across_versions",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_reconciles_crd_managed_fields_after_schema_change",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_authentication_modes",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_apf_labels_requests",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_exposes_inflight_metrics",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_exposes_full_request_metrics",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_honors_patch_dry_run",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_authorizes_before_special_handlers",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_writes_audit_log",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_audits_rejected_requests",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_rotates_audit_log",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_delivers_audit_webhook",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_audits_request_and_response_objects",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_rejects_unsupported_field_selector",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_serves_generic_status_subresource",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_excludes_status_from_main_managed_fields",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_serves_ephemeralcontainers_subresource",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_enforces_service_account_mountable_secrets",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_enforces_mountable_secrets_for_ephemeral_containers",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_watches_an_uncommon_builtin_resource",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_honors_watch_options",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_recreates_a_dynamic_watch_cache",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_watches_partial_object_metadata",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_rejects_unsupported_resource_route",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_rejects_oversized_request_body",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_validating_admission_policy_denies_create",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_enforces_crd_schema_constraints",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_mutating_admission_policy_mutates_create",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_validates_crd_status_subresource",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_honors_webhook_match_conditions",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_honors_webhook_side_effects_on_dry_run",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_runs_webhook_for_delete_collection",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_honors_finalizers",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_honors_authorization_webhook_decisions",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_honors_resource_version_snapshot",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_serves_partial_object_metadata",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_honors_generate_name",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_honors_dry_run_and_delete_preconditions",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_graceful_node_shutdown_manual_note",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_tls_bootstrap_issues_a_real_client_certificate",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_combined_binary_contains_every_component",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_combined_binary_rejects_an_unknown_component",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_installed_component_binaries_are_runnable_whatever_the_layout",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_a_failing_component_says_why_before_it_exits",
        group: TestGroup::General,
    },
    TestCase {
        name: "apiserver_serves_resources",
        group: TestGroup::General,
    },
    TestCase {
        name: "node_is_ready",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_kubernetes_service_has_a_reachable_endpoint",
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
        name: "test_crash_loop_backoff_throttles_immediate_restarts",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_pod_with_a_finalizer_tears_down_but_stays_until_the_finalizer_is_removed",
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
        name: "test_pod_exceeding_its_own_ephemeral_storage_limit_is_evicted",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_pod_exceeding_an_empty_dir_size_limit_is_evicted",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_eviction_manual_procedure",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_eviction_priority_tiebreak_manual_procedure",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_eviction_exceeds_requests_tiebreak_manual_procedure",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_eviction_soft_grace_period_manual_procedure",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_node_is_ready_with_capacity_advertised",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_node_reports_hugepages_capacity_when_reserved",
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
        name: "test_node_status_images_reflects_a_real_pulled_image",
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
        name: "test_cert_manager_crds_are_usable_without_nodecontroller_restart",
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
        name: "test_no_swap_default_disables_swap_via_cgroup",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_limited_swap_gives_burstable_pods_proportional_swap",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_hugepages_limit_is_enforced_via_cgroup",
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
        name: "test_besteffort_pod_gets_the_certain_death_oom_score",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_guaranteed_pod_gets_the_protected_oom_score",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_env_resource_field_ref_reports_the_containers_own_limits",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_in_place_resize_updates_memory_limit_without_restarting",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_cpu_manager_pins_guaranteed_containers_to_disjoint_exclusive_cores",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_cpu_manager_retroactively_shrinks_an_already_running_shared_pool_container",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_memory_manager_pins_guaranteed_containers_to_a_numa_node",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_log_rotation_creates_a_rotated_file",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_static_pod_creates_a_mirror_pod",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_datastore_serves_the_etcd_status_rpc",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_datastore_round_trips_a_key_over_grpc",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_datastore_lists_a_prefix_in_key_order",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_datastore_enforces_compare_and_swap",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_datastore_creates_a_key_only_if_absent",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_datastore_streams_watch_events_as_they_happen",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_datastore_replays_missed_events_to_a_late_watcher",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_datastore_refuses_a_read_below_the_compaction_point",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_datastore_expires_a_lease_and_its_keys",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_datastore_survives_a_restart_with_its_data",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_upgrade_a_populated_single_member_into_a_one_member_cluster",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_upgrade_straight_to_a_multi_member_cluster_is_refused",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_upgrade_a_dead_member_leaves_nothing_listening_behind_it",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_datastore_refuses_a_cluster_it_cannot_be_part_of",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_datastore_refuses_a_malformed_cluster_spec",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_cluster_elects_a_single_leader",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_cluster_replicates_a_write_to_every_member",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_a_follower_forwards_writes_to_the_leader",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_the_cluster_keeps_serving_when_a_follower_dies",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_the_cluster_survives_the_leader_being_killed",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_a_minority_refuses_writes_rather_than_inventing_them",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_a_restarted_member_catches_up_on_what_it_missed",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_the_cluster_tolerates_a_slow_link",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_a_partitioned_leader_steps_down_and_the_majority_elects_another",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_a_real_apiserver_starts_and_serves_against_nodestore",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_apiserver_crud_round_trips_through_nodestore",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_apiserver_watch_delivers_through_nodestore",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_apiserver_state_survives_a_datastore_restart",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_a_pending_pod_recovers_after_the_node_failure_is_fixed",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_existing_pod_recreates_its_container_after_a_runtime_restart",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_endpointslice_is_produced_for_a_selected_pod",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_replacement_control_plane_identities_can_read_all_watch_inputs",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_config_file_sets_a_value_env_did_not_override",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_config_file_precedence_a_real_env_var_still_wins",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_config_dir_merges_files_in_filename_order",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_holds_the_leader_lease",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_wakes_a_pending_pod_on_a_real_event",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_node_allocatable_cgroup_exists_and_is_capped",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_pod_cgroup_reflects_its_qos_class",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_garbage_collector_cascades_deployment_delete_to_replicaset_and_pods",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_pod_teardown_actually_removes_the_sandbox",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_orphaned_sandbox_gc_reaps_a_pod_deleted_while_nodelet_is_down",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_unreferenced_image_is_not_removed_below_the_watermark",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_image_gc_removes_unreferenced_images_above_the_watermark",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_allocated_resources_status_absent_without_device_resources",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_device_plugin_advertises_capacity_and_allocates_into_a_container",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_device_plugin_health_transition_updates_allocated_resources_status",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_device_plugin_preferred_allocation_and_prestart",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_host_network_pod_uses_the_node_network_namespace",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_host_port_publishes_the_container_on_the_nodes_own_ip",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_host_network_pod_needs_no_explicit_port_mapping",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_custom_dns_config_reaches_resolv_conf",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_enable_service_links_false_preserves_kubernetes_env",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_cluster_dns_resolves_service_names",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_spec_hostname_overrides_the_container_hostname",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_set_hostname_as_fqdn_reports_the_full_fqdn",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_pod_resources_socket_is_created_on_a_cri_node",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_pod_resources_grpc_query_returns_real_data",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_plugin_registry_directory_exists",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_plugin_registry_watches_for_device_plugins_too",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_plugin_registry_watches_for_dra_drivers_too",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_dynamic_csi_registration_actually_registered_the_driver",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_resource_api_group_is_enabled",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_credential_provider_config_unset_by_default",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_credential_provider_supplies_auth_for_an_otherwise_rejected_pull",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_dra_claim_is_allocated_and_reserved_for_the_pod",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_csi_ephemeral_inline_volume_is_mounted",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_pod_uses_a_raw_block_volume",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_node_reports_volumes_in_use_for_a_csi_volume",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_fsgroup_change_policy_on_root_mismatch_skips_the_second_chown",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_pod_with_an_attach_required_pvc_waits_for_volumeattachment",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_scheduler_places_an_ordinary_pod",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_consults_an_http_extender_and_honours_a_filter_rejection",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_schedules_a_pod_an_http_extender_approves",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_honours_a_node_selector_that_matches",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_rejects_a_pod_that_does_not_fit",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_preempts_a_lower_priority_pod",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_does_not_preempt_when_policy_forbids_it",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_honours_a_node_selector_that_matches_nothing",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_leaves_a_gated_pod_alone",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_ignores_pods_for_another_scheduler",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_honours_pod_affinity",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_honours_pod_anti_affinity",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_honours_topology_spread",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_respects_a_taint_and_its_toleration",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_resolves_a_namespace_selector_against_real_labels",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_schedules_pods_that_get_default_spread_constraints",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_scheduler_delays_binding_a_wait_for_first_consumer_pvc_until_a_node_is_chosen",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_scheduler_claims_a_static_wait_for_first_consumer_volume",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_scheduler_enforces_read_write_once_pod_exclusivity",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_restart_policy_never_exit_zero_is_succeeded",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_restart_policy_never_exit_nonzero_is_failed",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_exited_container_reports_terminated_state_with_exit_code",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_guaranteed_pod_reports_guaranteed_qos",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_pod_status_reports_qos_class",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_pod_exceeding_its_active_deadline_is_terminated",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_container_status_container_id_has_a_runtime_scheme_prefix",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_crash_loop_backoff_reports_waiting_reason",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_crash_loop_backoff_reports_waiting_reason_and_last_state",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_image_pull_policy_never_fails_when_image_is_absent",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_image_pull_policy_if_not_present_skips_the_registry_round_trip",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_pod_status_reports_host_ips_plural",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_pod_condition_reports_observed_generation",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_container_status_reports_a_real_image_id",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_readiness_probe_gates_ready_condition",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_liveness_probe_failure_restarts_the_container",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_liveness_probes_own_grace_period_overrides_the_pods",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_startup_probe_gates_readiness_until_server_starts",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_startup_probe_gates_liveness_and_readiness",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_startup_probe_failure_past_threshold_restarts_the_container",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_http_get_readiness_probe_against_a_real_server",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_wrong_port_readiness_probe_keeps_pod_not_ready",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_grpc_probe_gates_readiness_against_a_real_grpc_server",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_grpc_probe_leaves_pod_not_ready_against_the_wrong_port",
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
        name: "test_service_account_token_projected_volume_mints_a_real_token",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_host_aliases_are_written_to_etc_hosts",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_host_aliases_still_work_under_host_users_false",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_empty_dir_medium_memory_is_backed_by_tmpfs",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_empty_dir_medium_hugepages_is_backed_by_hugetlbfs",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_image_volume_source_mounts_a_read_only_image",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_configmap_volume_updates_live_without_pod_restart",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_host_path_directory_mounts_the_real_host_directory",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_host_path_directory_or_create_creates_a_missing_directory",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_mount_propagation_host_to_container_still_mounts_normally",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_recursive_read_only_still_mounts_read_only_normally",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_mount_propagation_host_to_container_sees_a_new_host_mount",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_mount_propagation_private_default_does_not_see_a_new_host_mount",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_recursive_read_only_enabled_blocks_writes_in_a_nested_mount_too",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_recursive_read_only_if_possible_falls_back_without_erroring",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_recursive_read_only_if_possible_tracks_the_runtime_handlers_own_capability",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_sub_path_expr_expands_a_downward_api_env_var",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_host_path_directory_type_rejects_a_nonexistent_path",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_fsgroup_never_applies_to_hostpath_volumes",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_fsgroup_chowns_materialized_volumes",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_read_only_root_filesystem_is_enforced",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_without_read_only_root_filesystem_writes_succeed",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_run_as_user_is_applied",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_container_status_reports_resolved_user",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_container_status_reports_recursive_read_only",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_host_users_false_gets_a_real_user_namespace",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_host_users_false_volume_still_reads_and_writes_normally",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_host_users_volume_ownership_translation_is_correct",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_supplemental_groups_policy_strict_ignores_image_group_membership",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_client_certificate_authentication_works",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_topology_manager_does_not_reject_pods_on_a_single_numa_node_host",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_topology_manager_restricted_does_not_reject_pods_on_a_single_numa_node_host",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_topology_manager_cross_provider_alignment_manual_note",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_topology_manager_restricted_spread_manual_note",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_sysctls_are_applied_to_the_sandbox",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_proc_mount_default_masks_proc_kcore",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_proc_mount_unmasked_leaves_proc_kcore_readable",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_containers_get_isolated_pid_namespaces_by_default",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_share_process_namespace_puts_every_container_in_one_pid_namespace",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_host_pid_sees_host_processes",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_poststart_hook_runs_after_container_start",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_termination_message_path_is_read_back_into_container_status",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_lifecycle_stop_signal_is_honored_by_the_runtime",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_prestop_hook_runs_before_termination",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_termination_grace_period_is_honored_not_instant",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_clusterip_service_routes_to_its_backend_pod",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeapiserver_proxy_subresources_relay_requests",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeport_service_is_reachable_on_the_node_ip",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_service_with_no_endpoints_does_not_wedge_the_ruleset",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_headless_service_programs_no_rules_and_does_not_break_others",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_clusterip_is_reachable_from_inside_a_pod",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeproxy_runs_as_its_own_service_separate_from_nodelet",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_a_pod_reaching_its_own_service_gets_hairpin_masquerade",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_multiple_backends_actually_share_traffic",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_losing_every_backend_removes_the_dnat_rule",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_deleting_a_service_removes_its_rules",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_session_affinity_pins_a_client_to_one_backend",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_nodeproxy_rebuilds_the_whole_ruleset_after_a_restart",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_a_long_lived_watch_survives_a_service_churn_burst",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_pv_binder_binds_a_static_pv_and_protection_finalizers_gate_deletion",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_pv_binder_requests_dynamic_provisioning_from_the_storage_class",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_pod_mounts_a_persistent_volume_claim",
        group: TestGroup::CsiDra,
    },
    TestCase {
        name: "test_kubectl_logs_returns_real_output",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_kubectl_logs_follow_streams_new_output",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_kubectl_exec_runs_a_command_and_returns_its_output",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_kubectl_attach_streams_the_containers_stdout",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_kubectl_port_forward_reaches_a_real_container_port",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_stats_summary_returns_real_pod_usage",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_metrics_resource_returns_real_pod_usage",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_metrics_cadvisor_returns_real_container_usage",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_a_slow_terminating_pod_does_not_stall_another_pods_creation",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_a_recreated_pod_survives_the_old_pods_detached_teardown",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_the_node_still_reconciles_pods_after_an_apiserver_restart",
        group: TestGroup::General,
    },
    TestCase {
        name: "test_node_is_tainted_unreachable_after_heartbeat_loss_and_recovers",
        group: TestGroup::General,
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
    let mut kube_config = KubeConfig::infer().await.context(
        "loading the Kubernetes client for bootstrap e2e; set KUBECONFIG or bootstrap the cluster first",
    )?;
    // kubectl's --request-timeout=30s equivalent for ordinary kube-rs
    // requests.  Without this, a broken apiserver/nodelet connection can
    // leave one polling predicate stuck forever, so CI never reaches the
    // component diagnostics or publishes the real failing test.
    kube_config.read_timeout = Some(context::API_REQUEST_TIMEOUT);
    kube_config.write_timeout = Some(context::API_REQUEST_TIMEOUT);
    let client = Client::try_from(kube_config).context(
        "building the Kubernetes client for bootstrap e2e; set KUBECONFIG or bootstrap the cluster first",
    )?;
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
        let _ = std::io::stdout().flush();
        // The shell harness gave every test a fresh namespace and removed it
        // before the next test started. Reusing one namespace here left every
        // successful test's Pods, Services, and controller-owned children in
        // the shard until the very end. On a single-node runner that consumed
        // the node's pod budget and made later failures look unrelated (for
        // example, a ClusterIP or CSI timeout reported as "Too many pods").
        // Clean each namespace before continuing. The workflow's next
        // diagnostic phase collects service state and logs; retaining a
        // failed namespace would let one failure poison later tests.
        let test_context = match E2eContext::create(client.clone()).await {
            Ok(context) => context,
            Err(error) => {
                println!("FAIL ({}ms)", started.elapsed().as_millis());
                eprintln!("    creating the per-test namespace failed: {error:#}");
                failures.push(name);
                continue;
            }
        };
        let result = tokio::time::timeout(
            Duration::from_secs(300),
            run_test(name, &test_context),
        )
        .await;
        match result {
            Ok(Ok(())) => {
                test_context.cleanup().await;
                passed += 1;
                println!("PASS ({}ms)", started.elapsed().as_millis());
            }
            Ok(Err(error)) => {
                if let Some(skip) = error.downcast_ref::<SkipTest>() {
                    test_context.cleanup().await;
                    skipped += 1;
                    println!("SKIP ({}; {}ms)", skip, started.elapsed().as_millis());
                } else {
                    // A failed test must not poison later tests with its
                    // Pods, Services, PVCs, or controller-owned children.
                    // The following workflow diagnostic phase collects
                    // service state and logs, so retaining the namespace is
                    // not worth allowing a failure cascade on a one-node
                    // runner.
                    test_context.cleanup().await;
                    println!("FAIL ({}ms)", started.elapsed().as_millis());
                    eprintln!("    {error:#}");
                    failures.push(name);
                }
            }
            Err(_) => {
                test_context.cleanup().await;
                println!("FAIL ({}ms)", started.elapsed().as_millis());
                eprintln!(
                    "    test exceeded the 300-second safety timeout; the next test will run after cleanup"
                );
                failures.push(name);
            }
        }
    }

    if failures.is_empty() {
        println!("Results: {passed} passed, {skipped} skipped, 0 failed");
        Ok(())
    } else {
        // Leave the test namespace and its status/events in place for the
        // workflow's next-phase diagnostics step. A failed run is disposable
        // CI state, and removing it here would erase the evidence we need.
        bail!(
            "bootstrap e2e failed: {} test(s): {}",
            failures.len(),
            failures.join(", ")
        )
    }
}

/// Print the tests selected by the same shard/filter logic as `run`, without
/// loading a kubeconfig or contacting a cluster. CI uses this to omit empty
/// matrix entries before provisioning a runner.
pub fn list(only: Option<&str>, shard: Option<&str>) -> Result<()> {
    for name in select_tests(only, shard)? {
        println!("{name}");
    }
    Ok(())
}

/// Report whether the selected checks require the reference CSI/DRA drivers.
/// This intentionally reuses the exact same filter and shard assignment as
/// `--e2e-list`, so CI does not have to maintain a second test classification.
pub fn needs_drivers(only: Option<&str>, shard: Option<&str>) -> Result<()> {
    println!("{}", selected_tests_need_drivers(only, shard)?);
    Ok(())
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
    // The shell harness kept tests that restart a host service or rewrite a
    // systemd environment override at the end of the run. Preserve the
    // existing shard assignment, but apply the same stable partition inside
    // each shard so an ordinary test never starts while one of these fixtures
    // is still restoring the node.
    Ok(reorder_environment_reconfiguring_tests(selected))
}

fn selected_tests_need_drivers(only: Option<&str>, shard: Option<&str>) -> Result<bool> {
    let selected = select_tests(only, shard)?;
    Ok(selected.iter().any(|name| {
        TESTS
            .iter()
            .any(|test| test.name == *name && test.group == TestGroup::CsiDra)
    }))
}

fn reorder_environment_reconfiguring_tests(selected: Vec<&'static str>) -> Vec<&'static str> {
    let (ordinary, disruptive): (Vec<_>, Vec<_>) = selected
        .into_iter()
        .partition(|name| !is_environment_reconfiguring_test(name));
    ordinary.into_iter().chain(disruptive).collect()
}

fn is_environment_reconfiguring_test(name: &str) -> bool {
    matches!(
        name,
        "test_pending_pod_recovers_after_the_node_failure_is_fixed"
            | "test_config_file_sets_a_value_env_did_not_override"
            | "test_config_file_precedence_a_real_env_var_still_wins"
            | "test_config_dir_merges_files_in_filename_order"
            | "test_limited_swap_gives_burstable_pods_proportional_swap"
            | "test_cpu_manager_pins_guaranteed_containers_to_disjoint_exclusive_cores"
            | "test_cpu_manager_retroactively_shrinks_an_already_running_shared_pool_container"
            | "test_memory_manager_pins_guaranteed_containers_to_a_numa_node"
            | "test_log_rotation_creates_a_rotated_file"
            | "test_static_pod_creates_a_mirror_pod"
            | "test_orphaned_sandbox_gc_reaps_a_pod_deleted_while_nodelet_is_down"
            | "test_unreferenced_image_is_not_removed_below_the_watermark"
            | "test_image_gc_removes_unreferenced_images_above_the_watermark"
            | "test_credential_provider_supplies_auth_for_an_otherwise_rejected_pull"
            | "test_scheduler_consults_an_http_extender_and_honours_a_filter_rejection"
            | "test_scheduler_schedules_a_pod_an_http_extender_approves"
            | "test_nodeproxy_rebuilds_the_whole_ruleset_after_a_restart"
            | "test_nodeapiserver_authentication_modes"
            | "test_nodeapiserver_writes_audit_log"
            | "test_nodeapiserver_audits_rejected_requests"
            | "test_nodeapiserver_rotates_audit_log"
            | "test_nodeapiserver_delivers_audit_webhook"
            | "test_nodeapiserver_audits_request_and_response_objects"
            | "test_nodeapiserver_honors_always_pull_images"
            | "test_client_certificate_authentication_works"
            | "test_topology_manager_does_not_reject_pods_on_a_single_numa_node_host"
            | "test_topology_manager_restricted_does_not_reject_pods_on_a_single_numa_node_host"
            | "test_the_node_still_reconciles_pods_after_an_apiserver_restart"
            | "test_node_is_tainted_unreachable_after_heartbeat_loss_and_recovers"
            | "test_cert_manager_crds_are_usable_without_nodecontroller_restart"
    )
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
        "external_cni_mode_disables_flannel" => bootstrap::external_cni_mode_disables_flannel(context).await,
        "test_bootstrap_persists_installation_flags" => bootstrap::bootstrap_persists_installation_flags(context).await,
        "test_nodelet_service_has_cluster_dns_configured" => {
            bootstrap::nodelet_service_has_cluster_dns_configured(context).await
        }
        "test_configured_service_cidrs_are_used" => {
            bootstrap::configured_service_cidrs_are_used(context).await
        }
        "test_coredns_is_a_healthy_deployment" => bootstrap::coredns_is_a_healthy_deployment(context).await,
        "test_nodeapiserver_target_is_serving" => bootstrap::nodeapiserver_target_is_serving(context).await,
        "test_nodeapiserver_enforces_node_restriction" => {
            bootstrap::nodeapiserver_enforces_node_restriction(context).await
        },
        "test_nodeapiserver_applies_core_defaults" => bootstrap::nodeapiserver_applies_core_defaults(context).await,
        "test_nodeapiserver_rejects_invalid_builtin_schema_constraints" => {
            bootstrap::nodeapiserver_rejects_invalid_builtin_schema_constraints(context).await
        },
        "test_nodeapiserver_rejects_invalid_metadata_keys" => {
            bootstrap::nodeapiserver_rejects_invalid_metadata_keys(context).await
        },
        "test_nodeapiserver_rejects_invalid_batch_names" => {
            bootstrap::nodeapiserver_rejects_invalid_batch_names(context).await
        },
        "test_nodeapiserver_rejects_invalid_workload_names" => {
            bootstrap::nodeapiserver_rejects_invalid_workload_names(context).await
        },
        "test_nodeapiserver_rejects_privileged_csr_subject" => {
            bootstrap::nodeapiserver_rejects_privileged_csr_subject(context).await
        },
        "test_nodeapiserver_applies_pure_admission_to_apply" => {
            bootstrap::nodeapiserver_applies_pure_admission_to_apply(context).await
        },
        "test_nodeapiserver_honors_always_pull_images" => {
            bootstrap::nodeapiserver_honors_always_pull_images(context).await
        },
        "test_nodeapiserver_applies_storage_admission_to_apply" => {
            bootstrap::nodeapiserver_applies_storage_admission_to_apply(context).await
        },
        "test_nodeapiserver_adds_extended_resource_tolerations" => {
            bootstrap::nodeapiserver_adds_extended_resource_tolerations(context).await
        },
        "test_nodeapiserver_defaults_ingress_class" => bootstrap::nodeapiserver_defaults_ingress_class(context).await,
        "test_nodeapiserver_defaults_storage_class_on_apply" => {
            bootstrap::nodeapiserver_defaults_storage_class_on_apply(context).await
        },
        "test_nodeapiserver_adds_storage_protection_finalizer" => {
            bootstrap::nodeapiserver_adds_storage_protection_finalizer(context).await
        },
        "test_nodeapiserver_applies_runtime_class_admission" => {
            bootstrap::nodeapiserver_applies_runtime_class_admission(context).await
        },
        "test_nodeapiserver_applies_priority_admission" => {
            bootstrap::nodeapiserver_applies_priority_admission(context).await
        },
        "test_nodeapiserver_rejects_unsupported_pvc_resize" => {
            bootstrap::nodeapiserver_rejects_unsupported_pvc_resize(context).await
        },
        "test_nodeapiserver_applies_namespace_node_selector" => {
            bootstrap::nodeapiserver_applies_namespace_node_selector(context).await
        },
        "test_nodeapiserver_serializes_resource_quota_creates" => {
            bootstrap::nodeapiserver_serializes_resource_quota_creates(context).await
        },
        "test_nodeapiserver_taints_new_nodes_not_ready" => {
            bootstrap::nodeapiserver_taints_new_nodes_not_ready(context).await
        },
        "test_nodeapiserver_binds_a_pod_through_binding_subresource" => {
            bootstrap::nodeapiserver_binds_a_pod_through_binding_subresource(context).await
        }
        "test_nodeapiserver_advertises_subresources" => {
            bootstrap::nodeapiserver_advertises_subresources(context).await
        }
        "test_nodeapiserver_serves_workload_scale_subresource" => {
            bootstrap::nodeapiserver_serves_workload_scale_subresource(context).await
        }
        "test_nodeapiserver_reconciles_managed_fields_across_versions" => {
            bootstrap::nodeapiserver_reconciles_managed_fields_across_versions(context).await
        }
        "test_nodeapiserver_reconciles_crd_managed_fields_after_schema_change" => {
            bootstrap::nodeapiserver_reconciles_crd_managed_fields_after_schema_change(context).await
        }
        "test_nodeapiserver_authentication_modes" => {
            bootstrap::nodeapiserver_authentication_modes(context).await
        }
        "test_nodeapiserver_apf_labels_requests" => {
            bootstrap::nodeapiserver_apf_labels_requests(context).await
        }
        "test_nodeapiserver_exposes_inflight_metrics" => {
            bootstrap::nodeapiserver_exposes_inflight_metrics(context).await
        }
        "test_nodeapiserver_exposes_full_request_metrics" => {
            bootstrap::nodeapiserver_exposes_full_request_metrics(context).await
        }
        "test_nodeapiserver_honors_patch_dry_run" => bootstrap::nodeapiserver_honors_patch_dry_run(context).await,
        "test_nodeapiserver_authorizes_before_special_handlers" => {
            bootstrap::nodeapiserver_authorizes_before_special_handlers(context).await
        }
        "test_nodeapiserver_writes_audit_log" => bootstrap::nodeapiserver_writes_audit_log(context).await,
        "test_nodeapiserver_audits_rejected_requests" => {
            bootstrap::nodeapiserver_audits_rejected_requests(context).await
        },
        "test_nodeapiserver_rotates_audit_log" => bootstrap::nodeapiserver_rotates_audit_log(context).await,
        "test_nodeapiserver_delivers_audit_webhook" => {
            bootstrap::nodeapiserver_delivers_audit_webhook(context).await
        }
        "test_nodeapiserver_audits_request_and_response_objects" => {
            bootstrap::nodeapiserver_audits_request_and_response_objects(context).await
        }
        "test_nodeapiserver_rejects_unsupported_field_selector" => {
            bootstrap::nodeapiserver_rejects_unsupported_field_selector(context).await
        }
        "test_nodeapiserver_serves_generic_status_subresource" => {
            bootstrap::nodeapiserver_serves_generic_status_subresource(context).await
        }
        "test_nodeapiserver_excludes_status_from_main_managed_fields" => {
            bootstrap::nodeapiserver_excludes_status_from_main_managed_fields(context).await
        }
        "test_nodeapiserver_serves_ephemeralcontainers_subresource" => {
            bootstrap::nodeapiserver_serves_ephemeralcontainers_subresource(context).await
        }
        "test_nodeapiserver_enforces_service_account_mountable_secrets" => {
            bootstrap::nodeapiserver_enforces_service_account_mountable_secrets(context).await
        }
        "test_nodeapiserver_enforces_mountable_secrets_for_ephemeral_containers" => {
            bootstrap::nodeapiserver_enforces_mountable_secrets_for_ephemeral_containers(context).await
        }
        "test_nodeapiserver_watches_an_uncommon_builtin_resource" => {
            bootstrap::nodeapiserver_watches_an_uncommon_builtin_resource(context).await
        },
        "test_nodeapiserver_honors_watch_options" => bootstrap::nodeapiserver_honors_watch_options(context).await,
        "test_nodeapiserver_recreates_a_dynamic_watch_cache" => {
            bootstrap::nodeapiserver_recreates_a_dynamic_watch_cache(context).await
        },
        "test_nodeapiserver_watches_partial_object_metadata" => {
            bootstrap::nodeapiserver_watches_partial_object_metadata(context).await
        },
        "test_nodeapiserver_rejects_unsupported_resource_route" => {
            bootstrap::nodeapiserver_rejects_unsupported_resource_route(context).await
        },
        "test_nodeapiserver_rejects_oversized_request_body" => {
            bootstrap::nodeapiserver_rejects_oversized_request_body(context).await
        },
        "test_nodeapiserver_validating_admission_policy_denies_create" => {
            bootstrap::nodeapiserver_validating_admission_policy_denies_create(context).await
        },
        "test_nodeapiserver_enforces_crd_schema_constraints" => {
            bootstrap::nodeapiserver_enforces_crd_schema_constraints(context).await
        }
        "test_nodeapiserver_mutating_admission_policy_mutates_create" => {
            bootstrap::nodeapiserver_mutating_admission_policy_mutates_create(context).await
        },
        "test_nodeapiserver_validates_crd_status_subresource" => {
            bootstrap::nodeapiserver_validates_crd_status_subresource(context).await
        },
        "test_nodeapiserver_honors_webhook_match_conditions" => {
            bootstrap::nodeapiserver_honors_webhook_match_conditions(context).await
        },
        "test_nodeapiserver_honors_webhook_side_effects_on_dry_run" => {
            bootstrap::nodeapiserver_honors_webhook_side_effects_on_dry_run(context).await
        },
        "test_nodeapiserver_runs_webhook_for_delete_collection" => {
            bootstrap::nodeapiserver_runs_webhook_for_delete_collection(context).await
        },
        "test_nodeapiserver_honors_finalizers" => {
            bootstrap::nodeapiserver_honors_finalizers(context).await
        },
        "test_nodeapiserver_honors_authorization_webhook_decisions" => {
            bootstrap::nodeapiserver_honors_authorization_webhook_decisions(context).await
        },
        "test_nodeapiserver_honors_resource_version_snapshot" => {
            bootstrap::nodeapiserver_honors_resource_version_snapshot(context).await
        },
        "test_nodeapiserver_serves_partial_object_metadata" => {
            bootstrap::nodeapiserver_serves_partial_object_metadata(context).await
        },
        "test_nodeapiserver_honors_generate_name" => {
            bootstrap::nodeapiserver_honors_generate_name(context).await
        },
        "test_nodeapiserver_honors_dry_run_and_delete_preconditions" => {
            bootstrap::nodeapiserver_honors_dry_run_and_delete_preconditions(context).await
        },
        "test_graceful_node_shutdown_manual_note" => {
            bootstrap::graceful_node_shutdown_manual_note(context).await
        }
        "test_tls_bootstrap_issues_a_real_client_certificate" => {
            bootstrap::tls_bootstrap_issues_a_real_client_certificate(context).await
        }
        "test_combined_binary_contains_every_component" => {
            build_layout::combined_binary_contains_every_component(context).await
        }
        "test_combined_binary_rejects_an_unknown_component" => {
            build_layout::combined_binary_rejects_an_unknown_component(context).await
        }
        "test_installed_component_binaries_are_runnable_whatever_the_layout" => {
            build_layout::installed_component_binaries_are_runnable_whatever_the_layout(context).await
        }
        "test_a_failing_component_says_why_before_it_exits" => {
            build_layout::a_failing_component_says_why_before_it_exits(context).await
        }
        "apiserver_serves_resources" => apiserver_serves_resources(context.client.clone()).await,
        "node_is_ready" => node_is_ready(context.client.clone()).await,
        "test_kubernetes_service_has_a_reachable_endpoint" => {
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
        "test_crash_loop_backoff_throttles_immediate_restarts" => {
            lifecycle::crash_loop_backoff_throttles_immediate_restarts(context).await
        }
        "test_pod_with_a_finalizer_tears_down_but_stays_until_the_finalizer_is_removed" => {
            pods::pod_with_a_finalizer_tears_down_but_stays_until_removed(context).await
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
        "test_pod_exceeding_its_own_ephemeral_storage_limit_is_evicted" => {
            eviction::pod_exceeding_its_own_ephemeral_storage_limit_is_evicted(context).await
        }
        "test_pod_exceeding_an_empty_dir_size_limit_is_evicted" => {
            eviction::pod_exceeding_an_empty_dir_size_limit_is_evicted(context).await
        }
        "test_eviction_manual_procedure" => eviction::eviction_manual_procedure(context).await,
        "test_eviction_priority_tiebreak_manual_procedure" => {
            eviction::eviction_priority_tiebreak_manual_procedure(context).await
        }
        "test_eviction_exceeds_requests_tiebreak_manual_procedure" => {
            eviction::eviction_exceeds_requests_tiebreak_manual_procedure(context).await
        }
        "test_eviction_soft_grace_period_manual_procedure" => {
            eviction::eviction_soft_grace_period_manual_procedure(context).await
        }
        "test_node_is_ready_with_capacity_advertised" => {
            node_status::node_is_ready_with_capacity_advertised(context).await
        }
        "test_node_reports_hugepages_capacity_when_reserved" => {
            node_status::node_reports_hugepages_capacity_when_reserved(context).await
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
        "test_node_status_images_reflects_a_real_pulled_image" => {
            node_status::node_status_images_reflects_a_real_pulled_image(context).await
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
        "test_cert_manager_crds_are_usable_without_nodecontroller_restart" => {
            cert_manager::cert_manager_crds_are_usable_without_nodecontroller_restart(context).await
        }
        "test_resourcequota_used_pods_tracks_actual_pod_count" => {
            resource_quota::resourcequota_used_pods_tracks_actual_pod_count(context).await
        }
        "test_memory_limit_is_enforced_via_cgroup" => {
            resources::memory_limit_is_enforced_via_cgroup(context).await
        }
        "test_no_swap_default_disables_swap_via_cgroup" => {
            resources::no_swap_default_disables_swap_via_cgroup(context).await
        }
        "test_limited_swap_gives_burstable_pods_proportional_swap" => {
            resources::limited_swap_gives_burstable_pods_proportional_swap(context).await
        }
        "test_hugepages_limit_is_enforced_via_cgroup" => {
            resources::hugepages_limit_is_enforced_via_cgroup(context).await
        }
        "test_cpu_limit_is_enforced_via_cgroup" => {
            resources::cpu_limit_is_enforced_via_cgroup(context).await
        }
        "test_besteffort_pod_gets_no_cgroup_limit" => {
            resources::besteffort_pod_gets_no_cgroup_limit(context).await
        }
        "test_besteffort_pod_gets_the_certain_death_oom_score" => {
            resources::besteffort_pod_gets_the_certain_death_oom_score(context).await
        }
        "test_guaranteed_pod_gets_the_protected_oom_score" => {
            resources::guaranteed_pod_gets_the_protected_oom_score(context).await
        }
        "test_env_resource_field_ref_reports_the_containers_own_limits" => {
            resources::env_resource_field_ref_reports_the_containers_own_limits(context).await
        }
        "test_in_place_resize_updates_memory_limit_without_restarting" => {
            resources::in_place_resize_updates_memory_limit_without_restarting(context).await
        }
        "test_cpu_manager_pins_guaranteed_containers_to_disjoint_exclusive_cores" => {
            resource_managers::cpu_manager_pins_guaranteed_containers_to_disjoint_exclusive_cores(context).await
        }
        "test_cpu_manager_retroactively_shrinks_an_already_running_shared_pool_container" => {
            resource_managers::cpu_manager_retroactively_shrinks_an_already_running_shared_pool_container(context).await
        }
        "test_memory_manager_pins_guaranteed_containers_to_a_numa_node" => {
            resource_managers::memory_manager_pins_guaranteed_containers_to_a_numa_node(context).await
        }
        "test_log_rotation_creates_a_rotated_file" => {
            resource_managers::log_rotation_creates_a_rotated_file(context).await
        }
        "test_static_pod_creates_a_mirror_pod" => {
            static_pods::static_pod_creates_a_mirror_pod(context).await
        }
        "test_datastore_serves_the_etcd_status_rpc" => {
            datastore::datastore_serves_the_etcd_status_rpc(context).await
        }
        "test_datastore_round_trips_a_key_over_grpc" => {
            datastore::datastore_round_trips_a_key_over_grpc(context).await
        }
        "test_datastore_lists_a_prefix_in_key_order" => {
            datastore::datastore_lists_a_prefix_in_key_order(context).await
        }
        "test_datastore_enforces_compare_and_swap" => {
            datastore::datastore_enforces_compare_and_swap(context).await
        }
        "test_datastore_creates_a_key_only_if_absent" => {
            datastore::datastore_creates_a_key_only_if_absent(context).await
        }
        "test_datastore_streams_watch_events_as_they_happen" => {
            datastore::datastore_streams_watch_events_as_they_happen(context).await
        }
        "test_datastore_replays_missed_events_to_a_late_watcher" => {
            datastore::datastore_replays_missed_events_to_a_late_watcher(context).await
        }
        "test_datastore_refuses_a_read_below_the_compaction_point" => {
            datastore::datastore_refuses_a_read_below_the_compaction_point(context).await
        }
        "test_datastore_expires_a_lease_and_its_keys" => {
            datastore::datastore_expires_a_lease_and_its_keys(context).await
        }
        "test_datastore_survives_a_restart_with_its_data" => {
            datastore::datastore_survives_a_restart_with_its_data(context).await
        }
        "test_upgrade_a_populated_single_member_into_a_one_member_cluster" => {
            datastore::datastore_upgrades_a_populated_single_member_into_a_one_member_cluster(context).await
        }
        "test_upgrade_straight_to_a_multi_member_cluster_is_refused" => {
            datastore::datastore_refuses_direct_upgrade_to_a_multi_member_cluster(context).await
        }
        "test_upgrade_a_dead_member_leaves_nothing_listening_behind_it" => {
            datastore::datastore_shutdown_leaves_no_listener_behind(context).await
        }
        "test_datastore_refuses_a_cluster_it_cannot_be_part_of" => {
            datastore::datastore_refuses_a_cluster_it_cannot_be_part_of(context).await
        }
        "test_datastore_refuses_a_malformed_cluster_spec" => {
            datastore::datastore_refuses_a_malformed_cluster_spec(context).await
        }
        "test_cluster_elects_a_single_leader" => {
            datastore_cluster::cluster_elects_a_single_leader(context).await
        }
        "test_cluster_replicates_a_write_to_every_member" => {
            datastore_cluster::cluster_replicates_a_write_to_every_member(context).await
        }
        "test_a_follower_forwards_writes_to_the_leader" => {
            datastore_cluster::follower_forwards_writes_to_the_leader(context).await
        }
        "test_the_cluster_keeps_serving_when_a_follower_dies" => {
            datastore_cluster::cluster_keeps_serving_when_a_follower_dies(context).await
        }
        "test_the_cluster_survives_the_leader_being_killed" => {
            datastore_cluster::cluster_survives_the_leader_being_killed(context).await
        }
        "test_a_minority_refuses_writes_rather_than_inventing_them" => {
            datastore_cluster::minority_refuses_writes_rather_than_inventing_them(context).await
        }
        "test_a_restarted_member_catches_up_on_what_it_missed" => {
            datastore_cluster::restarted_member_catches_up_on_what_it_missed(context).await
        }
        "test_the_cluster_tolerates_a_slow_link" => {
            datastore_cluster::cluster_tolerates_a_slow_link(context).await
        }
        "test_a_partitioned_leader_steps_down_and_the_majority_elects_another" => {
            datastore_cluster::partitioned_leader_steps_down_and_majority_elects_another(context).await
        }
        "test_a_real_apiserver_starts_and_serves_against_nodestore" => {
            datastore_apiserver::real_apiserver_starts_and_serves_against_nodestore(context).await
        }
        "test_apiserver_crud_round_trips_through_nodestore" => {
            datastore_apiserver::apiserver_crud_round_trips_through_nodestore(context).await
        }
        "test_apiserver_watch_delivers_through_nodestore" => {
            datastore_apiserver::apiserver_watch_delivers_through_nodestore(context).await
        }
        "test_apiserver_state_survives_a_datastore_restart" => {
            datastore_apiserver::apiserver_state_survives_a_datastore_restart(context).await
        }
        "test_a_pending_pod_recovers_after_the_node_failure_is_fixed" => {
            host_recovery::pending_pod_recovers_after_the_node_failure_is_fixed(context).await
        }
        "test_existing_pod_recreates_its_container_after_a_runtime_restart" => {
            host_recovery::existing_pod_recreates_its_container_after_a_runtime_restart(context)
                .await
        }
        "test_endpointslice_is_produced_for_a_selected_pod" => {
            endpoint_slice::endpointslice_is_produced_for_a_selected_pod(context).await
        }
        "test_replacement_control_plane_identities_can_read_all_watch_inputs" => {
            component_rbac::replacement_control_plane_identities_can_read_all_watch_inputs(context).await
        }
        "test_config_file_sets_a_value_env_did_not_override" => {
            config_file::config_file_sets_a_value_env_did_not_override(context).await
        }
        "test_config_file_precedence_a_real_env_var_still_wins" => {
            config_file::config_file_precedence_a_real_env_var_still_wins(context).await
        }
        "test_config_dir_merges_files_in_filename_order" => {
            config_file::config_dir_merges_files_in_filename_order(context).await
        }
        "test_scheduler_holds_the_leader_lease" => {
            scheduler::scheduler_holds_the_leader_lease(context).await
        }
        "test_scheduler_wakes_a_pending_pod_on_a_real_event" => {
            scheduler::scheduler_wakes_a_pending_pod_on_a_real_event(context).await
        }
        "test_node_allocatable_cgroup_exists_and_is_capped" => {
            cgroup::node_allocatable_cgroup_exists_and_is_capped(context).await
        }
        "test_pod_cgroup_reflects_its_qos_class" => {
            cgroup::pod_cgroup_reflects_its_qos_class(context).await
        }
        "test_garbage_collector_cascades_deployment_delete_to_replicaset_and_pods" => {
            garbage_collection::garbage_collector_cascades_deployment_delete_to_replicaset_and_pods(context).await
        }
        "test_pod_teardown_actually_removes_the_sandbox" => {
            garbage_collection::pod_teardown_actually_removes_the_sandbox(context).await
        }
        "test_orphaned_sandbox_gc_reaps_a_pod_deleted_while_nodelet_is_down" => {
            garbage_collection::orphaned_sandbox_gc_reaps_a_pod_deleted_while_nodelet_is_down(context).await
        }
        "test_unreferenced_image_is_not_removed_below_the_watermark" => {
            garbage_collection::unreferenced_image_is_not_removed_below_the_watermark(context).await
        }
        "test_image_gc_removes_unreferenced_images_above_the_watermark" => {
            garbage_collection::image_gc_removes_unreferenced_images_above_the_watermark(context).await
        }
        "test_allocated_resources_status_absent_without_device_resources" => {
            device_plugins::allocated_resources_status_absent_without_device_resources(context).await
        }
        "test_device_plugin_advertises_capacity_and_allocates_into_a_container" => {
            device_plugins::device_plugin_advertises_capacity_and_allocates_into_a_container(context).await
        }
        "test_device_plugin_health_transition_updates_allocated_resources_status" => {
            device_plugins::device_plugin_health_transition_updates_allocated_resources_status(context).await
        }
        "test_device_plugin_preferred_allocation_and_prestart" => {
            device_plugins::device_plugin_preferred_allocation_and_prestart(context).await
        }
        "test_host_network_pod_uses_the_node_network_namespace" => {
            networking::host_network_pod_uses_the_node_network_namespace(context).await
        }
        "test_host_port_publishes_the_container_on_the_nodes_own_ip" => {
            networking::host_port_reaches_the_container_on_the_node_ip(context).await
        }
        "test_host_network_pod_needs_no_explicit_port_mapping" => {
            networking::host_network_pod_needs_no_explicit_port_mapping(context).await
        }
        "test_custom_dns_config_reaches_resolv_conf" => {
            networking::custom_dns_config_reaches_resolv_conf(context).await
        }
        "test_enable_service_links_false_preserves_kubernetes_env" => {
            networking::enable_service_links_false_preserves_kubernetes_env(context).await
        }
        "test_cluster_dns_resolves_service_names" => networking::cluster_dns_resolves_service_names(context).await,
        "test_spec_hostname_overrides_the_container_hostname" => {
            networking::spec_hostname_overrides_the_container_hostname(context).await
        }
        "test_set_hostname_as_fqdn_reports_the_full_fqdn" => {
            networking::set_hostname_as_fqdn_reports_the_full_fqdn(context).await
        }
        "test_pod_resources_socket_is_created_on_a_cri_node" => {
            pod_resources::pod_resources_socket_is_created_on_a_cri_node(context).await
        }
        "test_pod_resources_grpc_query_returns_real_data" => {
            pod_resources::pod_resources_grpc_query_returns_real_data(context).await
        }
        "test_plugin_registry_directory_exists" => {
            pod_resources::plugin_registry_directory_exists(context).await
        }
        "test_plugin_registry_watches_for_device_plugins_too" => {
            device_plugins::plugin_registry_watches_for_device_plugins_too(context).await
        }
        "test_plugin_registry_watches_for_dra_drivers_too" => {
            dra::plugin_registry_watches_for_dra_drivers_too(context).await
        }
        "test_dynamic_csi_registration_actually_registered_the_driver" => {
            pod_resources::dynamic_csi_registration_is_visible_on_the_node(context).await
        }
        "test_resource_api_group_is_enabled" => dra::resource_api_group_is_enabled(context).await,
        "test_credential_provider_config_unset_by_default" => {
            credential_provider::credential_provider_config_unset_by_default(context).await
        }
        "test_credential_provider_supplies_auth_for_an_otherwise_rejected_pull" => {
            credential_provider::credential_provider_supplies_auth_for_an_otherwise_rejected_pull(context).await
        }
        "test_dra_claim_is_allocated_and_reserved_for_the_pod" => {
            dra::dra_claim_is_allocated_and_reserved_for_the_pod(context).await
        }
        "test_csi_ephemeral_inline_volume_is_mounted" => {
            csi::csi_ephemeral_inline_volume_is_mounted(context).await
        }
        "test_pod_uses_a_raw_block_volume" => csi::pod_uses_a_raw_block_volume(context).await,
        "test_node_reports_volumes_in_use_for_a_csi_volume" => {
            csi::node_reports_volumes_in_use_for_a_csi_volume(context).await
        }
        "test_fsgroup_change_policy_on_root_mismatch_skips_the_second_chown" => {
            csi::fsgroup_change_policy_on_root_mismatch_skips_the_second_chown(context).await
        }
        "test_pod_with_an_attach_required_pvc_waits_for_volumeattachment" => {
            csi::pod_with_an_attach_required_pvc_waits_for_volumeattachment(context).await
        }
        "test_scheduler_places_an_ordinary_pod" => {
            scheduler::scheduler_places_an_ordinary_pod(context).await
        }
        "test_scheduler_consults_an_http_extender_and_honours_a_filter_rejection" => {
            scheduler::scheduler_consults_an_http_extender_and_honours_a_filter_rejection(context).await
        }
        "test_scheduler_schedules_a_pod_an_http_extender_approves" => {
            scheduler::scheduler_schedules_a_pod_an_http_extender_approves(context).await
        }
        "test_scheduler_honours_a_node_selector_that_matches" => {
            scheduler::scheduler_honours_a_matching_node_selector(context).await
        }
        "test_scheduler_rejects_a_pod_that_does_not_fit" => {
            scheduler::scheduler_rejects_a_pod_that_does_not_fit(context).await
        }
        "test_scheduler_preempts_a_lower_priority_pod" => {
            scheduler::scheduler_preempts_a_lower_priority_pod(context).await
        }
        "test_scheduler_does_not_preempt_when_policy_forbids_it" => {
            scheduler::scheduler_does_not_preempt_when_policy_forbids_it(context).await
        }
        "test_scheduler_honours_a_node_selector_that_matches_nothing" => {
            scheduler::scheduler_leaves_an_impossible_selector_pending(context).await
        }
        "test_scheduler_leaves_a_gated_pod_alone" => {
            scheduler::scheduler_leaves_a_gated_pod_alone(context).await
        }
        "test_scheduler_ignores_pods_for_another_scheduler" => {
            scheduler::scheduler_ignores_a_pod_for_another_scheduler(context).await
        }
        "test_scheduler_honours_pod_affinity" => {
            scheduler::scheduler_honours_pod_affinity(context).await
        }
        "test_scheduler_honours_pod_anti_affinity" => {
            scheduler::scheduler_honours_pod_anti_affinity(context).await
        }
        "test_scheduler_honours_topology_spread" => {
            scheduler::scheduler_honours_topology_spread(context).await
        }
        "test_scheduler_respects_a_taint_and_its_toleration" => {
            scheduler::scheduler_respects_a_taint_and_its_toleration(context).await
        }
        "test_scheduler_resolves_a_namespace_selector_against_real_labels" => {
            scheduler::scheduler_resolves_a_namespace_selector_against_real_labels(context).await
        }
        "test_scheduler_schedules_pods_that_get_default_spread_constraints" => {
            scheduler::scheduler_schedules_pods_that_get_default_spread_constraints(context).await
        }
        "test_scheduler_delays_binding_a_wait_for_first_consumer_pvc_until_a_node_is_chosen" => {
            scheduler::scheduler_delays_binding_a_wait_for_first_consumer_pvc_until_a_node_is_chosen(context).await
        }
        "test_scheduler_claims_a_static_wait_for_first_consumer_volume" => {
            scheduler::scheduler_claims_a_static_wait_for_first_consumer_volume(context).await
        }
        "test_scheduler_enforces_read_write_once_pod_exclusivity" => {
            scheduler::scheduler_enforces_read_write_once_pod_exclusivity(context).await
        }
        "test_restart_policy_never_exit_zero_is_succeeded" => {
            lifecycle::restart_policy_never_exit_zero_is_succeeded(context).await
        }
        "test_restart_policy_never_exit_nonzero_is_failed" => {
            lifecycle::restart_policy_never_nonzero_exit_is_failed(context).await
        }
        "test_exited_container_reports_terminated_state_with_exit_code" => {
            lifecycle::terminated_container_reports_its_exit_code(context).await
        }
        "test_guaranteed_pod_reports_guaranteed_qos" => {
            lifecycle::guaranteed_pod_reports_guaranteed_qos(context).await
        }
        "test_pod_status_reports_qos_class" => {
            lifecycle::pod_status_reports_qos_class(context).await
        }
        "test_pod_exceeding_its_active_deadline_is_terminated" => {
            lifecycle::pod_exceeding_its_active_deadline_is_terminated(context).await
        }
        "test_container_status_container_id_has_a_runtime_scheme_prefix" => {
            lifecycle::container_status_id_has_runtime_scheme(context).await
        }
        "test_crash_loop_backoff_reports_waiting_reason" => {
            lifecycle::crash_loop_backoff_reports_waiting_reason(context).await
        }
        "test_crash_loop_backoff_reports_waiting_reason_and_last_state" => {
            lifecycle::crash_loop_backoff_reports_waiting_reason_and_last_state(context).await
        }
        "test_image_pull_policy_never_fails_when_image_is_absent" => {
            lifecycle::image_pull_policy_never_fails_when_image_is_absent(context).await
        }
        "test_image_pull_policy_if_not_present_skips_the_registry_round_trip" => {
            lifecycle::image_pull_policy_if_not_present_skips_the_registry_round_trip(context).await
        }
        "test_pod_status_reports_host_ips_plural" => {
            lifecycle::pod_status_reports_host_ips_plural(context).await
        }
        "test_pod_condition_reports_observed_generation" => {
            pod_status::pod_condition_reports_observed_generation(context).await
        }
        "test_container_status_reports_a_real_image_id" => {
            lifecycle::container_status_reports_a_real_image_id(context).await
        }
        "test_readiness_probe_gates_ready_condition" => {
            probes::readiness_probe_gates_ready_condition(context).await
        }
        "test_liveness_probe_failure_restarts_the_container" => {
            probes::liveness_probe_failure_restarts_container(context).await
        }
        "test_liveness_probes_own_grace_period_overrides_the_pods" => {
            probes::liveness_probe_uses_its_own_grace_period(context).await
        }
        "test_startup_probe_gates_readiness_until_server_starts" => {
            probes::startup_probe_gates_readiness_until_server_starts(context).await
        }
        "test_startup_probe_gates_liveness_and_readiness" => {
            probes::startup_probe_gates_liveness_and_readiness(context).await
        }
        "test_startup_probe_failure_past_threshold_restarts_the_container" => {
            probes::startup_probe_failure_past_threshold_restarts_the_container(context).await
        }
        "test_http_get_readiness_probe_against_a_real_server" => {
            probes::http_get_readiness_probe_against_a_real_server(context).await
        }
        "test_wrong_port_readiness_probe_keeps_pod_not_ready" => {
            probes::wrong_port_readiness_probe_keeps_pod_not_ready(context).await
        }
        "test_grpc_probe_gates_readiness_against_a_real_grpc_server" => {
            probes::grpc_probe_gates_readiness_against_a_real_grpc_server(context).await
        }
        "test_grpc_probe_leaves_pod_not_ready_against_the_wrong_port" => {
            probes::grpc_probe_leaves_pod_not_ready_against_the_wrong_port(context).await
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
        "test_service_account_token_projected_volume_mints_a_real_token" => {
            volumes::service_account_token_projected_volume_mints_a_token(context).await
        }
        "test_host_aliases_are_written_to_etc_hosts" => {
            volumes::host_aliases_are_written_to_etc_hosts(context).await
        }
        "test_host_aliases_still_work_under_host_users_false" => {
            volumes::host_aliases_still_work_under_host_users_false(context).await
        }
        "test_empty_dir_medium_memory_is_backed_by_tmpfs" => {
            volumes::empty_dir_memory_is_backed_by_tmpfs(context).await
        }
        "test_empty_dir_medium_hugepages_is_backed_by_hugetlbfs" => {
            volumes::empty_dir_hugepages_is_backed_by_hugetlbfs(context).await
        }
        "test_image_volume_source_mounts_a_read_only_image" => {
            volumes::image_volume_source_mounts_a_read_only_image(context).await
        }
        "test_configmap_volume_updates_live_without_pod_restart" => {
            volumes::configmap_volume_updates_live_without_pod_restart(context).await
        }
        "test_host_path_directory_mounts_the_real_host_directory" => {
            volumes::host_path_directory_mounts_the_real_host_directory(context).await
        }
        "test_host_path_directory_or_create_creates_a_missing_directory" => {
            volumes::host_path_directory_or_create_creates_missing_directory(context).await
        }
        "test_mount_propagation_host_to_container_still_mounts_normally" => {
            volumes::mount_propagation_host_to_container_still_mounts_normally(context).await
        }
        "test_recursive_read_only_still_mounts_read_only_normally" => {
            volumes::recursive_read_only_still_mounts_read_only_normally(context).await
        }
        "test_mount_propagation_host_to_container_sees_a_new_host_mount" => {
            volumes::mount_propagation_host_to_container_sees_a_new_host_mount(context).await
        }
        "test_mount_propagation_private_default_does_not_see_a_new_host_mount" => {
            volumes::mount_propagation_private_default_does_not_see_a_new_host_mount(context).await
        }
        "test_recursive_read_only_enabled_blocks_writes_in_a_nested_mount_too" => {
            volumes::recursive_read_only_enabled_blocks_writes_in_a_nested_mount_too(context).await
        }
        "test_recursive_read_only_if_possible_falls_back_without_erroring" => {
            volumes::recursive_read_only_if_possible_falls_back_without_erroring(context).await
        }
        "test_recursive_read_only_if_possible_tracks_the_runtime_handlers_own_capability" => {
            volumes::recursive_read_only_if_possible_tracks_the_runtime_handlers_own_capability(context).await
        }
        "test_sub_path_expr_expands_a_downward_api_env_var" => {
            volumes::sub_path_expr_expands_a_downward_api_env_var(context).await
        }
        "test_host_path_directory_type_rejects_a_nonexistent_path" => {
            volumes::host_path_directory_type_rejects_a_nonexistent_path(context).await
        }
        "test_fsgroup_never_applies_to_hostpath_volumes" => {
            volumes::fsgroup_never_applies_to_hostpath_volumes(context).await
        }
        "test_fsgroup_chowns_materialized_volumes" => {
            volumes::fsgroup_chowns_materialized_volumes(context).await
        }
        "test_read_only_root_filesystem_is_enforced" => {
            security::read_only_root_filesystem_blocks_writes(context).await
        }
        "test_without_read_only_root_filesystem_writes_succeed" => {
            security::writable_root_filesystem_allows_writes(context).await
        }
        "test_run_as_user_is_applied" => {
            security::run_as_user_is_applied(context).await
        }
        "test_container_status_reports_resolved_user" => {
            security::container_status_reports_resolved_user(context).await
        }
        "test_container_status_reports_recursive_read_only" => {
            security::container_status_reports_recursive_read_only(context).await
        }
        "test_host_users_false_gets_a_real_user_namespace" => {
            security::host_users_false_gets_a_real_user_namespace(context).await
        }
        "test_host_users_false_volume_still_reads_and_writes_normally" => {
            security::host_users_false_volume_still_reads_and_writes_normally(context).await
        }
        "test_host_users_volume_ownership_translation_is_correct" => {
            security::host_users_volume_ownership_translation_is_correct(context).await
        }
        "test_supplemental_groups_policy_strict_ignores_image_group_membership" => {
            security::supplemental_groups_policy_strict_ignores_image_group_membership(context).await
        }
        "test_client_certificate_authentication_works" => {
            security::client_certificate_authentication_works(context).await
        }
        "test_topology_manager_does_not_reject_pods_on_a_single_numa_node_host" => {
            topology::topology_manager_does_not_reject_pods_on_a_single_numa_node_host(context).await
        }
        "test_topology_manager_restricted_does_not_reject_pods_on_a_single_numa_node_host" => {
            topology::topology_manager_restricted_does_not_reject_pods_on_a_single_numa_node_host(context).await
        }
        "test_topology_manager_cross_provider_alignment_manual_note" => {
            topology::topology_manager_cross_provider_alignment_manual_note(context).await
        }
        "test_topology_manager_restricted_spread_manual_note" => {
            topology::topology_manager_restricted_spread_manual_note(context).await
        }
        "test_sysctls_are_applied_to_the_sandbox" => {
            security::sysctls_are_applied_to_the_sandbox(context).await
        }
        "test_proc_mount_default_masks_proc_kcore" => {
            security::proc_mount_default_masks_proc_kcore(context).await
        }
        "test_proc_mount_unmasked_leaves_proc_kcore_readable" => {
            security::proc_mount_unmasked_leaves_proc_kcore_readable(context).await
        }
        "test_containers_get_isolated_pid_namespaces_by_default" => {
            process::containers_get_isolated_pid_namespaces_by_default(context).await
        }
        "test_share_process_namespace_puts_every_container_in_one_pid_namespace" => {
            process::share_process_namespace_puts_every_container_in_one_pid_namespace(context).await
        }
        "test_host_pid_sees_host_processes" => process::host_pid_sees_host_processes(context).await,
        "test_poststart_hook_runs_after_container_start" => {
            hooks::poststart_hook_runs_before_container_exit(context).await
        }
        "test_termination_message_path_is_read_back_into_container_status" => {
            hooks::termination_message_path_is_read_back_into_status(context).await
        }
        "test_lifecycle_stop_signal_is_honored_by_the_runtime" => {
            hooks::lifecycle_stop_signal_is_honored_by_the_runtime(context).await
        }
        "test_prestop_hook_runs_before_termination" => {
            hooks::prestop_hook_runs_before_termination(context).await
        }
        "test_termination_grace_period_is_honored_not_instant" => {
            hooks::termination_grace_period_is_honored_not_instant(context).await
        }
        "test_clusterip_service_routes_to_its_backend_pod" => {
            service_proxy::clusterip_service_routes_to_its_backend_pod(context).await
        }
        "test_nodeapiserver_proxy_subresources_relay_requests" => {
            service_proxy::nodeapiserver_proxy_subresources_relay_requests(context).await
        }
        "test_nodeport_service_is_reachable_on_the_node_ip" => {
            service_proxy::nodeport_service_is_reachable_on_the_node_ip(context).await
        }
        "test_service_with_no_endpoints_does_not_wedge_the_ruleset" => {
            service_proxy::service_with_no_endpoints_does_not_wedge_the_ruleset(context).await
        }
        "test_headless_service_programs_no_rules_and_does_not_break_others" => {
            service_proxy::headless_service_does_not_break_other_services(context).await
        }
        "test_clusterip_is_reachable_from_inside_a_pod" => {
            service_proxy::clusterip_is_reachable_from_inside_a_pod(context).await
        }
        "test_nodeproxy_runs_as_its_own_service_separate_from_nodelet" => {
            service_proxy::nodeproxy_runs_as_its_own_service_separate_from_nodelet(context).await
        }
        "test_a_pod_reaching_its_own_service_gets_hairpin_masquerade" => {
            service_proxy::a_pod_reaching_its_own_service_gets_hairpin_masquerade(context).await
        }
        "test_multiple_backends_actually_share_traffic" => {
            service_proxy::multiple_backends_actually_share_traffic(context).await
        }
        "test_losing_every_backend_removes_the_dnat_rule" => {
            service_proxy::losing_every_backend_removes_the_dnat_rule(context).await
        }
        "test_deleting_a_service_removes_its_rules" => {
            service_proxy::deleting_a_service_removes_its_rules(context).await
        }
        "test_session_affinity_pins_a_client_to_one_backend" => {
            service_proxy::session_affinity_pins_a_client_to_one_backend(context).await
        }
        "test_nodeproxy_rebuilds_the_whole_ruleset_after_a_restart" => {
            service_proxy::nodeproxy_rebuilds_the_whole_ruleset_after_a_restart(context).await
        }
        "test_a_long_lived_watch_survives_a_service_churn_burst" => {
            service_proxy::a_long_lived_watch_survives_a_service_churn_burst(context).await
        }
        "test_pv_binder_binds_a_static_pv_and_protection_finalizers_gate_deletion" => {
            storage::pv_binder_binds_a_static_pv_and_protection_finalizers_gate_deletion(context).await
        }
        "test_pv_binder_requests_dynamic_provisioning_from_the_storage_class" => {
            storage::pv_binder_requests_dynamic_provisioning_from_storage_class(context).await
        }
        "test_pod_mounts_a_persistent_volume_claim" => {
            storage::pod_mounts_a_persistent_volume_claim(context).await
        }
        "test_kubectl_logs_returns_real_output" => {
            streaming::kubectl_logs_returns_real_output(context).await
        }
        "test_kubectl_logs_follow_streams_new_output" => {
            streaming::kubectl_logs_follow_streams_new_output(context).await
        }
        "test_kubectl_exec_runs_a_command_and_returns_its_output" => {
            streaming::kubectl_exec_runs_a_command_and_returns_its_output(context).await
        }
        "test_kubectl_attach_streams_the_containers_stdout" => {
            streaming::kubectl_attach_streams_the_containers_stdout(context).await
        }
        "test_kubectl_port_forward_reaches_a_real_container_port" => {
            streaming::kubectl_port_forward_reaches_a_real_container_port(context).await
        }
        "test_stats_summary_returns_real_pod_usage" => {
            metrics::stats_summary_returns_real_pod_usage(context).await
        }
        "test_metrics_resource_returns_real_pod_usage" => {
            metrics::metrics_resource_returns_real_pod_usage(context).await
        }
        "test_metrics_cadvisor_returns_real_container_usage" => {
            metrics::metrics_cadvisor_returns_real_container_usage(context).await
        }
        "test_a_slow_terminating_pod_does_not_stall_another_pods_creation" => {
            termination::slow_terminating_pod_does_not_stall_another_pods_creation(context).await
        }
        "test_a_recreated_pod_survives_the_old_pods_detached_teardown" => {
            termination::recreated_pod_survives_the_old_pods_detached_teardown(context).await
        }
        "test_the_node_still_reconciles_pods_after_an_apiserver_restart" => {
            watch_recovery::node_still_reconciles_pods_after_an_apiserver_restart(context).await
        }
        "test_node_is_tainted_unreachable_after_heartbeat_loss_and_recovers" => {
            controller_manager::node_is_tainted_unreachable_after_heartbeat_loss_and_recovers(context).await
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
        assert_eq!(
            select_tests(None, None).unwrap(),
            reorder_environment_reconfiguring_tests(test_names())
        );
    }

    #[test]
    fn only_matches_test_name_substrings_and_comma_separates() {
        assert_eq!(
            select_tests(Some("node_is_ready,kubernetes_service"), None).unwrap(),
            vec![
                "node_is_ready",
                "test_kubernetes_service_has_a_reachable_endpoint",
                "test_node_is_ready_with_capacity_advertised"
            ]
        );
    }

    #[test]
    fn an_unknown_only_pattern_is_an_error() {
        assert!(select_tests(Some("does_not_exist"), None).is_err());
    }

    #[test]
    fn driver_requirement_follows_selected_test_group() {
        assert!(selected_tests_need_drivers(
            Some("test_pod_mounts_a_generic_ephemeral_volume"),
            None
        )
        .unwrap());
        assert!(!selected_tests_need_drivers(
            Some("test_node_is_ready_with_capacity_advertised"),
            None
        )
        .unwrap());
    }

    #[test]
    fn disruptive_environment_tests_run_after_ordinary_tests_in_each_shard() {
        for index in 1..=5 {
            let selected = select_tests(None, Some(&format!("{index}/5"))).unwrap();
            let mut deferred = false;
            for name in selected {
                if is_environment_reconfiguring_test(name) {
                    deferred = true;
                } else {
                    assert!(!deferred, "ordinary test {name} followed a disruptive test");
                }
            }
        }
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
