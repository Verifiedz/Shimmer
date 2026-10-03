# 0005. Fallback targets `notify.*` only, and never nests

Status: accepted (M2, #1) · Signed off: Dev A, Dev B (recorded in #6)

> **Renamed since:** `swe`, `swe-*` and `SWE_*` in this ADR are now `shimmer`, `shimmer-*` and
> `SHIMMER_*` (ADR 0011). The text below is kept as written.

## Context

CLAUDE.md §11.3 lets a scheduled task carry a fallback, set at trigger-creation time, that the
queue enqueues in the `notify` lane when the primary fails for a structural reason
(`workspace_dirty`, `unavailable`). The first draft was `Fallback::NotifyOnly { message,
priority }`, which fixed *what* is sent but gave no way to choose a sink or template.

The obvious generalisation, "a fallback is any op", has a hole: the fallback op is itself a task,
and tasks can fail. Then the question of what *its* fallback is has no good answer. Either
failures are silently dropped at the second level (the exact "degrade, don't drop" violation the
mechanism exists to prevent), or fallbacks nest without bound.

## Decision

`Fallback` names an op, constrained to one namespace:

```rust
Fallback::Notify { fallback_op: String, params: Value, priority: NotifyPriority }
```

* **`fallback_op` must be a `notify.*` op.** Checked at `scheduler.add` time (and for
  module-declared triggers at registration), never at failure time. Anything else is
  `invalid_params`. A trigger that cannot degrade safely is refused when it is created, while a
  human is looking, not discovered at 2pm when the launch fails.
* **A `notify.*` op must not itself declare a fallback.** `scheduler.add` rejects a trigger whose
  `op` is in `notify.*` and which carries a `fallback`. The queue also refuses to attach a
  fallback to a task it enqueues *as* a fallback. No nesting, so the chain is at most one link.

## Why `notify.*` closes the regress instead of deferring it

`notify` already owns a terminating failure path that is not another fallback (§11.3): each
`Sink` may name one `fallback_sink_id`, tried once, and a delivery that exhausts every sink
appends to `notifications/failed.jsonl` and emits `notify.exhausted`. So the worst outcome of a
fallback is a durable, visible record, never a loop and never silence. A generic op has no such
terminal and would need one invented per op.

This is also why the constraint is on the *namespace*, not on a list of allowed ops: a new sink
or template op added to `notify` later is automatically a valid fallback, with no change here.

## Consequences

* The queue must know how to place a fallback without naming a module (§12 rule 7): the fallback
  carries its own op, and the queue enqueues it through the same `submit` path as anything else.
  The only string the queue inspects is the `notify.` *prefix*, in the validation helper.
* "Structural" stays defined by error code (`workspace_dirty`, `unavailable`), not by op.
* A fallback that cannot be enqueued (op unknown, lane missing) is logged as
  `queue.fallback.failed` and never stalls the lane (§11.1).

## Status of the code

Implemented (M2). `swe_core::Fallback` is `Notify { fallback_op, params, priority }`, and
`NotifyOnly` is gone: a stored or submitted `notify_only` fallback no longer deserialises.

* **Rules**: `crates/daemon/src/queue/fallback.rs`, plain functions with unit tests. `validate`
  rejects a `fallback_op` outside `notify.*` (a bare `notify.` or a lookalike such as
  `notifications.send` does not count) and a `notify.*` op that carries a fallback, both as
  `invalid_params`. It checks the shape only, not that the fallback op exists (see below).
* **Where it runs**: `scheduler.add`; module trigger registration, where an invalid declaration
  is skipped with a warning like any other bad trigger and does not stop the daemon; loading
  persisted triggers, where an invalid file is skipped like a corrupt one; and the queue's own
  `submit`, so no path, including a module calling `ctx.queue`, can put an invalid fallback in
  front of a lane.
* **Enqueueing**: when a task fails with `workspace_dirty` or `unavailable` and carries a
  fallback, the queue enqueues the fallback op after freeing the failed task's slot and
  emitting `queue.task.failed`. Failures with any other code (`module_error`, `internal`, ...)
  and cancelled tasks get none. The fallback runs in the lane its own op declares, as `Normal`,
  with origin `Module { id: "queue" }` and no fallback of its own.
* **Events**: `queue.fallback.enqueued` (`task_id`, `fallback_task_id`, `op`, `lane`) and
  `queue.fallback.failed` (`task_id`, `op`, `error`, `code`) when it cannot be enqueued, for
  example because `notify` is absent. Both are logged. Neither stalls the lane, and nothing
  retries.
* **Two additions beyond this document**, both because the op receives only `params`. Into
  object params (null becomes an object) the queue stamps the following keys, the same way the
  scheduler stamps `scheduled_for` (ADR 0004). The rule is overwrite-if-present, for each key:
  a value already in the configured `params` under that name is replaced, other keys are kept,
  and non-object params (a string, an array) are passed through unchanged.

  | Key | Value |
  |---|---|
  | `priority` | The fallback's `NotifyPriority` (`low` / `normal` / `high`). |
  | `failure_reason` | The failed task's error message: `queue.task.failed`'s `error`. |
  | `failure_code` | The error code: `queue.task.failed`'s `code` (`workspace_dirty` / `unavailable`). Branch on this, never on the reason text. |
  | `failure_detail` | The error's structured `detail`: `queue.task.failed`'s `detail`, or `null`. For `workspace_dirty` this is the object from `protocol.md` (`workspace`, `failed_step`, `failed_at`, `log`, `has_cleanup_script`), so the notice can name the step and the log path. |

  These names are fixed the same way script ABI names are (§10.1): a `notify` op reads them,
  so renaming one breaks every sink and template. They are built from the same `Error` as the
  failure event, not a second copy. Nothing else about the failed task is stamped; a consumer
  that needs more (the failed task's id, its op) joins on `queue.fallback.enqueued`.
* **Existence is not checked at creation.** ADR 0005 constrains the namespace, and the `notify`
  module does not exist until M6, so a trigger with `fallback_op: "notify.send"` is accepted
  today and reports `queue.fallback.failed` (`unknown_op`) if it ever needs it.
* Tests: `tests/queue.rs` (enqueued in the `notify` lane as configured plus the stamped keys;
  a `workspace_dirty` failure's params carry the dirty reason, failed step and log path; a failing
  fallback ends the chain; non-structural and cancelled tasks get none; an un-enqueueable
  fallback is reported and the lane carries on) and `tests/scheduler.rs` (the rejection cases
  and module registration).
