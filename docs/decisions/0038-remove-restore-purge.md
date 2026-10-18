# 0038. Remove, restore and purge, and one set of command words for every feature

Status: proposed · Raised by Dev B · Needs sign-off: Dev A (§3 and §6 change the scheduler and queue,
daemon code; §6 renames two `proto` op constants and bumps the protocol to v2) · Dev C notified
(renamed and new ops and topics, §4, §6) · No `core` change · Follows #258's
convention 2 ("created and changed through ops and the CLI: new / edit / remove / restore")

## Context

`remove` means different things in different places today:

| Command | What happens now |
|---|---|
| `records remove` / `remove-collection` | Moved aside; `restore` brings it back; `purge` deletes it for good (ADR 0020, 0021, 0024 §5) |
| `workspaces remove` | Moved to `.removed/`; `restore` brings it back; **no purge**, so removed ones pile up (ADR 0026) |
| `automations remove` | Moved to `removed/`; `restore` brings it back; **no purge** (ADR 0035 §8) |
| `scheduler remove` | **Deleted for good**, though a schedule is something the person made |
| `notify remove` | Deleted for good |
| `queue cancel` | The task is dropped |

A person who removes something they made should be able to change their mind, and something
moved aside should be removable for good without opening the Shimmer folder by hand.

## Decision

### 1. The rule

Everything a person **makes** (a record, a collection, a workspace, a rule, a schedule) follows the
same three steps:

- **`remove`** moves it aside. It stops working (a removed schedule never fires, a removed rule
  never reacts) and leaves every list, but nothing is lost.
- **`restore`** brings it back exactly as it was. `conflict` if something with that id exists
  again; `not_found` if there's nothing removed to restore.
- **`purge`** deletes what was removed, for good: one by id, or all of them. It always asks first,
  as `records purge` does: without a matching `confirm`, `confirmation_required` with the count in
  `detail`, re-sent with `"confirm": {"<things>": N}`. `{"purged": 0}` without asking when there's
  nothing removed; `not_found` for an id that isn't removed. The CLI asks, or takes `--yes`.

Only the latest removal of an id is kept: removing it again replaces the older one (as records,
workspaces and automations already do).

### 2. Not everything is something a person made

- **Notify's messages** aren't: they're things Shimmer told you. `notify remove` stays a delete for
  good. The inbox cap already deletes old ones, and every message's text stays in the event log.
- **Queue tasks** aren't: a task is one run of a command. A cancelled or finished one leaves its
  trace in `queue show` and the event log; to do it again, run the command (or the schedule fires it
  again). There's nothing to restore.
- **A module's declared triggers** (notify's hourly flush) aren't the person's: they're paused, never
  removed (unchanged).

### 3. What changes

| Feature | Remove | Restore | Purge |
|---|---|---|---|
| Records | ✓ (unchanged) | ✓ | ✓ |
| Workspaces | ✓ (unchanged) | ✓ | **new: `workspaces.purge {"id"?, "confirm"?}`** |
| Automations | ✓ (unchanged) | ✓ | **new: `automations.purge {"id"?, "confirm"?}`** (with automations, ADR 0035) |
| Scheduler | **now moves aside** to `data/scheduler/removed/<id>.json` | **new: `scheduler.restore {"trigger_id"}`** | **new: `scheduler.purge {"trigger_id"?, "confirm"?}`** |

**The scheduler (Dev A):**

- `scheduler.remove` writes the trigger to `removed/<id>.json` and deletes it from `triggers/`, in
  one transaction, for a person's schedule (`usr-…`) and for a module's one-time schedule a person
  cancels (ADR 0035 §4). A module removing its own one-time schedule through `ctx.schedules`, or one
  that has run, is still deleted for good: it isn't the person's to restore.
- `scheduler.restore` brings it back as it was (paused or not) and, like `resume`, **counts from
  now**: a recurring schedule's next run is its next time after now; time spent removed is never
  caught up. A `once` keeps its time, and if that has passed, its own `catch_up` decides (run it now
  with `run_once`, drop it with `skip`). A restored module one-time schedule counts toward that
  module's 1,000 again, and is refused past it.
- On master today only `usr-…` schedules exist; the module one-time rule above lands with the
  automations stack (ADR 0035), which adds those schedules.
- `scheduler.list` doesn't show removed ones; `scheduler.list {"removed": true}` lists them, as
  `records trash` and `automations list`'s `removed` do.

### 4. Events and confirmation

- New topics: `workspaces.workspace.purged {workspace?, workspaces}`, `automations.rule.purged
  {rule?, rules}`, `scheduler.trigger.restored {trigger_id}`, `scheduler.trigger.purged
  {trigger_id?, triggers}`. `scheduler.trigger.removed` keeps its shape, with `reason: "removed by
  user"`.
- `confirm` names what's counted: `{"workspaces": N}`, `{"rules": N}`, `{"triggers": N}` (records:
  `{"records": N}`). A stale count is refused again with the fresh one, so two people (or a person
  and a rule) can't purge more than was shown.

### 5. CLI

`shimmer workspaces purge [ID] [--yes]`, `shimmer automations purge [RULE] [--yes]`, `shimmer
scheduler restore ID`, `shimmer scheduler purge [ID] [--yes]`, `shimmer scheduler list --removed`.
Each gets an alias in every built-in pack (ADR 0013, 0029), Tab completion and help.

### 6. Command conventions (amended 2026-10-18)

The same idea for the words themselves: each meaning has one word, and that word is the same in the
CLI, the op and the docs. Nothing depends on the old names yet (no dogfooding, no outside clients),
so they're renamed outright rather than kept as second names.

| Meaning | Word | Was |
|---|---|---|
| Show one thing in full | `show` (op `<feature>.show`) | `records get`, `workspaces status NAME`; ops `records.get`, `workspaces.status`, `scheduler.get`, `notify.get`, `queue.task` |
| The whole feature's state | `status` (no argument) | (`notify status`, `automations status`, unchanged) |
| Make a definition | `new` (op `<feature>.new`) | ops `records.create_collection`, `workspaces.create`, `automations.create` |
| Put an entry into something, change an entry | `add` (records, schedules) or `send` (notify); `update` changes a record's fields | unchanged |
| Change a definition | `edit` | **new: `scheduler edit TRIGGER WHEN … COMMAND…`** (keeps the id, whether it's on, and its last run; was remove and add, which gave a new id and left a removed copy) |
| Switch a thing off and on | `disable` / `enable` (a thing shows `enabled`) | `scheduler pause` / `resume` (`paused`); topics `scheduler.trigger.paused` / `.resumed` → `.disabled` / `.enabled`. Notify's `pause` / `resume` stays: it's do-not-disturb for a while, not switching a thing off |
| See what's removed | `list --removed` (op `<feature>.list {"removed": true}`) | `records trash COLLECTION` (op `records.trash`), automations mixing removed ones into `list`; **new for workspaces** |
| Rename / copy a named thing | `rename` / `copy` | records and workspaces had them; **new for automations** |
| Reorder | `move` (op `queue.move`) | op `queue.reorder` |
| Problems for `shimmer doctor` | op `<feature>.health` (ADR 0033) | records, workspaces, automations had it; **new: `notify.health`** |
| Purged records | topic `records.item.purged` | `records.trash.purged` |

- **The CLI word is always the op's verb** (with `_` as `-`). The CLI's list of exceptions (ADR
  0015 §1) is now empty, and a test keeps it that way.
- **Ids:** named things (collections, records, workspaces, rules) are typed by name; things
  Shimmer names (schedules, tasks, messages) take a unique ending of their id (ADR 0031 §4).
- **Every command** takes `--json`; anything that can't be undone asks first and takes `--yes`.
- **Templates** (`templates`, `peek`, `new --from`) follow #202.
- **The protocol goes to v2**, since ops were renamed (protocol.md lists the changes). `queue.task`
  and `queue.reorder` are constants in `proto`; that and the version are the only `proto` changes.
- **Fetchers** follows this in #277 (`show`, `enable` / `disable`, `list --removed`, `health`); it
  lives on `integration/fetchers`, not in this PR.

## Not chosen

- **One shared `core` helper for move-aside, restore and purge.** It would stop a fourth copy of the
  pattern (#252's review), but it's a `core` change, and the storage differs enough (a record file
  per collection, a workspace folder, one JSON file per trigger) that the helper would mostly be
  parameters. Each module follows this rule instead; revisit if a fifth feature needs it.
- **A trash for notify and the queue** (§2).
- **Purge without asking for a named id.** One id is still for good; the same confirmation as a
  bulk purge keeps it from being one keystroke.
- **Keeping the old command words as second names** (§6). It would add a second word for every
  meaning, the opposite of the point, to protect scripts that don't exist yet.
- **`pause` / `resume` everywhere instead of `enable` / `disable`.** Notify's pause means "hold
  everything for a while", with a time; a schedule or a rule is simply on or off.

## Consequences

- `remove` is always undoable for what a person made; `purge` is always the one way to delete it for
  good, and it always asks.
- Four new ops (`workspaces.purge`, `automations.purge`, `scheduler.restore`, `scheduler.purge`), four
  new topics, a `removed/` folder in the scheduler's namespace, and four aliases per pack.
- protocol.md documents each op and topic, and that `scheduler.remove` now keeps the trigger.
- §6: one word per meaning across every feature, the same in the CLI and the protocol; `scheduler
  edit`, `workspaces list --removed`, automations `rename` / `copy` and `notify.health` fill the
  gaps; packs, completion, help and docs follow; the protocol is v2.
