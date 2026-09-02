# nodeapiserver e2e fix tracker

This is the living log for failures found while bringing `nodeapiserver` to
parity with the upstream kube-apiserver. Each independent defect gets its own
branch and PR into `nodeapiserver`; a PR is not marked fixed until its focused
e2e test passes. Failures that occur after the API listener is lost are marked
for triage rather than assumed to be separate defects.

Initial evidence: [full e2e run 33541038722](https://github.com/centerionware/not-k8s/actions/runs/33541038722), commit `6c6c0060`.

## Priority order

| PRIORITY | ISSUE | STATUS |
| --- | --- | --- |
| 1 | API listener availability after a failure or reconfiguration | Recovery fix verified in PR #488; keep this as the release-blocking regression gate. |
| 2 | EndpointSlice/Service watch handoff and nodeproxy routing | Nodeapiserver LIST and all-namespaces WATCH pass; the remaining ClusterIP routing failure is outside this API branch and needs a separate nodeproxy investigation. |
| 3 | CRD discovery and cert-manager usability | Unverified on the current triage run; isolate after the EndpointSlice path. |
| 4 | Nodelet reconciliation after API/runtime restart | Unverified on this branch; keep separate from API work until an isolated failure is reproduced. |
| 5 | TLS bootstrap and remaining compatibility gaps | Open or triage; address after the higher-impact runtime paths. |
| 6 | Large-file refactor | Maintainability work; separate PR after runtime defects, with no single source file over 500 lines. |
| 7 | Termination grace-period timing | Lower priority and outside nodeapiserver; the API metadata assertion already passes. |

| ISSUE | STATUS | EVIDENCE / NEXT TEST | PR |
| --- | --- | --- | --- |
| `/metrics` metrics access and e2e authentication | verified | The original anonymous curl was not upstream-compatible: release-1.34 protects `/metrics` with the `system:monitoring` RBAC group. The bootstrap manifest was also missing that upstream role and binding. PR #487 restores the policy and makes both focused checks use the authenticated admin kube client. Runs `33554455305` (quick-check) and `33554455419` (focused e2e, shards 1 and 5) passed. | [#487](https://github.com/centerionware/not-k8s/pull/487) |
| API/client recovery after environment reconfiguration | verified | PR #488 classifies the authorization-webhook fixture as disruptive, rebuilds the kube-rs client after the service restart, and makes the recovery barrier fail the run when the API does not return. The first focused attempts still conflated namespace-controller ServiceAccount recovery with API recovery; the barrier now requires only a fresh authenticated API LIST, leaving controller/watch recovery for its own issue. The nodeapiserver listener also uses a lazy nodestore channel (`9932713f`) so a nodestore reconnect cannot block listener startup. Runs `33566352979` (nodeapiserver quick-check) and `33566353450` (focused authorization-webhook recovery e2e) passed. | [#488](https://github.com/centerionware/not-k8s/pull/488) |
| Node authorizer mirror-Pod denial response | verified (stale finding) | The current base already allows the node-authorizer mirror-Pod create/delete path and matches upstream NodeRestriction validation. Focused run `33568205267` passed `test_nodeapiserver_enforces_node_restriction` on shard 2 without a source change, so no PR was needed. | — |
| CSI workload startup | verified | Clean-base focused run `33576356564` passed `test_csi_ephemeral_inline_volume_is_mounted` on shard 1, including fresh bootstrap and reference CSI/DRA driver setup. The earlier timeout was not reproducible on the recovered nodeapiserver base; no API source change was needed. | — |
| DRA API discovery | verified | Clean-base focused run `33577664800` passed `test_resource_api_group_is_enabled` on shard 1 with the reference drivers installed, confirming that nodeapiserver advertises `resource.k8s.io/v1` ResourceClaims. | — |
| DRA claim allocation and workload startup | verified | Clean-base focused run `33579027378` passed `test_dra_claim_is_allocated_and_reserved_for_the_pod` on shard 2 with the reference DRA driver installed, confirming claim allocation, Pod startup, allocation status, and reservation. The earlier timeout was not reproducible on the recovered nodeapiserver base; no API source change was needed. | — |
| Namespace-selector ServiceAccount error | triage (controller timing) | Isolated run `33575004375` reproduced the 403 because the helper Namespace's `default` ServiceAccount had not yet been created. This is the expected ServiceAccount admission behavior; the remaining race belongs to the service-account controller or fixture readiness, not nodeapiserver authorization. No API PR was opened. | — |
| Dry-run collection delete semantics | verified | The dedicated `deletecollection` listener path now parses `dryRun=All`, forwards it through admission and webhooks, and uses `delete_with_options` so selected objects are not persisted as deleted. Focused e2e `33569985186` passed `test_nodeapiserver_honors_dry_run_and_delete_preconditions` on shard 4; nodeapiserver quick-check `33569976266` also passed. | [#490](https://github.com/centerionware/not-k8s/pull/490) |
| Termination grace period | triage (nodelet/runtime) | The focused nodeapiserver run `33573473607` used a strengthened assertion that passed after DELETE: the API returned a Pod with `deletionTimestamp` and `deletionGracePeriodSeconds=8`. The same test then observed nodelet/CRI remove it after 48ms. The relevant teardown code is unchanged from `main`, so closed PR #491 without merge; the remaining timing failure is outside this API branch. | — |
| Nodeproxy headless/ClusterIP routing | nodeapiserver verified; nodeproxy follow-up | Focused run `33599414807` passed the explicit ready EndpointSlice LIST and the all-namespaces live WATCH assertion, then timed out only at normal ClusterIP traffic. The retained failure snapshot shows nodeproxy active with no watch error and only the built-in nft rules; no nodeapiserver source fix remains justified. PR #497 should be closed or converted to evidence-only, with any routing fix on a separate nodeproxy branch. | [#497](https://github.com/centerionware/not-k8s/pull/497) |
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
| `33571657597` | failed | PR #491 first focused e2e reached the test but observed Pod removal after 1.046s; the API fix was not sufficient to establish the runtime grace interval. |
| `33573269584` | passed | PR #491 nodeapiserver quick-check after aligning the deletion timestamp with upstream. |
| `33573271801` | failed | PR #491 e2e failed during source compilation because the strengthened test omitted the `anyhow::Context` import; no behavior was exercised. |
| `33573471045` | passed | PR #491 nodeapiserver quick-check after fixing the test import. |
| `33573473607` | failed | PR #491 focused e2e reached the test; the API deletion metadata assertion passed, but nodelet/CRI removed the Pod after 48ms. PR #491 was closed without merge because this remaining failure is outside nodeapiserver. |
| `33575004375` | failed | Clean-base focused e2e: `test_scheduler_resolves_a_namespace_selector_against_real_labels` reached Pod creation but received the expected 403 for a missing default ServiceAccount in its newly created helper Namespace; classified as service-account controller/fixture timing, not an API defect. |
| `33576356564` | passed | Clean-base focused e2e: `test_csi_ephemeral_inline_volume_is_mounted` passed on shard 1 after the reference CSI/DRA drivers were installed; this verified the isolated CSI startup path without a nodeapiserver change. |
| `33577664800` | passed | Clean-base focused e2e: `test_resource_api_group_is_enabled` passed on shard 1 after the reference CSI/DRA drivers were installed; nodeapiserver advertised the ResourceClaim API and no source change was needed. |
| `33579027378` | passed | Clean-base focused e2e: `test_dra_claim_is_allocated_and_reserved_for_the_pod` passed on shard 2 after the reference CSI/DRA drivers were installed; allocation and reservation completed without a nodeapiserver change. |
| `33580340080` | failed | Focused nodeproxy run on the clean nodeapiserver base: `test_headless_service_programs_no_rules_and_does_not_break_others` timed out because nodecontroller's new EndpointSlice server-side-apply PATCHes returned HTTP 404. Existing `kube-dns` EndpointSlice apply returned 200 in the same run. |
| `33584791114` | failed | PR #497 diagnostic run: the focused headless-service test reproduced the EndpointSlice 404. The temporary diagnostics showed no initial SSA resource-resolution failure; the namespace-admission warning appeared only after test teardown, confirming the original request had fallen through ordinary PATCH and hit the missing-object 404 path. |
| `33588192416` | failed | PR #497 diagnostic rerun after adding the absent-header SSA fallback: build and bootstrap passed, but `test_headless_service_programs_no_rules_and_does_not_break_others` still timed out. A later run proved the EndpointSlice requests already carried `application/apply-patch+yaml` and were classified as SSA, so this was not an unrecognized-header failure. |
| `33589443041` | failed | PR #497 quick-check caught an over-broad SSA fallback regression: the new unit assertion showed that a normal `application/merge-patch+json` request with `fieldManager` and `force` must remain an ordinary merge patch. No runtime behavior was exercised. |
| `33589735363` | passed | PR #497 quick-check after narrowing the fallback to the actual apply-only query shape. |
| `33589951136` | failed | PR #497 focused e2e after the narrowed fallback: build, bootstrap, and the selected test setup passed, but `test_headless_service_programs_no_rules_and_does_not_break_others` timed out waiting for the normal ClusterIP beside the headless Service. Diagnostics showed the EndpointSlice PATCHes had `Content-Type: application/apply-patch+yaml` and `apply_request=true`; post-cleanup 404s are not the initial cause. The test snapshot had no test ClusterIP nft rule and nodeproxy had no watch error. |
| `33591946625` | failed | PR #497 diagnostic quick-check failed to compile because the temporary persistence log borrowed `context.key` after `into_bytes()` moved it. No tests ran; fixed by cloning the key for the PutRequest. |
| `33592332954` | failed | PR #497 focused e2e after the diagnostic compile fix: build, bootstrap, and the selected test setup passed, but `test_headless_service_programs_no_rules_and_does_not_break_others` timed out waiting for the normal ClusterIP beside the headless Service. The compact audit records only show post-timeout EndpointSlice 404s after namespace cleanup; the initial apply/cache/watch handoff remains unproven. |
| `33593610706` | passed | PR #497 focused nodeapiserver quick-check after bounding the temporary diagnostics. |
| `33593828918` | failed | PR #497 focused e2e with bounded diagnostics: build, bootstrap, and the selected test setup passed, but the same normal ClusterIP traffic wait timed out. The diagnostic lines were still outside the shard's retained service-log tail, so the initial EndpointSlice apply/cache/watch handoff remained unproven. |
| `33595273761` | passed | PR #497 focused nodeapiserver quick-check after adding stage-level apply tracing. |
| `33595560126` | failed | PR #497 focused e2e with bounded stage tracing: bootstrap passed and the same normal ClusterIP traffic wait timed out. The result motivated an explicit ready EndpointSlice wait in the test before the traffic assertion; no source fix is claimed from this run. |
| `33596833290` | failed | PR #497 focused e2e after adding the explicit ready EndpointSlice wait: the wait passed and the test still timed out only at the normal ClusterIP traffic assertion. This proves nodeapiserver can serve a ready EndpointSlice; no source fix is claimed yet. |
| `33599414807` | failed | PR #497 built successfully and the focused test passed both the ready EndpointSlice LIST and the all-namespaces live WATCH assertion, then timed out only at normal ClusterIP traffic. The retained snapshot showed nodeproxy active with only built-in nft rules and no nodeproxy watch error; no nodeapiserver source fix is claimed. |
