# nodeapiserver e2e fix tracker

This is the living log for failures found while bringing `nodeapiserver` to
parity with the upstream kube-apiserver. Each independent defect gets its own
branch and PR into `nodeapiserver`; a PR is not marked fixed until its focused
e2e test passes. Failures that occur after the API listener is lost are marked
for triage rather than assumed to be separate defects.

Initial evidence: [full e2e run 33541038722](https://github.com/centerionware/not-k8s/actions/runs/33541038722), commit `6c6c0060`.

| ISSUE | STATUS | EVIDENCE / NEXT TEST | PR |
| --- | --- | --- | --- |
| `/metrics` metrics access and e2e authentication | verified | The original anonymous curl was not upstream-compatible: release-1.34 protects `/metrics` with the `system:monitoring` RBAC group. The bootstrap manifest was also missing that upstream role and binding. PR #487 restores the policy and makes both focused checks use the authenticated admin kube client. Runs `33554455305` (quick-check) and `33554455419` (focused e2e, shards 1 and 5) passed. | [#487](https://github.com/centerionware/not-k8s/pull/487) |
| API/client recovery after environment reconfiguration | verified | PR #488 classifies the authorization-webhook fixture as disruptive, rebuilds the kube-rs client after the service restart, and makes the recovery barrier fail the run when the API does not return. The first focused attempts still conflated namespace-controller ServiceAccount recovery with API recovery; the barrier now requires only a fresh authenticated API LIST, leaving controller/watch recovery for its own issue. The nodeapiserver listener also uses a lazy nodestore channel (`9932713f`) so a nodestore reconnect cannot block listener startup. Runs `33566352979` (nodeapiserver quick-check) and `33566353450` (focused authorization-webhook recovery e2e) passed. | [#488](https://github.com/centerionware/not-k8s/pull/488) |
| Node authorizer mirror-Pod denial response | verified (stale finding) | The current base already allows the node-authorizer mirror-Pod create/delete path and matches upstream NodeRestriction validation. Focused run `33568205267` passed `test_nodeapiserver_enforces_node_restriction` on shard 2 without a source change, so no PR was needed. | — |
| CSI and DRA workload startup | open | Shard 1 CSI tests and shard 2 DRA/raw-block/fsGroup tests timed out waiting for Pods. Re-run after API recovery is fixed to distinguish nodelet/runtime failures from cascade failures. | — |
| Namespace-selector ServiceAccount error | open | Shard 1: `test_scheduler_resolves_a_namespace_selector_against_real_labels` received a 403 for a missing default ServiceAccount. | — |
| Dry-run collection delete semantics | verified | The dedicated `deletecollection` listener path now parses `dryRun=All`, forwards it through admission and webhooks, and uses `delete_with_options` so selected objects are not persisted as deleted. Focused e2e `33569985186` passed `test_nodeapiserver_honors_dry_run_and_delete_preconditions` on shard 4; nodeapiserver quick-check `33569976266` also passed. | [#490](https://github.com/centerionware/not-k8s/pull/490) |
| Termination grace period | open | Shard 4: `test_termination_grace_period_is_honored_not_instant` observed the Pod disappear after about 20ms despite a 20s grace period. | — |
| Nodeproxy headless/ClusterIP routing | open | Shard 4: `test_headless_service_programs_no_rules_and_does_not_break_others` and `test_losing_every_backend_removes_the_dnat_rule` failed. Re-run independently after API recovery. | — |
| TLS bootstrap client certificate kubeconfig | open | Shard 1: `test_tls_bootstrap_issues_a_real_client_certificate` timed out waiting for nodelet's issued kubeconfig. | — |
| Remaining nodelet runtime, streaming, storage, and scheduler timeouts | triage | Many failures begin after the API listener/recovery failures. Reclassify only after the focused API recovery test is green. | — |
| Cert-manager CRD usability | triage | Shard 4's CRD test could not create its test namespace after API recovery failed; do not treat this run as evidence against CRD discovery. | — |
| Nodelet reconciliation after API restart | triage | Shards 2 and 4 could not create their test namespaces after API recovery failures; rerun as an isolated test later. | — |

## Run history

| RUN | RESULT | NOTES |
| --- | --- | --- |
| `33541038722` | failed | Build passed. All five shards failed; the metrics authorization defect was independently visible, while the API listener recovery failure caused broad cascades. |
| `33552707582` | failed | First PR #487 attempt: both selected metrics tests reached the endpoint but the runner's curl could not load the generated admin private-key PEM; no application request was made. |
| `33554455305` | passed | PR #487 focused nodebootstrap quick-check. |
| `33554455419` | passed | PR #487 focused e2e: `test_nodeapiserver_exposes_full_request_metrics` (shard 1) and `test_nodeapiserver_exposes_inflight_metrics` (shard 5) passed. |
| `33556584355` | failed | PR #488 first focused e2e: the authorization-webhook test passed, but its post-test recovery barrier timed out. |
| `33558611188` | failed | PR #488 quick-check caught the initial immutable-client assignment; fixed before the next run. |
| `33558610886` | failed | PR #488 e2e failed in `build-source-runtime` before the test ran. |
| `33558946274` | passed | PR #488 quick-check after making the recovered client replace the runner client. |
| `33558946279` | failed | PR #488 focused e2e still timed out in the recovery barrier while it required namespace-controller ServiceAccount readiness. |
| `33561364951` | passed | PR #488 quick-check after rebuilding a fresh client for every recovery attempt. |
| `33561364992` | failed | PR #488 focused e2e still timed out in the same combined API/controller recovery barrier; diagnostics showed the API serving later while controller watches were retrying. |
| `33563462734` | failed | Quick-check caught that the lazy-channel regression test needed a Tokio runtime; fixed before the next run. |
| `33563462906` | cancelled | The corresponding e2e attempt was cancelled after the quick-check failure. |
| `33563725791` | passed | PR #488 quick-check after moving the lazy-channel regression under `#[tokio::test]`. |
| `33563725832` | failed | Focused e2e reproduced the combined-barrier failure after the lazy listener change; the API later served requests, so the barrier was narrowed instead of adding a nodecontroller change. |
| `33566352979` | passed | PR #488 nodeapiserver quick-check for the final standalone recovery fix. |
| `33566353450` | passed | PR #488 focused e2e: `test_nodeapiserver_honors_authorization_webhook_decisions` passed and the fresh authenticated API recovery barrier completed. |
| `33568205267` | passed | Current-base focused e2e: `test_nodeapiserver_enforces_node_restriction` passed on shard 2; the previously reported mirror-Pod denial was not reproducible, so no source change or PR was needed. |
| `33569976266` | passed | PR #490 nodeapiserver quick-check. |
| `33569985186` | passed | PR #490 focused e2e: `test_nodeapiserver_honors_dry_run_and_delete_preconditions` passed on shard 4. |
