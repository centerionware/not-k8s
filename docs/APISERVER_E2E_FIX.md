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
| API/client recovery after environment reconfiguration | in progress | Shards 1–5 lose API availability during or after authorization/audit reconfiguration tests; later requests fail with connection refused or recovery timeout. The recovery barrier already exists but the newly-added authorization-webhook test was not classified as disruptive, recovery errors were only logged, and the runner reused a kube-rs client across the service restart. This PR classifies the fixture, rebuilds the client, and makes the barrier fail the focused run. | pending |
| Node authorizer mirror-Pod denial response | open | Shard 2: `test_nodeapiserver_enforces_node_restriction` received 403 while deleting the node's mirror Pod. | — |
| CSI and DRA workload startup | open | Shard 1 CSI tests and shard 2 DRA/raw-block/fsGroup tests timed out waiting for Pods. Re-run after API recovery is fixed to distinguish nodelet/runtime failures from cascade failures. | — |
| Namespace-selector ServiceAccount error | open | Shard 1: `test_scheduler_resolves_a_namespace_selector_against_real_labels` received a 403 for a missing default ServiceAccount. | — |
| Dry-run collection delete semantics | open | Shard 4: `test_nodeapiserver_honors_dry_run_and_delete_preconditions` observed a dry-run collection delete removing a ConfigMap. | — |
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
