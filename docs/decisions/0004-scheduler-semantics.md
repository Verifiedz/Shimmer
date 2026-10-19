# 0004. Scheduler semantics

Status: accepted (M2) · Needs sign-off: Dev A

CLAUDE.md §6.3 fixes the shape (`TriggerSpec`, `CatchUp`, never overlap, persisted state,
injected clock). These are the points it left open.

* **UTC only.** Cron and `once` are evaluated in UTC. "09:00 Mondays" is 09:00 UTC, not local
  time. Local-time schedules need a timezone source (system TZ or an IANA database) and a
  rule for DST gaps and overlaps; deferred, and a real limitation for a laptop app.
* **Cron dialect.** Standard 5-field (`min hour dom month dow`, Sunday = 0 or 7, Monday = 1)
  or 6-field with leading seconds. The `cron` crate numbers weekdays 1-7 from Sunday and
  rejects 0, so a literal `1` would fire on Sundays; numeric weekdays are translated to
  names before parsing, and a test pins this.
* **`every` is anchored, not drifting.** First due = creation time + period; each next due is
  previous due + period, whatever time the daemon actually woke.
* **"Missed" means later than 60 s** after the scheduled time. Within that, a firing is on
  time under every policy (including `skip`), so a busy poll never becomes a catch-up.
  `skip` drops the rest; `run_once` fires one task stamped with the latest occurrence;
  `backfill` fires one per occurrence, stamped with its own time.
* **Bounds.** One catch-up examines at most 10,000 occurrences and backfills at most 1,000;
  the remainder is counted as missed. A `1s` trigger after a month asleep cannot flood the queue.
* **`scheduled_for`.** Object params gain a `scheduled_for` RFC 3339 timestamp (null params
  become an object holding it) so a backfilled run knows which period it is for. Other param
  types are passed unchanged.
* **At-most-once.** State (`last_run`, `next_due`) is persisted *before* the task is
  enqueued, so a firing is never repeated. Duplicate emails are the worse failure.
  *Amended (#263 finding 1):* a firing is also never **lost** to a stop. The queue lives in
  memory, so a task still waiting in its lane when the daemon stopped used to vanish (and a
  finished `once` with it). Each firing is now kept on the trigger (`unfinished`, by the time
  it was for) until its task ends, and settled on the next start from the event log, which
  logs `queue.task.started` before a task's code runs:
  - never handed to the queue, or handed and never **started** → queued again
    (`scheduler.trigger.fired` with `after_restart: true`). It never ran, so this can't
    repeat anything;
  - **started** and never ended (the daemon died mid-run) → **not** run again: it may have
    half sent something. It's recorded on the trigger (`interrupted`) and reported
    (`scheduler.trigger.interrupted`, which the built-in `scheduled-task-interrupted` rule
    turns into a message, ADR 0035 §10);
  - ended (finished, failed or cancelled) → forgotten.
  A module's finished one-time schedule is deleted once its task has ended, not when it
  fires. A paused trigger's waiting firing is still run: it fired before the pause. This is
  scheduled work only; whether *every* queued task should survive a restart is #236.
* **Overlap.** If any task from an earlier firing is still queued or running, every firing
  due this tick is dropped (`scheduler.trigger.skipped`, `reason: "overlap"`), including a
  whole backfill batch. Tasks from the same batch do not count against each other. Pending
  ids live in memory, like the queue.
* **Pause/resume.** Paused triggers do not advance. On resume `next_due` is recomputed from
  now: time spent paused is not "missed" and is never backfilled.
* **Ownership.** Module-declared triggers are reconciled with `Module::triggers()` on every
  start: state is kept, a changed schedule resets `next_due`, an undeclared one is deleted.
  Users may pause them, not remove them. A trigger aimed at an op that is not registered is
  skipped with a warning, so removing a module degrades instead of failing startup.
* **Storage.** `data/scheduler/triggers/<id>.json`, pretty-printed and hand-editable. JSON
  rather than TOML because params are arbitrary JSON, which TOML cannot represent (no null).
  An unreadable file is skipped, never overwritten, and listed (`scheduler.list`'s `broken`,
  `shimmer scheduler list`, `shimmer doctor`) until fixed or `scheduler.remove`d, which deletes
  it (#263 finding 5). A module's own trigger is written again from its declaration.
* **A `once` already past with `skip`** is refused at `scheduler.add`/`edit` (`invalid_params`):
  it could never run (#263 finding 3). `run_once`/`backfill` run it now.
* **Finished `once`.** Kept with `done: true` so a module re-declaring it does not fire it again.
* `scheduler` is a reserved module id *and* namespace.
* **Fallback.** The scheduler validates it (a `notify.*` op, never on a `notify.*` trigger) and
  passes it to the queue with the task; it never acts on it. The queue enqueues it when the
  task fails structurally, and stamps the failure context into its params. See ADR 0005.
