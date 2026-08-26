# Contributing to not-k8s

Build, test, and architecture docs live elsewhere and aren't repeated here:
[`CLAUDE.md`](CLAUDE.md) for build/test/e2e commands and repo layout,
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for design rationale,
[`docs/GAP_CLOSURE.md`](docs/GAP_CLOSURE.md) for what's actually implemented,
[`docs/E2E_FINDINGS.md`](docs/E2E_FINDINGS.md) for bugs found by real testing.

Grep before assuming something is or isn't implemented; don't guess from the
architecture doc alone.

## The one norm that matters

**Claims get verified against real infrastructure, not inferred from the spec.**
When a feature looks done, stand up the real thing (a real CSI driver, a real
DRA driver, a real runner) and watch it run. `docs/E2E_FINDINGS.md` is the
record of every time that caught a bug code review had missed.

Comments here say *why*, and often say "confirmed for real" followed by what
actually happened. Please keep writing them that way.

## Commit messages

[Conventional Commits](https://www.conventionalcommits.org/), **enforced in
CI** on every PR (`.github/workflows/commit-lint.yml`) for both the commits and
the PR title.

```
type(optional-scope)!: description

Optional body explaining WHY.
```

**Types:** `feat`, `fix`, `docs`, `refactor`, `perf`, `test`, `build`, `ci`,
`chore`, `style`, `revert`.

**Scope** is optional and free-form — whatever names the part you touched: a
crate (`nodeproxy`), a module (`pods`), an area (`e2e`, `deploy`), or a file
(`ARCHITECTURE.md`).

**Enforced rules:**

- Header at most 100 characters. Not 50 or 72 — subjects here carry real
  information rather than a label. Use the room; don't pad it.
- Description starts lowercase, no trailing period, at least 10 characters, and
  says what changed (`chore: updates` is rejected).
- Blank line between header and body.
- Breaking changes: `!` before the colon, a `BREAKING CHANGE:` footer, or both.
  That footer is spelled exactly that way — a near-miss silently declares
  nothing, so it's rejected.
- `fixup!`/`squash!` are fine during review, autosquash before merge.
- Merge and auto-generated `Revert "..."` subjects are exempt.

```
feat(nodeproxy): watch EndpointSlices instead of Endpoints
fix(deploy): remove the stale pid file, not just the process it named
docs: explain why the profiling legs both run --proxy=none
refactor(nodelet)!: drop the in-process service proxy
```

**Write a body.** The header says what; the body is where the value is — what
was broken, how you know, what you ruled out. A one-liner for a non-trivial
change throws that away.

Check before pushing with the same Conventional Commits shape CI enforces:

```bash
git log -1 --format=%s | grep -Eq '^(build|chore|ci|docs|feat|fix|perf|refactor|revert|style|test)(\([A-Za-z0-9._/-]+\))?!?: .{10,100}$'
git config core.hooksPath .githooks     # or have git reject it at commit time
```

Existing history predates this convention (`Round 124: ...`,
`ARCHITECTURE.md: ...`). It isn't being rewritten, and CI only checks the
commits a PR introduces — so don't copy what's behind you.

## Pull requests

- Branch off `main`, merge back into `main`.
- PR title follows the same rules — a squash merge uses it as the subject.
- PRs get **CodeRabbit review only**. Unit tests and e2e need real sudo and a
  real cluster, so they deliberately don't run on arbitrary PR code (see
  `.coderabbit.yaml`). The commit-convention check is the one exception;
  `.github/workflows/commit-lint.yml` explains why it's a different risk class.
- Say plainly what you did and didn't verify. "Not built or tested yet" is a
  fine thing to write, and much better than letting a reader assume otherwise.
