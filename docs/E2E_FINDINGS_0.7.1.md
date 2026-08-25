# 0.7.1 Rust bootstrapper e2e findings

This file records the complete source-built baseline for the 0.7.1
nodebootstrap e2e migration. It is separate from
[`E2E_FINDINGS.md`](E2E_FINDINGS.md), which is the historical record of older
runs.

## Complete source-built baseline: run 32812857956

Run `32812857956` completed all five shards against the source-built combined
binary, but used the pre-artifact workflow revision `adc746ef`. It found 30
Rust test failures (6/6/5/6/7 by shard). Worker validation also ran on all
five shards and failed five times for the same workflow path bug: each step
still pointed at `target/debug/nodebootstrap` and `target/debug/notk8s`, even
though the source-built artifacts were installed elsewhere. Those five are
one CI defect, not five worker-runtime failures. The corrected workflow
downloads the shared artifact and runs that validation only on shard 5.

The completion column remains empty. A fix is complete only after the named
test passes in a later run against the corrected bootstrap path.

| Shard | Rust test or workflow check | Observed result | Classification | Required next action | Completed |
| --- | --- | --- | --- | --- | --- |
| 1 | `test_static_pod_creates_a_mirror_pod` | Mirror Pod was NotFound | Static-Pod reconciliation or fixture timing | Capture static-pod and mirror-watch state, then wait for the mirror by UID/name after nodelet reload | |
| 1 | `test_datastore_creates_a_key_only_if_absent` | Create-if-absent transaction returned `succeeded: false` | Nodestore transaction semantics | Verify revision-zero compare handling through the JSON/gRPC adapter | |
| 1, 2, 3 | Device-plugin capacity/allocation, health transition, preferred allocation/prestart | Fake device capacity never appeared in Node status | Nodelet device-plugin registration/watch integration | Capture plugin socket registration and ListAndWatch delivery before creating the Pod | |
| 1 | `test_csi_ephemeral_inline_volume_is_mounted` | Inline-volume Pod never reached Running | CSI inline-volume runtime path | Inspect nodelet CSI inline staging against the installed host-path driver | |
| 1 | `test_supplemental_groups_policy_strict_ignores_image_group_membership` | Merge-policy Pod timed out | CRI user/group setup or status observation | Capture containerd create/start and termination-log state for both group-policy Pods | |
| 1 | `test_pod_mounts_a_persistent_volume_claim` | PVC mount timed out | CSI/PVC mount integration | Compare this PVC path with the passing attach and `volumesInUse` tests | |
| 2 | `test_scheduler_wakes_a_pending_pod_on_a_real_event` | Scheduler blocker never became bound | Scheduler event/watch or test fixture | Capture scheduler queue, lease, and Node status at the resource-change event | |
| 2 | `test_device_plugin_health_transition_updates_allocated_resources_status` | Fake device capacity timed out | Device-plugin integration | Same plugin registration investigation as shard 1 | |
| 2 | `test_fsgroup_change_policy_on_root_mismatch_skips_the_second_chown` | First fsGroup Pod timed out | Volume ownership/runtime path | Capture CSI mount and fsGroup application errors before changing the assertion | |
| 2 | `test_client_certificate_authentication_works` | Connection refused on nodelet port 10250 | Nodelet serving configuration/restart | Verify the restarted service has the server enabled, port, and client CA drop-in | |
| 2 | `test_sysctls_are_applied_to_the_sandbox` | Sysctl value was not observed | CRI sandbox setup or host policy | Capture sandbox creation errors and explicitly report rejected sysctls | |
| 2 | `test_clusterip_is_reachable_from_inside_a_pod` | ClusterIP access timed out | Flannel/CNI/nodeproxy integration | Inspect CNI mode, EndpointSlice programming, and nftables rules on the same runner | |
| 3 | `test_tls_bootstrap_issues_a_real_client_certificate` | Nodelet never submitted a CSR | TLS-bootstrap fixture or nodelet bootstrap startup | Include the child nodelet log in the failure and verify the bootstrap kubeconfig is consumed | |
| 3 | `test_statefulset_creates_ordinal_pods_and_scales_down_highest_first` | OrderedReady created ordinal 1 before ordinal 0 was Ready | StatefulSet controller ordering or assertion race | Record Pod creation timestamps and readiness transitions before changing ordering logic | |
| 3 | `test_unreferenced_image_is_not_removed_below_the_watermark` | Pulled image was not retained | Containerd image namespace/GC setup | Verify the image is pulled and queried in the nodelet's CRI namespace | |
| 3 | `test_device_plugin_preferred_allocation_and_prestart` | Fake device capacity timed out | Device-plugin integration | Same plugin registration investigation as shard 1 | |
| 3 | `test_image_volume_source_mounts_a_read_only_image` | Image-volume Pod timed out | Image-volume runtime or image availability | Capture image pull, unpack, and read-only mount errors | |
| 4 | `test_datastore_refuses_a_read_below_the_compaction_point` | `invalid digit found in string` | Confirmed e2e JSON numeric conversion bug | Preserve the numeric revision when converting the Status response | |
| 4 | `test_image_gc_removes_unreferenced_images_above_the_watermark` | Pulled image was not retained | Containerd image namespace/GC setup | Make the image-GC fixture deterministic and verify the configured namespace | |
| 4 | `test_host_network_pod_uses_the_node_network_namespace` | Pod IP did not match Node InternalIP | Host-network/runtime or node-address detection | Capture both addresses and the selected network interface | |
| 4 | `test_set_hostname_as_fqdn_reports_the_full_fqdn` | Full FQDN was not observed before timeout | Hostname/domain setup or timing | Set a stable runner hostname/domain and inspect the container hostname | |
| 4 | `test_credential_provider_supplies_auth_for_an_otherwise_rejected_pull` | Containerd had no CRI registry section | Confirmed bootstrap containerd configuration gap | Install a writable CRI registry config with credential-provider settings | |
| 4 | `test_kubectl_attach_streams_the_containers_stdout` | Later stream line never arrived | Attach streaming or test timing | Preserve the initial and later-line evidence and inspect stream closure | |
| 5 | `test_image_pull_policy_never_fails_when_image_is_absent` | `ErrImageNeverPull` was not observed | Runtime/status timing or image cleanup | Prove absence in the configured containerd namespace before creating the Pod | |
| 5 | `test_host_aliases_still_work_under_host_users_false` | Pod timed out | User-namespace hosts-file setup | Capture sandbox/userns and `/etc/hosts` materialization errors | |
| 5 | `test_host_path_directory_type_rejects_a_nonexistent_path` | Directory rejection was not observed | Runtime/status timing | Prove the path is absent and capture the mount error in Pod status | |
| 5 | `test_run_as_user_is_applied` | Termination message was not observed | CRI security-context or log collection | Capture container exit and termination-log state | |
| 5 | `test_host_users_volume_ownership_translation_is_correct` | Ownership translation was not observed | User-namespace/volume setup | Capture subordinate-ID mapping and hostPath ownership before changing the check | |
| 5 | `test_lifecycle_stop_signal_is_honored_by_the_runtime` | `stop-signal-check` was NotFound | Test cleanup/race or Pod lifecycle | Preserve the Pod object/status before querying the stop result | |
| 5 | `test_kubectl_port_forward_reaches_a_real_container_port` | Port-forward response was empty | Port-forward stream/runtime path | Capture nodelet server and CRI stream errors | |
| 1-5 | Worker validation without flannel or proxy | All five checks failed before bootstrap because the workflow referenced missing `target/debug/*` paths | Confirmed CI workflow defect, duplicated five times | Use the downloaded combined artifact and run this check once, on shard 5 | |

## Warnings and skips from the same baseline

| Observation | Evidence | Classification | Required action | Completed |
| --- | --- | --- | --- | --- |
| `Server cert bypassed` | Emitted by the throwaway nodestore-backed apiserver watch test on shards 3 and 4 | Test-fixture TLS warning, not a cluster certificate failure | Generate a serving CA/certificate for the fixture and configure the client to trust it | |
| Metrics tests skipped | TokenRequest used `expirationSeconds: 300`, rejected by the current API because the minimum is 600 seconds | Confirmed test compatibility defect | Request a 600-second token | |
| Throwaway apiserver download skipped | `dl.k8s.io` connection reset during TLS setup on shard 1 | External runner/network failure | Keep this as a clean skip or provide the asset through CI setup; do not call it a nodebootstrap failure | |
| Nodeproxy/nftables tests skipped | Runner lacked the required nftables capabilities | Runner capability gap | Run proxy coverage on a capable runner; do not force flannel off to compensate | |
| Repeated `Expired` watch and CRI `No such file or directory` warnings near teardown | Occurred after tests had already created terminating namespaces and while nodelet/containerd were being restarted | Likely lifecycle cleanup cascade; not yet a root-cause finding | Fix/verify Pod and namespace cleanup before attributing later timeouts to these warnings | |
