---
name: not-k8s-stabilize
description: Diagnose and fix not-k8s CI or real-cluster failures, preserve Kubernetes semantics, and carry an authorized stabilization branch through CI verification. Use for apiserver bring-up, recurring e2e failures, and controller/watch convergence bugs.
---

# Stabilize not-k8s

Read the repository's [AGENTS.md](../../../AGENTS.md) and the user's selected
handoff first. This workflow does not authorize a merge, release, deployment,
or message that the user has not placed in scope. Keep their chosen PR, test
target, run cadence, and terminal condition. Do not split an explicitly shared
integration PR into a new branch per failure.

## Establish the evidence

Record the actual checkout, branch, HEAD, PR base/head, and tested SHA.
Handoff hypotheses guide searches; confirm them against code and completed-run
logs before adopting a fix. Preserve useful existing work.

For each failure, identify:

- The first failing operation, its error/status, and its timestamp.
- Whether the test reached its body or failed during namespace/driver setup.
- The component that owns the missing behavior, including upstream/reference
  driver involvement. A timeout names an observation, not a root cause.
- What happened immediately before it: a restart, CRD removal, controller
  status write, dependency arrival, or watch/list boundary.

Group downstream failures only when the evidence supports a shared cause.
For example, several missing default ServiceAccounts plus a stopped root-CA
publisher suggest the shared Namespace watch; they do not prove that watch
ended. A systemd unit remaining active does not prove all controller tasks live.

Search saved logs locally with `rg`. Keep observations, hypotheses, and
confirmed mechanisms distinct in notes and updates. If the journal tail omits
the failure interval, say the evidence is missing. Obtain better diagnostics
on the next authorized run instead of treating silence as proof.

## Repair the producer of the wrong behavior

| Symptom | Check before editing the test |
| --- | --- |
| PATCH returns 409 during status writes | Did the caller supply resourceVersion or a JSON Patch test? If not, trace server read/merge/admission/CAS retries. Recompute from fresh state on an internal race. |
| PUT returns 409 | Refresh the object and the caller's version precondition before retrying. Resending the same stale body cannot resolve it. |
| UID-scoped DELETE returns 409 | Separate a changed UID from a status-write CAS race. Rechecking the original UID preserves replacement safety while allowing retries. |
| Object never converges | Find the event that should enqueue reconciliation. A failed write produces no new object event; an independent informer can arrive later. |
| New namespaces lack controller-owned objects | Trace Namespace events, shared subscriptions, controller task lifetime, queue/permit ownership, and timed-out creates. Do not create default SAs in the harness. |
| Deleted owner/child reappears | Check stale owner caches, deletionTimestamp, UID identity, delete conflicts, and GC's retry path. A longer timeout does not repair recreation. |
| Watch misses an object | Check snapshot/subscription ordering, replay, bookmark guarantees, same-revision batches, relist/lag recovery, and whether readiness updates survive zero subscribers. |

Read the complete relevant function and its callers. Server storage retries
must preserve validation/admission and explicit preconditions. Controller
retries must be keyed and bounded/backed off. Do not add a tight global poll,
relax authorization, discard resourceVersion, or hide unexpected API errors.

Write a regression at the failure boundary. For an ordering bug, explicitly
arrange the adverse order or use a deterministic barrier; do not depend on an
arbitrary sleep in a unit test. For a real-container failure, the final assertion
must still exercise real containerd/CNI/CSI/DRA as applicable. Test changes
must preserve what the feature promises, not merely make the observed error
disappear. Keep failure cleanup and useful diagnostics.

## Run the validation loop

Use the [CI skill](../not-k8s-ci/SKILL.md) for dispatch, exact run identity,
watch cadence, completed-log capture, and artifact handling. Start the
authorized validation pair after the fix is committed and pushed. Use its
completed results to repeat this diagnostic loop until the requested full
suite passes or an external blocker prevents further authorized progress.

## Finish with verifiable status

Report the behavior fixed, tested commit, exact CI links, full e2e outcome,
and remaining gates. A correctness result is not a CPU/RSS benchmark.
Performance claims need equivalent workload, build profile, architecture,
measurement window, and component scope.

Keep handoffs concise: checkout/branch/PR, constraints, last pushed SHA,
exact active/completed runs, saved logs, proven failures versus hypotheses,
and the next action. Never mark unrun tests or unobserved recovery as passing.
Follow AGENTS.md and the user's explicit merge order; stabilization does not
by itself authorize merging.
