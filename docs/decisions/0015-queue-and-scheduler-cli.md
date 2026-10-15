# 0015. `shimmer queue` and `shimmer scheduler`: CLI commands for the core services

Status: accepted (M5, #75) · Raised by Dev B (owner of `crates/cli`) · Signed off: Dev A (owner of
the queue and scheduler ops these commands drive) · No `core`, `proto` or daemon change: every op
used already exists on master (`docs/protocol.md`, "Queue ops" and "Scheduler ops")

## Context

The queue and the scheduler are the two core services (CLAUDE.md §1.2), and their ops have been on
master since M2. The only way to reach them from a terminal today is `shimmer call queue.list`,
which prints raw JSON. Now that workspace launches really run through the queue, "what is my
machine doing right now" (§1.2) should be one readable command away, and adding a scheduled task
shouldn't mean hand-writing a JSON `TriggerSpec`.

The CLI's command surface is permanent once people script against it (ADR 0007), so its shape is
decided here first.

## Decision

### 1. The commands

```
shimmer queue list [--lane LANE]       running and queued tasks, per lane
shimmer queue show TASK                one task in full (also finished ones the daemon still remembers)
shimmer queue cancel TASK              cancel a queued task, or ask a running one to stop
shimmer queue move TASK --before OTHER move a queued task ahead of another in its lane
shimmer queue move TASK --last         move it to the back of its lane

shimmer scheduler list                 every trigger: schedule, catch-up, next and last run, state
shimmer scheduler show TRIGGER         one trigger in full
shimmer scheduler add OP [PARAMS] (--every DURATION | --cron EXPR | --once TIME)
                      --catch-up skip|run-once|backfill [--lane LANE]
shimmer scheduler pause|resume|remove TRIGGER
```

As with `records` and `workspaces` (ADR 0007), the bare word (`shimmer queue`) shows help, and
`--json` on any command prints the daemon's raw answer.

| Command | Op |
|---|---|
| `queue list` | `queue.list` |
| `queue show` | `queue.task` |
| `queue cancel` | `queue.cancel` |
| `queue move` | `queue.task` (to find the lane), `queue.list` (for its `queue_version`), then `queue.reorder` |
| `scheduler list`, `show` | `scheduler.list` (`show` picks one trigger from it; since then, `show` uses `scheduler.get`) |
| `scheduler add` | `scheduler.add`, then `scheduler.list` to say when it first runs |
| `scheduler pause`, `resume`, `remove` | `scheduler.pause`, `.resume`, `.remove` |

### 2. `scheduler add`: how a trigger is written

- **What it runs:** `OP [PARAMS]`, exactly like `shimmer call` (PARAMS is a JSON object, default
  `{}`). Any registered op is allowed; the daemon checks it.
- **When**, exactly one of:
  - `--every DURATION`: a number and a unit, combinable: `30s`, `15m`, `2h`, `3d`, `1w`, `1h30m`.
    A bare number is rejected, so `--every 2` can't mean two seconds by accident.
  - `--cron "EXPR"`: five fields, passed through unchanged. **Cron times are UTC** (ADR 0004), and
    the help says so.
  - `--once TIME`: RFC 3339 with an offset (`2026-10-10T09:00:00+01:00`, or `Z`), or a plain
    `2026-10-10 09:00` / `2026-10-10T09:00`, read as this machine's local time. The CLI converts
    it to UTC before sending, and prints back what it understood.
- **`--catch-up` is required** (CLAUDE.md §6.3: there is no default). The CLI spells the values
  `skip`, `run-once`, `backfill` (`run_once` is accepted too), and when it's missing, the error
  explains all three in one line each.
- **`--lane LANE`** is optional and passed through.
- **Fallbacks are not offered yet.** A fallback must be a `notify.*` op (ADR 0005), and no notify
  module exists, so a flag for it couldn't be used or tested. It arrives with notify (M6).

### 3. Showing it

- Task and trigger ids are shown in full, so they can be copied straight back into a command.
- Times are shown relative and in local time: a task's age (`12s`), a trigger's next run
  (`in 2h`) and last run (`3m ago`); `show` adds the full local time. All of this is presentation
  only: the daemon's values are UTC and unchanged (ADR 0009).
- A task's origin reads as `you`, `trigger <id>` or `module <id>`. A trigger's state is `active`,
  `paused` or `done` (a `--once` trigger that has fired).
- `queue cancel` says which case happened: a queued task is gone; a running one has been asked to
  stop and ends at its next cancellation check (CLAUDE.md §12 rule 11).

### 4. No confirmation prompts

Unlike `workspaces reset` and `force-relaunch`, these commands don't ask first. Each names exactly
one task or trigger by its id, and nothing they do is hard to undo: a cancelled task can be run
again, and a removed trigger added again.

### 5. Command packs

The new ops join the commands a pack may alias (ADR 0013 §6): `queue.list`, `queue.task`,
`queue.cancel`, `queue.reorder`, `scheduler.list`, `scheduler.add`, `scheduler.remove`,
`scheduler.pause`, `scheduler.resume` (`scheduler show` has no op of its own, so it can't be
aliased; since then it can, as `scheduler.get`, and packs may alias every command: ADR 0029).
They are **optional** for a user's own pack. The four built-in packs name all 25
commands (the table is in `docs/packs/README.md`), and anime-tropes was reworked so every alias is
an everyday expression anime watchers know (`yatta`, `sayonara`, `yamete`, …). `queue` and
`scheduler` were already reserved words (ADR 0013 §7), so no existing pack can break.

## Not done here

- **Promotion** (`priority: override` with its confirmation round, CLAUDE.md §6.2). It belongs on
  the commands that enqueue work (`workspaces activate --promote`, say), needs the request
  frame's `queue` block in the client, and needs its own design for showing what would be
  displaced.
- **Fallbacks on `scheduler add`**, until a notify module exists (§2).
- **Following a task live** (`queue show --wait`), as `workspaces activate --wait` does.
- **Short id prefixes** (`queue show 01M4`). Ids are copied in full for now.

## Alternatives considered

- **`shimmer queue` alone lists the queue.** Rejected for consistency: every other noun
  (`records`, `workspaces`, `packs`) shows help when bare.
- **A default `--catch-up`.** Rejected: CLAUDE.md §6.3 forbids one, and this is exactly the
  mistake it guards against.
- **Treating a bare number as seconds in `--every`.** Rejected: `--every 2` meaning two seconds
  is a trap; a unit costs one character.
- **Reading a time without an offset as UTC.** Rejected: people type the time on their own clock.
  Local time, converted and echoed back, is what they mean, and the echo catches a mistake.
