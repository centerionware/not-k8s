---
name: not-k8s-rust
description: Implement clear, maintainable, efficient Rust changes in not-k8s with correct component ownership, explicit state transitions, bounded async work, and focused verification. Use when writing or refactoring runtime Rust code; combine with stabilization or upgrade guidance when those workflows apply.
---

# Write Rust that remains correct under real workload pressure

Read [AGENTS.md](../../../AGENTS.md), the owning module, its callers, and the
closest existing tests. Preserve the user's scope. Use the stabilization skill
for diagnosis and the upgrade skill for target-version changes; this skill
guides implementation once the required behavior is understood.

## Decide the boundary before coding

State the observable behavior and the invariant the change must preserve.
Identify the owner crate, state owner, input event, state transition, external
effect, and recovery event. For a cross-component feature, trace the producer
and consumer rather than implementing the same policy twice.

Prefer the existing abstraction when it represents the real boundary:
`PodRuntime` for generic versus CRI behavior, the shared plugin registry,
component build tables, shared informers, and keyed work queues. Keep wire
translation separate from semantic decisions. Do not create a new trait,
framework, dependency, or shared crate for one call site or hypothetical reuse.
Do not copy an existing implementation if it contains the bug being fixed.

## Make the code easy to verify

- Give functions one cohesive responsibility. Extract named preparation,
  validation, and persistence steps when that exposes an invariant; avoid a
  giant function with many mutable flags or a helper whose only purpose is
  hiding three obvious lines.
- Prefer typed state/enums to combinations of booleans or stringly typed
  transitions. Use names that describe domain meaning, not implementation
  accidents. Keep a state machine's transitions visible in one place.
- Make ownership and lifetimes explicit. Borrow when the owner outlives the
  operation; clone when a task/cache needs its own data. Do not introduce `Arc`,
  interior mutability, or lifetimes everywhere to avoid a measured-small copy.
- Follow local conventions, but do not add more macro machinery merely to
  avoid passing arguments. In macro-heavy handlers, keep substantive new logic
  in ordinary functions where feasible; preserve the existing control flow and
  avoid an unrelated handler rewrite.
- Use comments for contracts, non-obvious ordering, tradeoffs, and evidence.
  Put incident narratives in the findings document. Delete obsolete comments;
  do not surround straightforward code with repeated assurances that it is real.
- Keep generated code, mechanical formatting, dependency churn, and behavioral
  changes distinguishable in the diff. Do not reformat unrelated modules or
  hand-edit generated output. Keep public interfaces no broader than necessary.

## Make failure and cancellation explicit

Return meaningful outcomes and contextual errors. Distinguish absent, stale,
forbidden, invalid, retryable, and successful no-op states. Do not collapse all
errors to `false`, `None`, or "not found" when callers need different behavior.
Avoid `unwrap`/`expect` for network, configuration, deserialization, or other
runtime input. An invariant assertion should describe an actual invariant.

For external effects, define the timeout, retry predicate, backoff/limit, and
identity/preconditions. Re-read and recompute on stale-state races. Do not retry
invalid/authz failures, blindly resend stale PUT bodies, or silently discard a
failed write. Preserve the original useful error when retries are exhausted.

Check cancellation at every `select!`, timeout, spawned task, and stream
boundary. If a future loses the race and is dropped, what state/effect remains?
Use cancellation-safe queues/receivers or preserve partial state explicitly.
Supervise background tasks and propagate or recover from their errors/exit;
a process that stays alive with a dead controller is not healthy.

Bound both active work and queued/spawned work. Acquiring a semaphore inside an
unbounded spawned task limits active I/O, not memory or waiting task count.
Coalesce by object key and retain an explicit retry edge when failure produces
no watch event. Avoid holding a synchronous lock across `.await`; take a small
snapshot under the lock, then release it. If an async lock must span I/O, explain
why serialization is necessary and bound that I/O.

For shared caches, make snapshot/subscription/revision updates atomic at the
contract boundary. Handle Init/InitDone, lag, relist, deletion, and replacement
UIDs; do not assume another informer is already initialized. A reconnect must
not lose data or permanently park a subscriber.

## Spend CPU and memory where they do useful work

Prefer event-driven wakeups, keyed queues, bounded buffers, and reuse of
immutable schema indexes. Avoid rebuilding lookup tables, cloning large object
graphs, or making API calls inside a per-item loop when one shared snapshot or
index suffices. Check invalidation and freshness before adding a cache.

Do not introduce unsafe code, custom allocators, lock-free structures, or a
new concurrency framework based on intuition alone. For a performance claim,
use [the performance skill](../not-k8s-performance/SKILL.md): profile first,
change the measured cost, then compare equivalent runs. Simpler code that
preserves the contract is preferable to clever code with unproved gains.

## Review the behavior, then validate

Walk through success, empty input, duplicate/out-of-order event, concurrent
update, dependency not yet present, same-name replacement, API outage, and
shutdown where relevant. Verify the caller can distinguish important outcomes
and that every asynchronous state change has a path back to reconciliation.

Add focused tests at the contract boundary. Use barriers or explicit state
transitions for races rather than sleep-based unit tests. Test observable
behavior and failure recovery; do not assert internal call counts, private
layout, or wording unless they are the contract. A real-infrastructure bug
still requires its real e2e regression. Avoid building a large mock framework
for a small fix or claiming a mocked runtime proves containers work.

Inspect the final diff for accidental scope changes, stale comments, swallowed
errors, unbounded work, and duplicated behavior. Use [the CI skill](../not-k8s-ci/SKILL.md)
to run the required checks with the changed crate list. Report what changed,
why, what passed, and any remaining limitation; do not claim speedups from code
appearance or compatibility from compilation.
