# September 8 stabilization investigation

PR #574 (`perf/stack-load-profiling`, base `nodeapiserver`), investigated
commit `7fc4e8d60ce090fa67da0d9857373bad6aaf2bf7`.
Of six full runs, 34179745718, 34179755858, 34180559916 and 34180578582
passed; 34179750501 and 34180545057 failed. This is not a claim that the
patch below has passed runtime validation.

## Failed runs and evidence

- [34179750501](https://github.com/centerionware/not-k8s/actions/runs/34179750501),
  shard 1: the reference DRA driver repeatedly rejected the API certificate
  starting at 02:32:01 UTC (`x509: certificate signed by unknown authority`).
  The API logged matching BadCertificate handshake errors. Driver readiness
  failed at 02:33:04 before the e2e runner started. The namespace CA publisher
  had already created the bundle at 02:31:55.506. Nodelet nevertheless
  truncated and rewrote projected files on every reconciliation, including
  unchanged CA data. A reader could observe an empty/partial trust bundle.
  The failing process's exact CA bytes were not captured: attribution of this
  TLS incident to the empty-read window remains an inference, not direct proof.
  The inode mutation defect is deterministic and now has focused and
  real-container regressions. Publish complete files with atomic rename and
  leave unchanged files alone; preserve permissions and ownership on refresh.
- [34180545057](https://github.com/centerionware/not-k8s/actions/runs/34180545057),
  shard 5: CoreDNS test setup started at 02:47:31.415 and failed after
  38.096 seconds waiting for its namespace's default ServiceAccount.
  The new controller acquired leadership at 02:47:38.160; its initial
  Namespace LIST at 02:47:38.172 did not finish before its 30-second timeout.
  Initialization succeeded on retry at 02:48:09.176, after setup had failed.
  Bootstrap's old readiness check accepted persisted leases and existing
  ServiceAccounts, neither of which proves the new controller initialized.
  Readiness now requires controllers to populate a newly created namespace
  with a default account and the expected CA, with bounded requests and
  UID-preconditioned cleanup. The restart e2e also checks fresh namespace
  reconciliation. The first LIST's precise transport stall is not proven by
  the available logs; it must not be reported as conclusively explained.

## Other actionable log errors

- Historical builds used Hyper 1.10.1. Its HTTP/1 write recheck can buffer
  watch EOF without flushing it; [upstream fix #4143](https://github.com/hyperium/hyper/pull/4143)
  is included in 1.11.1. Require that version and exercise the actual duplex
  transport's end-of-body flush, retaining existing recovery safeguards.
- Diagnostic SelfSubjectAccessReviews and cert-manager Secret deletes
  returned HTTP 400 because virtual-resource handlers did not decode
  Kubernetes protobuf. Route reviews and DeleteOptions through the existing
  codec, validate protobuf type metadata, and test both unit decoding and
  real API requests.
- Helm's Secret PUT returned HTTP 400. Its storage driver submits a new
  Secret body without resourceVersion. Upstream SecretStrategy permits
  unconditional updates. Accept omitted/empty versions for Secrets while
  preserving storage CAS, explicit version conflicts, and UID checks.
  The real API regression checks replacement, stale-version/UID rejection,
  and protobuf deletion.
- During the intentional API restart, nodeproxy emitted hundreds of
  connection errors because both resource watches retried without backoff.
  Apply kube-runtime's default backoff to both streams and test a refused
  API connection. A bounded retry warning during downtime is still expected.
- Build logs warned that the artifact actions still targeted deprecated
  Node 20. Update build/e2e upload and download actions to their supported
  Node 24 versions, preserving artifact names and archive behavior.

Sources for Secret behavior:
[Kubernetes Secret strategy](https://github.com/kubernetes/kubernetes/blob/v1.34.0/pkg/registry/core/secret/strategy.go),
[Helm Secret driver](https://github.com/helm/helm/blob/main/pkg/storage/driver/secrets.go).

The inspected journals also include intentional negative-test errors
(invalid projected inputs, missing host paths, probes, and unavailable node
runtime), cleanup NotFound responses, caller-precondition conflicts, and
namespace-termination admission denials. These must not be suppressed merely
to make a log search empty. Environment-gated skips are not passes.

## Saved diagnostics and validation

Complete failed-job logs, build log, and extracted gzip journals are saved
locally under `/tmp/not-k8s-e2e-investigation/`; original journals/results are
on `e2e-results` under the run/attempt directories. No local Cargo builds,
tests, or real-cluster runs were performed. Quick-check must cover nodelet
(including CRI), nodeproxy, nodeapiserver and nodebootstrap. After it passes,
dispatch three full unfiltered nodeapiserver e2es as requested; the user will
monitor those runs. Record final SHA and run links in the handoff.
