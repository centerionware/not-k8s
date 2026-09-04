# Working on not-k8s

This is the canonical repository guide for Codex and Claude Code.
`CLAUDE.md` points here; keep project instructions in one place.

## Objective and priorities

Replace k3s entirely with our own Kubernetes stack in Rust. Minimum
compatibility for real workloads is being established; do not claim full
Kubernetes parity from the project's ambition. Correct behavior against
real infrastructure comes first; reduce memory and CPU cost without weakening
that behavior. Rust alone is not evidence of a performance advantage.

The stack includes `nodeapiserver`, `nodestore`, `nodescheduler`,
`nodecontroller`, `nodelet`, `nodeproxy`, and `nodebootstrap`.
The `nodeapiserver` integration branch is stabilizing the replacement API
server. Upstream kube-apiserver is an explicit comparison target. Historical
k3s bootstrap descriptions are not the intended architecture.

After stabilization, the intended next target is Kubernetes 1.37 with broad
feature compatibility and measured CPU/memory advantages. That roadmap does
not authorize a version bump during a bug-fix task. Use the
[upgrade skill](.agents/skills/not-k8s-upgrade/SKILL.md) for version changes and
the [performance skill](.agents/skills/not-k8s-performance/SKILL.md) for efficiency
work. Report verified conformance and remaining feature gaps separately.

## Start here, before changing anything

1. Identify the actual checkout with `pwd`, `git status --short`,
   `git branch --show-current`, `git log -5 --oneline`, and
   `git remote -v`. The shell's starting directory may be an unrelated
   branch; a handoff may name a separate clone or worktree.
2. Read the handoff the user names. Preserve its accepted constraints,
   outstanding work, exact run IDs, and saved-log locations. Treat its
   proposed causes as hypotheses, not confirmed diagnoses. An untracked
   handoff stays untracked.
3. Verify the selected PR's base, head, and state before pushing or
   dispatching. Keep the user's chosen branch/PR arrangement. Never commit
   to `main`; separate unrelated concerns unless the user has requested
   one integration PR. Preserve existing changes.
4. Search current code with `rg` before deciding a feature is missing.
   Read the relevant status entry and failure history using the map below.
   A design document, an old comment, or a green unit test does not prove
   current runtime compatibility.
5. For stabilization or failed CI, read the repository
   [stabilization skill](.agents/skills/not-k8s-stabilize/SKILL.md).
   For CI selection, dispatch, monitoring, or artifacts, use the
   [CI skill](.agents/skills/not-k8s-ci/SKILL.md). Load only the workflow needed.

User instructions and accepted session decisions override repository defaults.
Do not ask again for permission already given. Do not infer authority to merge,
publish a release, restart an unrelated host, or send a message from a request
to investigate a failure.

## Commit messages

Use Conventional Commits: `type(scope): description` (scope is optional),
for example `fix(gc): preserve owner edges during relists`. The entire subject
line, including type, scope, punctuation, and spaces, must be **under 100
characters** (99 maximum), or the automated commit-convention check fails.
Put additional explanation in the body after a blank line. Apply the same
convention to squash-merge subjects and PR titles used as commit subjects.

## Find the implementation

| Concern | Start here |
| --- | --- |
| API behavior, admission, REST, watch | `crates/nodeapiserver/src/server/`, `src/cacher/`; `docs/APISERVER.md`, `docs/APISERVER_PLAN.md`, `docs/APISERVER_E2E_FIX.md` |
| Datastore semantics and ordering | `crates/nodestore/src/command.rs`, `store.rs`, `consensus.rs`; replication in `replication/` |
| Scheduling and workload controllers | `crates/nodescheduler/src/`, `crates/nodecontroller/src/controllers/`; `docs/SCHEDULER.md`, `docs/CONTROLLER_MANAGER.md` |
| Node agent and real containers | `crates/nodelet/src/`, `src/runtime/cri/`; `docs/ARCHITECTURE.md`, `docs/GAP_CLOSURE.md` |
| Service networking | `crates/nodeproxy/src/svc.rs` |
| Installation, PKI, services, builds | `crates/nodebootstrap/src/lib.rs`, `config.rs`, `components.rs`, `fetch.rs`, `service_mgr.rs`; `docs/NODEBOOTSTRAP_PLAN.md` |
| Real-cluster tests | `crates/nodebootstrap/src/e2e/mod.rs`, `e2e/tests/`; dispatch and environment setup in `.github/workflows/e2e.yml` |
| Past live failures | `docs/E2E_FINDINGS.md`, `docs/E2E_FINDINGS_0.7.1.md`, and the component's status document |

Read selected sections, not every large document on every task.
`docs/ARCHITECTURE.md` primarily describes nodelet, not the entire distribution.
Check that any command/path exists on the selected branch before using it.
The current Rust bootstrap CLI uses `--e2e`, `--only=pattern1,pattern2`,
and `--e2e-list`. Its parser/help lives in `nodebootstrap/src/lib.rs`.
Do not copy retired `deploy/bootstrap-source.sh`, `deploy/test-e2e.sh`,
or `--with-cri` invocations into a Rust-bootstrap checkout.

## Invariants that must survive a fix

When implementing or refactoring Rust, use the
[Rust implementation skill](.agents/skills/not-k8s-rust/SKILL.md) for code
structure, ownership, errors, async cancellation, bounded work, and verification.

- Components are replaceable crates and independent processes. `notk8s`
  only packages and dispatches them; do not add runtime policy there.
  `notk8s components` reports what a built binary actually contains.
  Keep split/combined behavior consistent and preserve `--proxy=none`.
- `nodeproxy` has no `cri` feature and must not acquire nodelet's
  tonic/prost/zbus dependency tree. Put behavior in its owning component.
- Nodelet's `PodRuntime` separates generic reconciliation from containerd
  mechanics. Plugin registration is shared by CSI, DRA, and device plugins.
- Reconciliation is watch-driven. A failed write, probe, runtime change,
  or missing dependency may produce no new object event. Give it an explicit,
  bounded recovery path. Prefer dependency events and keyed retries to
  periodic full-cluster scans; justify any safety-net poll.
- Independent informers have no global event order. Handle initial lists,
  relists, lag, watch errors, deletion, and same-name/new-UID replacement.
  A cached owner may already be gone. A live process may contain a stopped
  controller task.
- Never turn an internal storage race into a lost update. Distinguish
  caller preconditions from compare-and-swap failures. Recompute a PATCH
  from fresh state when retrying; recheck UID/version conditions. A stale
  conditional PUT needs a fresh read, not the same stale body.
- Watch progress is a promise: every matching event through the advertised
  revision has been delivered. LIST/WATCH handoff must not skip events.
  Multiple keys can change at one datastore revision. A resource version
  belongs to the whole parent object, including its subresources.
- Nodestore `apply()` reads no clock, randomness, or environment. Resolve
  nondeterminism before proposing. Read `replication/log.rs` before changing
  crash recovery: the separate raft log uses sqlite `synchronous=FULL`,
  and applied index plus resulting state commit in the same transaction.
  The gRPC server translates; storage/consensus own semantic decisions.
- Keep shared informer reads on the base controller identity and writes on
  the appropriate controller identity. Do not repair RBAC by granting
  cluster-admin or by weakening admission.
- Check the actual `Cargo.toml` schema/features and existing typed/raw
  adapters before changing Kubernetes API versions.

## Validation and completion

**Do not run local Cargo builds, Cargo tests, or local e2e on this development
host. Use CI.** It is resource constrained; debug artifacts can also exhaust
RAM/tmpfs. Source inspection, formatting checks, and lightweight documentation
or skill validation are fine. Do not install a toolchain to bypass this rule.

| Need | Required evidence |
| --- | --- |
| Iterate on Rust logic | `quick-check.yml` with `components=<changed crates>` |
| Produce installable binaries | `build.yml`, with correct architecture/profile/layout and tests enabled |
| Verify runtime compatibility | `e2e.yml` against the selected apiserver; targeted runs while iterating, full unfiltered suite before declaring green |
| Change a workflow or installation behavior | Exercise that workflow or real bootstrap path; Cargo tests alone provide no evidence |
| Change documentation/instructions only | Check accuracy, paths, and skill structure; state that there is no runtime behavior to exercise |

The merge-to-`main` gate remains **build.yml plus full unfiltered e2e** for
the final code being merged, including small changes, unless the user explicitly
changes that requirement. Quick-check is the integration-slice gate only;
it is not a substitute for the main gate. A documentation-only check does not
silently waive the merge protocol.

For runtime fixes, add or strengthen a real-cluster regression in the current
Rust e2e suite and add a focused unit test when it can deterministically expose
the race. Keep the existing failing test's behavioral assertions. Do not create
the controller-owned object from the harness, skip a failing test, replace a
real driver with a fake, or merely increase a timeout to obtain green.

Open the PR before CI dispatch. For substantive stabilization changes, start
quick-check and e2e together when that is the user's established workflow.
Record exact commit/run IDs. Read the skill for dispatch, watch cadence, log
capture, failure triage, and constrained-host artifact handling.

Control iteration cost: reuse matching SHA/target results, specify changed
crates, use the test host's architecture, and avoid release builds while
debugging correctness. Do not launch duplicate runs, read remote logs repeatedly,
or load entire historical documents to answer a narrow code question. Cost
savings must not remove the final full-suite gate. The current e2e workflow
deliberately uses stripped release binaries to keep artifacts small; do not
switch it to debug just to shorten compilation without measuring artifact size
and checking the user's storage budget. Quick-check remains the debug loop.

Do not merge automatically during stabilization. If merging is authorized,
require the applicable gates, squash-merge, then rebase other authorized open
PRs onto the new base and rerun their gates. Honor any user-specified ordering.
Do not force-push a shared branch without the required authority.

Completion reports must distinguish: changed, checked, passed, failed, skipped,
and still unverified. Include the tested SHA/run links and any remaining gate.
If a gate cannot run, explain why and leave the PR open. A passed filtered run
or unit test is never evidence that the full distribution is working.

## Keep this guidance useful

Keep permanent rules here and task-specific state in the handoff/PR. Do not
hardcode today's PR number, failure count, run ID, or temporary log path here.
Update obsolete guidance instead of appending another contradictory exception.
Keep Claude's entrypoint and the tool-specific skill entrypoints pointing to their
canonical sources.
