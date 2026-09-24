# nodemigrate CI and integration status

Last updated: 2026-09-24

This is the living CI record for the scope in
[NODEMIGRATION_GOAL.md](NODEMIGRATION_GOAL.md). The user-specific testing policy
is recorded there and overrides conflicting general `AGENTS.md` gates for
this objective.

## Workflow

- `.github/workflows/nodemigrate-integration.yml` runs shell syntax validation
  on relevant pull requests.
- Manual `workflow_dispatch` starts isolated K3s and upstream Kubernetes
  lanes. Each lane builds the combined runtime binary and standalone
  `nodemigrate` binary as test setup, then runs
  `.github/scripts/nodemigrate-integration.sh` as root and uploads its log.
- The script installs the selected upstream distribution first, uses Cilium
  with Flannel disabled in the K3s lane, installs the hostPath CSI test
  driver, cert-manager, Traefik, nginx, static and CSI-backed claims, then
  checks the source → nodestore → retained-source round trip. Every checkpoint
  asserts the Cilium DaemonSet rollout and CiliumEndpoint CRD are present.
- Runtime coverage for joining an existing cluster and replacing a member is
  a separate required scenario; the current script does not establish that
  case as verified.

## Run policy

- General `build.yml`: excluded by the user for this task.
- General e2e workflow: excluded by the user for this task.
- Local Cargo build/test/e2e: prohibited by repository host constraints.
- Targeted `nodemigrate checks`: the focused compile and test check for
  nodemigrate Rust changes. `quick-check.yml` does not currently include the
  nodemigrate crate; use `components=nodebootstrap` there only when
  nodebootstrap changes. Run both focused checks when both crates changed.
  Do not run unrelated crates.
- Dedicated migration workflow: do not dispatch until the user authorizes
  migration runtime/e2e execution. Its compile step is test setup, not a
  generic build gate.
- Static shell syntax, formatting, diff whitespace, and docs-link checks are
  allowed and must be reported separately from runtime evidence.

## Verification log

| Date | SHA | Check/lane | Result | Evidence |
| --- | --- | --- | --- | --- |
| 2026-09-24 | `daac9ad05285e6ff749d4c51a18f9b71c8fb7dee` | Targeted quick-check: `nodebootstrap,nodemigrate` | Passed; predates bidirectional changes | [Run 35949477611](https://github.com/centerionware/not-k8s/actions/runs/35949477611) |
| 2026-09-24 | `55704b1c202c248ab633aac8c74877c46499e59f` | Nodemigrate crate checks | Failed compile; fixes are in worktree, rerun pending | [Run 35952766075](https://github.com/centerionware/not-k8s/actions/runs/35952766075) |
| 2026-09-24 | `55704b1c202c248ab633aac8c74877c46499e59f` | Targeted quick-check | Failed compile; fixes are in worktree, rerun pending | [Run 35952782893](https://github.com/centerionware/not-k8s/actions/runs/35952782893) |
| 2026-09-24 | `4e932917080e21412c79625fb2b79d08f5e62c0f` | Nodemigrate crate tests | Compiled; 9 passed, upstream control-plane network fixture failed because the manifest path was rooted twice. Fix pending CI rerun. | [Run 35954374767](https://github.com/centerionware/not-k8s/actions/runs/35954374767) |
| 2026-09-24 | `4e932917080e21412c79625fb2b79d08f5e62c0f` | PR shell validation | Passed | [Run 35954374805](https://github.com/centerionware/not-k8s/actions/runs/35954374805) |
| 2026-09-24 | `4e932917080e21412c79625fb2b79d08f5e62c0f` | Commit convention | Passed | [Run 35954372285](https://github.com/centerionware/not-k8s/actions/runs/35954372285) |
| 2026-09-24 | `4ef3585eaea6beae51ffd1116134435b0edc305e` | Nodemigrate crate tests | Passed | [Run 35954606805](https://github.com/centerionware/not-k8s/actions/runs/35954606805) |
| 2026-09-24 | `4ef3585eaea6beae51ffd1116134435b0edc305e` | PR shell validation | Passed | [Run 35954606851](https://github.com/centerionware/not-k8s/actions/runs/35954606851) |
| 2026-09-24 | `4ef3585eaea6beae51ffd1116134435b0edc305e` | Commit convention | Passed | [Run 35954605041](https://github.com/centerionware/not-k8s/actions/runs/35954605041) |
| 2026-09-24 | `63cb20cbe75ebdfafee861c41137b334fb6ff95b` | Nodemigrate crate tests | Passed, including source CNI detection and joined replacement worker command tests | [Run 35955823452](https://github.com/centerionware/not-k8s/actions/runs/35955823452) |
| 2026-09-24 | `63cb20cbe75ebdfafee861c41137b334fb6ff95b` | PR shell validation | Passed | [Run 35955823568](https://github.com/centerionware/not-k8s/actions/runs/35955823568) |
| 2026-09-24 | `63cb20cbe75ebdfafee861c41137b334fb6ff95b` | Commit convention | Passed | [Run 35955821523](https://github.com/centerionware/not-k8s/actions/runs/35955821523) |
| — | — | K3s + Cilium round trip | Not run; manual dispatch pending authorization | — |
| — | — | Upstream Kubernetes + Cilium round trip | Not run; manual dispatch pending authorization | — |
| — | — | Existing-cluster join/replacement | Not run; scenario not yet exercised by current script | — |
| — | — | Per-stage Cilium health assertions | Added to the script; static syntax check and runtime lanes pending | — |

For every new result, record the commit SHA, workflow run URL, lane, resolved
Kubernetes/K3s/Cilium/add-on versions, and pass/fail state at each checkpoint.
Keep failures and skipped checks visible rather than replacing them with a
later green run.
