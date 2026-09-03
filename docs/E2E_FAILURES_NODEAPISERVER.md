# nodeapiserver e2e failure tracker (living document)

Started 2026-09-03. Tracks real, verified e2e failures against the
`nodeapiserver` branch and the fix/branch/PR working through them. Update
this file as failures are triaged, fixed, or found already fixed by a
rebase. Codex is working the same branch concurrently on the podresize
subresource and other fixes — check `gh pr list` / `git log
nodeapiserver..<branch>` before starting a new failure to avoid duplicating
or conflicting with in-flight work.

## Baseline run analyzed

- Workflow: `Bootstrap e2e` (`.github/workflows/e2e.yml`), run
  [33674376893](https://github.com/centerionware/not-k8s/actions/runs/33674376893),
  dispatched 2026-09-02T19:37:02Z against `nodeapiserver` @ `444f0d67`
  (PR #496). All 5 shards failed.
- **`nodeapiserver` has since moved to `42080746` (#503)** — 7 more PRs
  merged after the analyzed run. Some of the below may already be fixed;
  confirm against current `nodeapiserver` HEAD before opening a branch, and
  re-run the specific test with `--only=` first.
- Raw shard logs pulled via `gh run view --log --job=<id>` and archived
  in this session's scratchpad (not committed — regenerate with the run ID
  above if needed).

## Headline finding: this is NOT 155 independent bugs

155 of ~239 tests failed across the 5 shards, but the shards run their
tests **sequentially**, and in every shard the failures form one contiguous
cascade: one real bug fails a test early, and then the shard's cluster
never recovers — every subsequent test in that shard times out waiting for
its Pod to reach `Running` (or, once things get bad enough, times out just
waiting for a fresh namespace's default ServiceAccount to be provisioned).
**There are only a handful of distinct root causes here, one per shard**,
not 155. Fixing the root cause per shard should turn most of that shard's
failures green again.

Shard→job ID mapping for that run (for re-fetching logs):
shard 1=`100398989353`, 2=`100398989220`, 3=`100398989137`,
4=`100398989266`, 5=`100398989300`.

## Root causes identified (one per shard)

### 1. Shard 3 — nodeapiserver crashes/restarts right after a Binding subresource call

First test after the crash window: `test_nodeapiserver_binds_a_pod_through_binding_subresource`
**passes**, then the very next test's request gets `client error (Connect):
tcp connect error: Connection refused` — `nodeapiserver.service` shows
`Active: active (running) since <~2s before the dump>`, i.e. systemd
restarted it. All 54 tests after that point in the shard fail, almost all
instantly (0ms) on "creating the per-test namespace failed:
... Connection refused".

- **Status: NOT STARTED.**
- Hypothesis: something in the Binding subresource handler (recently added
  per `9356565f feat(nodeapiserver): proxy node and service subresources`
  and the subsequent refactor `dc782b94`/`aef12abc`) panics asynchronously
  shortly after the request returns 2xx — a spawned task, not the request
  handler itself, since the *triggering* test itself reported PASS.
- The last 300 lines of `journalctl -u nodeapiserver` at dump time is
  always saturated with audit-log spam within seconds (very high watch/audit
  volume), so the actual panic message rotated out of every dump in this
  run. **Next step: reproduce locally/in CI with a smaller journal window
  or `--only=test_nodeapiserver_binds_a_pod_through_binding_subresource`
  immediately followed by a service-status dump**, or grep source for
  `tokio::spawn` in the binding/proxy subresource paths for an unguarded
  `.unwrap()`/`.expect()`.
- Affected tests: see `git log`-free list below (54 tests, shard 3).

### 2. Shard 2 — node-restriction admission rejects a node deleting its own mirror Pod

`test_nodeapiserver_enforces_node_restriction` fails first:
`node identity could not delete its own mirror Pod (HTTP 403)`. Everything
after it in the shard times out waiting for pods to schedule/run, tailing
into "timed out waiting for the e2e namespace's default ServiceAccount".

- **Status: NOT STARTED.**
- A local (unpushed, uncommitted-to-remote) branch `apiserver-node-restriction`
  exists with a commit `feat(nodeapiserver): enforce node restriction
  admission` — likely the origin of this admission check, but it's based on
  an old point in history (`#427`-era) well behind current `nodeapiserver`
  tip (`#503`). Don't merge it as-is; read its diff for the intended
  authorization rule and re-derive the fix against current
  `crates/nodeapiserver/src/admission/` (or wherever node-restriction now
  lives) — the bug is almost certainly that the node-restriction check
  doesn't special-case a node deleting a **mirror Pod it owns** (static pod
  mirror deletion is one of the few self-delete cases the real
  `NodeRestriction` admission plugin explicitly allows).
- Affected tests: 25 tests, shard 2.

### 3. Shard 4 — pure admission (mutating webhook / defaulting) not re-run on PATCH

`test_nodeapiserver_applies_pure_admission_to_apply` fails first: `ApiError`
patching a Pod to verify pure admission runs on PATCH. Same cascade pattern
follows (37 tests, shard 4).

- **Status: NOT STARTED.**
- Check `crates/nodeapiserver/src/admission/` dispatch: does the admission
  chain run for PATCH requests, or only for CREATE/UPDATE? Get the real
  `ApiError` body (log dump for this shard has it — reproduce with
  `--only=test_nodeapiserver_applies_pure_admission_to_apply` since we
  don't yet have the actual response body captured, only the truncated
  message).

### 4. Shard 1 — nodelet never writes its issued client-cert kubeconfig

`test_tls_bootstrap_issues_a_real_client_certificate` fails first: "timed
out waiting for nodelet to write its issued client certificate kubeconfig".
28 tests fail in this shard following the same cascade.

- **Status: NOT STARTED.**
- Likely a CSR/certificate-issuance path regression, possibly interacting
  with `nodeapiserver`'s CSR handling (`test_nodeapiserver_rejects_privileged_csr_subject`
  passed earlier in shard 3, so CSR rejection logic works — this looks more
  like the *approval → issued cert → nodelet writes kubeconfig* leg).

### 5. Shard 5 — Pod Ready condition never reports `observedGeneration`

`test_pod_condition_reports_observed_generation` fails first: "timed out
waiting for Pod Ready observedGeneration". 11 tests fail in this shard.

- **Status: NOT STARTED.**
- Check whichever component sets Pod status conditions
  (`crates/nodelet` pod status reporting, or `nodeapiserver`'s status
  subresource merge) for whether it copies `observedGeneration` onto the
  `Ready` condition it writes.

## Non-cascade items: skips due to missing nodeapiserver features

Not yet enumerated separately from the above — the harness only reports
`FAIL`, not `SKIP`, in this run's summary line, so a skip census needs a
separate `grep SKIP` pass over the raw logs (all 5 `shard_*.log` archived
this session). **Do this before starting feature-gap work**: search each
shard log for `SKIP` to build that list. Exclude anything Pod-resize
related — Codex owns that (`nodeapiserver-pod-resize` branch / PR #504,
open).

## Full failing-test list from the analyzed run (155 unique)

Cross-reference against the per-shard root-cause sections above — a test
not mentioned by name in a root-cause section is a cascade victim of that
shard's root cause, not necessarily its own bug. Regenerate this list from
a fresh run once the 5 root causes are fixed, since most of these should
simply pass once their shard's blocker is gone.

```
test_a_pending_pod_recovers_after_the_node_failure_is_fixed
test_a_pod_reaching_its_own_service_gets_hairpin_masquerade
test_a_recreated_pod_survives_the_old_pods_detached_teardown
test_a_slow_terminating_pod_does_not_stall_another_pods_creation
test_apiserver_state_survives_a_datastore_restart
test_cert_manager_crds_are_usable_without_nodecontroller_restart
test_client_certificate_authentication_works
test_cluster_dns_resolves_service_names
test_clusterip_is_reachable_from_inside_a_pod
test_clusterip_service_routes_to_its_backend_pod
test_combined_binary_rejects_an_unknown_component
test_config_dir_merges_files_in_filename_order
test_config_file_precedence_a_real_env_var_still_wins
test_config_file_sets_a_value_env_did_not_override
test_configmap_and_secret_volumes_are_materialized
test_configmap_volume_updates_live_without_pod_restart
test_container_status_container_id_has_a_runtime_scheme_prefix
test_container_status_reports_a_real_image_id
test_container_status_reports_recursive_read_only
test_container_status_reports_resolved_user
test_cpu_manager_pins_guaranteed_containers_to_disjoint_exclusive_cores
test_cpu_manager_retroactively_shrinks_an_already_running_shared_pool_container
test_crash_loop_backoff_reports_waiting_reason_and_last_state
test_crashing_container_restarts_and_increments_restart_count
test_credential_provider_supplies_auth_for_an_otherwise_rejected_pull
test_csi_ephemeral_inline_volume_is_mounted
test_custom_dns_config_reaches_resolv_conf
test_daemonset_places_a_pod_directly
test_deleting_a_service_removes_its_rules
test_deployment_creates_replicaset_and_rolls_update
test_device_plugin_preferred_allocation_and_prestart
test_disruption_controller_computes_pdb_status
test_downward_api_volume_writes_pod_metadata
test_dra_claim_is_allocated_and_reserved_for_the_pod
test_empty_dir_medium_hugepages_is_backed_by_hugetlbfs
test_empty_dir_medium_memory_is_backed_by_tmpfs
test_enable_service_links_false_preserves_kubernetes_env
test_env_resource_field_ref_reports_the_containers_own_limits
test_eviction_manual_procedure
test_fsgroup_change_policy_on_root_mismatch_skips_the_second_chown
test_fsgroup_chowns_materialized_volumes
test_fsgroup_never_applies_to_hostpath_volumes
test_grpc_probe_leaves_pod_not_ready_against_the_wrong_port
test_guaranteed_pod_gets_the_protected_oom_score
test_guaranteed_pod_reports_guaranteed_qos
test_headless_service_programs_no_rules_and_does_not_break_others
test_host_aliases_are_written_to_etc_hosts
test_host_aliases_still_work_under_host_users_false
test_host_network_pod_needs_no_explicit_port_mapping
test_host_network_pod_uses_the_node_network_namespace
test_host_path_directory_mounts_the_real_host_directory
test_host_path_directory_or_create_creates_a_missing_directory
test_host_path_directory_type_rejects_a_nonexistent_path
test_host_pid_sees_host_processes
test_host_users_false_volume_still_reads_and_writes_normally
test_host_users_volume_ownership_translation_is_correct
test_hugepages_limit_is_enforced_via_cgroup
test_image_gc_removes_unreferenced_images_above_the_watermark
test_image_pull_policy_if_not_present_skips_the_registry_round_trip
test_image_pull_policy_never_fails_when_image_is_absent
test_init_container_failure_blocks_app_container_under_restart_policy_never
test_job_controller_runs_pods_to_completion
test_kubectl_attach_streams_the_containers_stdout
test_kubectl_exec_runs_a_command_and_returns_its_output
test_kubectl_logs_returns_real_output
test_kubectl_port_forward_reaches_a_real_container_port
test_lifecycle_stop_signal_is_honored_by_the_runtime
test_limited_swap_gives_burstable_pods_proportional_swap
test_liveness_probe_failure_restarts_the_container
test_liveness_probes_own_grace_period_overrides_the_pods
test_log_rotation_creates_a_rotated_file
test_losing_every_backend_removes_the_dnat_rule
test_memory_manager_pins_guaranteed_containers_to_a_numa_node
test_metrics_cadvisor_returns_real_container_usage
test_mount_propagation_host_to_container_sees_a_new_host_mount
test_mount_propagation_host_to_container_still_mounts_normally
test_mount_propagation_private_default_does_not_see_a_new_host_mount
test_multiple_backends_actually_share_traffic
test_namespace_controller_deletes_contents_before_finalizing
test_node_allocatable_cgroup_exists_and_is_capped
test_node_gets_a_pod_cidr_allocated
test_node_is_tainted_unreachable_after_heartbeat_loss_and_recovers
test_node_reports_hugepages_capacity_when_reserved
test_node_reports_volumes_in_use_for_a_csi_volume
test_nodeapiserver_applies_configured_node_selector
test_nodeapiserver_applies_pure_admission_to_apply
test_nodeapiserver_audits_rejected_requests
test_nodeapiserver_audits_request_and_response_objects
test_nodeapiserver_authentication_modes
test_nodeapiserver_enforces_mountable_secrets_for_ephemeral_containers
test_nodeapiserver_enforces_node_restriction
test_nodeapiserver_enforces_service_account_mountable_secrets
test_nodeapiserver_honors_authorization_webhook_decisions
test_nodeapiserver_honors_generate_name
test_nodeapiserver_proxy_subresources_relay_requests
test_nodeapiserver_runs_webhook_for_delete_collection
test_nodeapiserver_supports_crd_selectable_fields
test_nodeapiserver_watches_partial_object_metadata
test_nodeport_service_is_reachable_on_the_node_ip
test_nodeproxy_rebuilds_the_whole_ruleset_after_a_restart
test_orphaned_sandbox_gc_reaps_a_pod_deleted_while_nodelet_is_down
test_pod_cgroup_reflects_its_qos_class
test_pod_condition_reports_observed_generation
test_pod_exceeding_its_active_deadline_is_terminated
test_pod_mounts_a_persistent_volume_claim
test_pod_resources_grpc_query_returns_real_data
test_pod_status_reports_host_ips_plural
test_pod_status_reports_qos_class
test_pod_uses_a_raw_block_volume
test_pod_with_an_attach_required_pvc_waits_for_volumeattachment
test_prestop_hook_runs_before_termination
test_projected_volume_merges_configmap_and_downward_api
test_proc_mount_unmasked_leaves_proc_kcore_readable
test_readiness_probe_gates_ready_condition
test_recursive_read_only_enabled_blocks_writes_in_a_nested_mount_too
test_recursive_read_only_if_possible_falls_back_without_erroring
test_restart_policy_never_exit_nonzero_is_failed
test_restart_policy_never_exit_zero_is_succeeded
test_run_as_user_is_applied
test_runtime_class_handler_is_honored
test_scheduler_consults_an_http_extender_and_honours_a_filter_rejection
test_scheduler_does_not_preempt_when_policy_forbids_it
test_scheduler_honours_pod_anti_affinity
test_scheduler_resolves_a_namespace_selector_against_real_labels
test_scheduler_schedules_a_pod_an_http_extender_approves
test_service_with_no_endpoints_does_not_wedge_the_ruleset
test_session_affinity_pins_a_client_to_one_backend
test_set_hostname_as_fqdn_reports_the_full_fqdn
test_share_process_namespace_puts_every_container_in_one_pid_namespace
test_startup_probe_failure_past_threshold_restarts_the_container
test_startup_probe_gates_liveness_and_readiness
test_static_pod_creates_a_mirror_pod
test_stats_summary_returns_real_pod_usage
test_sub_path_expr_expands_a_downward_api_env_var
test_supplemental_groups_policy_strict_ignores_image_group_membership
test_sysctls_are_applied_to_the_sandbox
test_termination_grace_period_is_honored_not_instant
test_termination_message_path_is_read_back_into_container_status
test_the_cluster_tolerates_a_slow_link
test_the_node_still_reconciles_pods_after_an_apiserver_restart
test_topology_manager_cross_provider_alignment_manual_note
test_topology_manager_does_not_reject_pods_on_a_single_numa_node_host
test_topology_manager_restricted_does_not_reject_pods_on_a_single_numa_node_host
test_topology_manager_restricted_spread_manual_note
test_unreferenced_image_is_not_removed_below_the_watermark
test_upgrade_a_dead_member_leaves_nothing_listening_behind_it
test_a_follower_forwards_writes_to_the_leader
test_a_long_lived_watch_survives_a_service_churn_burst
test_the_cluster_tolerates_a_slow_link (dup, see above)
test_datastore_lists_a_prefix_in_key_order
test_datastore_refuses_a_read_below_the_compaction_point
test_without_read_only_root_filesystem_writes_succeed
test_wrong_port_readiness_probe_keeps_pod_not_ready
test_a_partitioned_leader_steps_down_and_the_majority_elects_another
```

## Working protocol for this document

1. Pick a root cause above with **Status: NOT STARTED** and no active
   branch/PR (`gh pr list --state open`, `git branch -a`) already covering
   it — check for Codex's in-flight work first.
2. Branch off current `origin/nodeapiserver` HEAD (not the stale SHA the
   failing run used).
3. Reproduce narrowly: `./deploy/test-e2e.sh --only=<test_name>` locally,
   or `gh workflow run e2e.yml --ref <branch> -f only=<test_name>`.
4. Fix, add/extend a regression test per the merge protocol in
   `CLAUDE.md`.
5. `quick-check.yml -f components=nodeapiserver` (or the relevant crate),
   then the targeted `e2e.yml` run for the fixed test **and** its shard's
   other previously-failing tests (they should go green too if the cascade
   theory holds — that's the actual proof the root cause was right, not
   just a coincidental fix).
6. Open the PR against `nodeapiserver`, update this doc's status line for
   that root cause to the PR number, then move to the next one.
7. Periodically re-fetch `origin/nodeapiserver` — Codex is landing PRs on
   it concurrently; rebase in-flight branches rather than letting them
   drift.
