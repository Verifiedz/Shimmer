# 0035. Automations: "when this happens, do that", written by users

Status: proposed · Raised by Dev B (#228) · Needs sign-off: Dev A (§4 changes `Event` and `Ctx` in
`crates/core`; §5 moves two helpers into `core`; CLAUDE.md §1.1 gains a row, CLAUDE.md §4) · Dev C
notified (new ops and topics, §8) · Amends ADR 0032 §5 (notify's rules)

## Context

### Events are delivered, but nothing acts on them

Shimmer's modules publish events, and the bus delivers every event to every module
(`crates/daemon/src/lib.rs`: one `on_event` loop per module). But publishing isn't doing. Today
**no module implements `on_event`**: when a LeetCode record is completed, `records.item.completed`
reaches every module, and every module ignores it. An event causes something only when code
somewhere says "when this arrives, do that".

There are only two places that code can live:

1. **In a module, written in Rust by the team.** Right for built-in behaviour (notify will warn about
   a dirty workspace, ADR 0032 §4). Wrong for everything else:
   - every reaction anyone wants needs a developer, a PR and a release;
   - it's the same for every user, though people want different things;
   - a reaction *between* modules ("an application moved to interviewing → open my prep
     workspace") has no good home: putting it in records or workspaces makes one know the other,
     which §1.3 forbids.
2. **In data the user writes.** That's this ADR.

### Why it matters

- **It's the general mechanism.** CLAUDE.md §1: "a records engine that a LeetCode tracker is a
  forty-line configuration of". Records made *trackers* configuration; automations make the
  *connections between* trackers, workspaces, fetchers and notifications configuration. Without
  it, every workflow is a feature request.
- **It closes the MVP chain without coupling.** §1.3's chain (a posting found → saved as an
  application → reminded before its deadline) needs glue nobody owns. "When a posting matching X
  is found, add it to my applications" is one rule, not a change to records or fetchers.
- **It makes notify smaller.** ADR 0032 §5 designed notify's own rules ("when this happens, send
  this message"). That's one case of an automation whose action is `notify.send`. One rules system
  instead of two (§6).
- **It's what kits (#79) and an assistant would build.** "Set up my job hunt" or "make sure this
  connects to my workspace" mostly means writing a few rules: files a person can read, edit and
  delete afterwards.
- **It's the biggest gap in the playground** (#228).

## Decision

### 1. A rule

One TOML file per rule, `data/automations/rules/<id>.toml`, readable and editable by hand (§1.4):

```toml
[rule]
id = "interview-prep"
label = "Open my prep workspace when an application reaches interviewing"
enabled = true
on = "records.item.updated"                    # a topic, or a pattern: records.item.*, *
when = { collection = "jobs", "item.stage" = "interviewing" }

[do]
op = "workspaces.activate"                      # a canonical op (§15), never an alias
params = { id = "interview-prep" }
```

- **`on`:** a topic or a subscription pattern, matched exactly as an IPC subscription is
  (`a.b.c`, `a.*`, `*`; `docs/protocol.md`).
- **`when`** (optional): conditions on the event's payload, in **records' filter language**
  (ADR 0019, ADR 0024 §5): a plain value is equality; an object holds operators that must all
  pass: `eq`, `ne`, `lt`, `lte`, `gt`, `gte`, `in`, `set`, `contains`, `has`; relative dates
  (`today+3`) resolve in the local timezone. Keys are payload fields, with `item.<field>` reaching
  into a record event's record (ADR 0017 §7).
  ```toml
  when = { collection = "leet", "item.attempts" = { gt = 3 } }   # more than 3 attempts
  ```
  ```toml
  when = { title = { contains = "intern" } }                    # a title mentioning intern
  ```
  ```toml
  when = { "item.apply_by" = { lte = "today+3" } }              # due within 3 days
  ```
- **`do`:** one op and its params. Params are filled from the event:
  - a value that is exactly `"{field}"` takes that payload value **with its type** (a number stays
    a number, a list stays a list);
  - `{field}` inside other text is filled in as text, with `[optional]` parts, by `core::text`
    (ADR 0032 §1): `"Solved: {title}"`.
  ```toml
  # A posting found by a fetcher becomes an application ([rule]'s id and label left out)
  [rule]
  on = "fetchers.item.found"
  when = { title = { contains = "intern" } }
  [do]
  op = "records.add"
  params = { collection = "jobs", fields = { position = "{title}", url = "{url}" } }
  ```
- **Validated when created and on load:** the pattern is valid, the operators exist, the op is in
  `core.manifest` (a rule naming an op that later disappears stays, and fails visibly when it fires,
  §3). A file that doesn't parse is reported (`automations.health`, ADR 0033), never silently
  skipped.

### 2. How a rule runs

1. `on_event` receives every event, as for every module. Matching is plain functions over plain
   data (§12 rule 10): `on`, then `when`, then filling the params.
2. On a match, the op is **enqueued** with `ctx.queue` (CLAUDE.md §5: "available to any module"),
   never run inline. It runs in the op's own lane, takes its turn (§6.2), and shows in
   `shimmer queue list` with origin `module: automations`. A slow or failing action never holds up
   the event bus.
3. `automations.rule.fired {rule, event, task_id}` is emitted.
4. **No retry** (§11.2). A failed action is `queue.task.failed` like any other task: visible, in
   the log, and the lane moves on (§11.1).

### 3. Trust: the same as a schedule

A rule runs what its author wrote, exactly as a schedule does (`scheduler add` already runs any op a
user names, ADR 0031). Ops that need a confirmation (`records.purge`, `remove_collection`…)
answer `confirmation_required`, which a rule can't give, so they fail visibly instead of running.
Per-caller permissions (read / change / run) come with #228's permissions item and will apply to
rules too. Until then, `shimmer automations list` and `show` make every rule's action plain to see.

### 4. Loops: events carry their cause (core change)

The risk: "when a record is updated, update a record" re-triggers itself forever, a popup and a
queue task each time. Today nothing can tell an update caused by a rule from one a person made:
`Event` is `{id, at, source, topic, payload}` (`crates/core/src/event.rs`), and a queued task runs
with `Ctx::for_task(cancel, progress)` (`crates/core/src/ctx.rs`), which doesn't know its task id.

**Change (Dev A + Dev B):**

```rust
pub struct Event {
    // …existing fields…
    /// The queued task this event was emitted from, if any. Omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<TaskId>,
}
```

- `Ctx::for_task` takes the task id; every event emitted while that task runs is stamped with it,
  through `ctx.bus` and through store transactions' `tx.emit` alike.
- Additive: the event log, the bus and the IPC stream gain an optional field; old lines and old
  clients are unaffected.

**What automations does with it:** it remembers each task it enqueued as `task id → (rule, chain)`.
An event whose `cause` is one of those tasks continues that chain:

- **a rule already in the chain doesn't fire again** (a cycle: A → B → A);
- **a chain stops at depth 3** (A → B → C → no more);
- either way, `automations.chain.stopped {chain, rule, event}` is emitted, so a stopped loop is
  visible (in the log, to doctor, and to notify's alerts if wanted).

Events with no cause (a person's command, the scheduler, a module's own work) start a new chain.
Legitimate rules are never paused or rate-limited. Automations also never reacts to its own
`automations.*` topics.

### 5. Two helpers move into `core` (ADR 0014's pattern)

A module may depend only on `core` (§3 rule 1). Automations needs two things that live elsewhere:

- **Topic patterns:** `is_valid_pattern` and `topic_matches` live in `crates/proto/src/topic.rs`.
  They move to `core`; `proto` re-exports them, so clients are unchanged. (ADR 0032's notify rules
  needed the same; §6 makes that moot for notify, but the move is still right.)
- **The filter operators:** records' matcher (`crates/modules/records/src/query.rs`) is
  schema-aware: it knows each field's type from the collection. Events from other modules have no
  schema. So `core::filter` holds the **same operators over plain JSON values** (numbers compare as
  numbers, dates as ISO text, `has`/`contains` ignore case as in records), and records' query keeps
  its schema checks (which operator applies to which field type, enum order) on top and calls it
  for the comparisons. Records' behaviour and tests are unchanged. One difference, documented:
  without a schema, enums compare as text, so rules use `in` for enum values rather than `lt`/`gt`.

### 6. Notify's rules become automations (amends ADR 0032 §5)

- **Notify keeps delivery only:** `notify.send`, the inbox, desktop popups, quiet hours, and its two
  built-in alerts (a workspace went dirty; a scheduled task failed), which stay module code
  because they're on for everyone by default (ADR 0032 §4).
- **"Message me when X" is an automation** whose action is `notify.send`:
  ```toml
  [rule]
  on = "records.item.completed"
  when = { collection = "leet" }
  [do]
  op = "notify.send"
  params = { title = "Solved: {title}", priority = "low" }
  ```
- ADR 0032 §5's rule file, its four rule templates (record-done, record-added, task-failed,
  workspace-launched) and `notify.create_rule` / `rules` / `remove_rule` move here as automation
  templates and ops. Notify is smaller and can ship before automations; until automations exists,
  only the built-in alerts and direct `notify.send` work.
- ADR 0032's loop guard (§4: notify ignores its own work) still holds for its built-in alerts; for
  rules, §4 above replaces it.

### 7. Templates

In the records/workspaces template format (ADR 0025 §3, #202 later): a header with notes and "How
it works" examples, a category, and `[[question]]`s whose answers fill the rule. A few to start:

| Template | Makes a rule for |
|---|---|
| `notify-on-complete` | a message when a record in a collection is completed |
| `notify-on-add` | a message when a record is added to a collection |
| `notify-on-failure` | a message when a task fails (optionally one lane) |
| `posting-to-application` | a fetched posting matching some words becomes a record |
| `stage-opens-workspace` | a record reaching a value (e.g. a stage) launches a workspace |

Every template is tested end to end (ADR 0025 §6a).

### 8. Ops, topics, CLI

| Op | Execution | Params | Returns |
|---|---|---|---|
| `automations.list` | inline | `{}` | `{"rules":[…]}`, with when each last fired |
| `automations.get` | inline | `{"id"}` | one rule |
| `automations.create` | inline | `{"id","template","values"?,"label"?}` | the rule (validated, §1) |
| `automations.remove` | inline | `{"id"}` | `{"removed": true}` |
| `automations.enable` / `.disable` | inline | `{"id"}` | the rule |
| `automations.test` | inline | `{"id","event"}` | what the rule would enqueue for that sample event, without enqueuing it |
| `automations.templates` | inline | `{"id"?,"file"?}` | as `records.templates` |
| `automations.health` | inline | `{}` | ADR 0033's shape: unreadable rules, rules naming ops that no longer exist |

Topics: `automations.rule.created`, `.removed`, `.enabled`, `.disabled`, `.fired`;
`automations.chain.stopped`.

```
shimmer automations [list] · show ID · new ID --from TEMPLATE [--set Q=A]… · remove ID
shimmer automations enable ID · disable ID · test ID --event FILE · templates · peek TEMPLATE
```

Pack targets and aliases for each command (ADR 0029). Creating a rule shows ADR 0034's one-time
note: rules only run while the daemon runs.

### 9. How it keeps CLAUDE.md's rules

| Rule | How |
|---|---|
| §1.3 Decoupled; no module calls another | Automations never knows a module. Topics and ops are strings from the user's rule file, like a schedule's op. Delete workspaces: a rule naming `workspaces.activate` fails when it fires, visibly; nothing fails to compile. Delete automations: nothing else notices. |
| §1.3 Plugins work the same way | It subscribes and enqueues through the same channels, with no special access. |
| §1.4 Files are the truth | Rules are TOML files; each change is one file + its event in one transaction (§7.1). No state only in the index. |
| §1.5 No identity | Rules belong to the Shimmer folder, not to a user. No `UserId`. |
| §1.6 Extensible | A new reaction is a file, not code. |
| §2 Clients hold no logic | Matching, filling and enqueuing are in the module; the CLI only shows and sends. |
| §3 rules 1–2 | Depends on `core` only; the two helpers it needs move into `core` (§5). |
| §5 `Ctx` is a security decision | No new capability: it uses `bus`, `queue`, `store`, `clock`, `local_tz`. `Event.cause` and `for_task`'s id are data, not powers. |
| §6.2 Ordering | Enqueued like any task; no `Overridden`. |
| §11.1–11.2 Failure, no retry | A failed action is an ordinary `queue.task.failed`; automations never retries. |
| §12 rule 4 | No `std::fs`, no network, no `Utc::now()`: store, queue and clock through `Ctx`. |
| §12 rule 10 | Matching and filling are pure functions, tested without a runtime. |
| §12 rule 13 | Tests against the in-memory `Ctx`, including a loop that's stopped. |
| §12 rule 16; §15 | Canonical op names only; aliases never reach a rule. |

### 10. Follow-up: delayed actions ("do this in 7 days")

A rule acts **now**. "When I apply, remind me in 7 days to follow up" needs to set a one-time
trigger later, and an automation can't: it may only enqueue **module** ops, and creating a schedule
is the scheduler's (`scheduler.add`, a daemon op). It's the same missing piece as calendar's
reminders: a scoped way for a module to set its own one-time triggers (`Ctx.triggers`, raised with
calendar and not yet decided). When that exists, `[do]` gains an optional `after = "7d"` (or `at`),
and the rule's action is scheduled instead of enqueued. Until then, a recurring schedule plus a
filter covers many cases (e.g. every morning, list applications whose `follow_up_on` is today).

## Consequences

- Users connect modules without code; developers write mechanisms, users write workflows.
- Notify is smaller (§6); kits and an assistant have something safe and inspectable to produce.
- `core` gains `Event.cause`, a task id in `Ctx::for_task`, and two moved helpers. The daemon stamps
  causes. CLAUDE.md §1.1 gains a row: `automations` · owns: reactions between events and ops,
  written as data · specialised by: rule templates.
- Rules only run while the daemon runs (ADR 0034's start-at-login matters more).
- Built in this order, in `integration/automations`, after sign-off: `core` (§4, §5) → the daemon's
  cause stamping → the module (rules, matching, chains) → templates and CLI → docs.

## Not done here

- **Delayed actions** (§10), with `Ctx.triggers`.
- **Replaying missed events:** rules react live. Every change goes through the daemon, so nothing
  happens while it's down; only a lagging bus can drop events, which is logged. Replaying from the
  log could repeat actions.
- **Several actions per rule, or branching:** one `do` per rule; chain rules instead.
- **Permissions per caller** (#228).
- **A rules editor in the TUI.**

## Alternatives considered

- **Each module owns its reactions** ("records opens a workspace when…"): couples modules, and
  needs code per reaction.
- **A core service in the daemon, like the scheduler:** it needs nothing a module can't have, so it
  stays a module (§1.1: services are the queue, scheduler and bus).
- **Scripts listening on the socket:** works today for programmers; not saved, not run inside the
  daemon, not visible in the queue, and not for non-programmers. Rules are the declarative version.
- **Rate limits instead of `Event.cause`:** a loop still runs N times before stopping, and a busy
  legitimate rule can be paused by mistake.
- **Equality-only conditions:** simpler, but a second filter language for users to learn; records'
  operators are already familiar.
- **Keeping notify's own rules:** two systems that say the same thing.
