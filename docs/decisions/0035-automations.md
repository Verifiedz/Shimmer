# 0035. Automations: "when this happens, do that", written by users

Status: proposed · Raised by Dev B (#228) · Needs sign-off: Dev A (§5's `core` changes: `Event.cause`,
three helpers moved into `core`, `Manifest.topic_descriptions`, one-time module schedules,
`Ctx.catalog`; CLAUDE.md
§1.1 gains a row, CLAUDE.md §4) · Dev C notified (new ops and topics, §8) · Amends ADR 0032 §4–§5
(notify's alerts and rules), ADR 0033 (a `fix` in workspaces' dirty detail, §10)

## Context

### Events are delivered, but nothing acts on them

Shimmer's modules publish events, and the bus delivers every event to every module
(`crates/daemon/src/lib.rs`: one `on_event` loop per module). But publishing isn't doing: when a
LeetCode record is completed, `records.item.completed` reaches every module, and an event causes
something only when code somewhere says "when this arrives, do that".

There are only two places that code can live:

1. **In a module, written in Rust by the team.** Right for a few built-in behaviours. Wrong for
   everything else:
   - every reaction anyone wants needs a developer, a PR and a release;
   - it's the same for every user, though people want different things;
   - a reaction *between* modules ("an application moved to interviewing → open my prep
     workspace") has no good home: putting it in records or workspaces makes one know the other,
     which CLAUDE.md §1.3 forbids.
2. **In data the user writes.** That's this ADR.

### Why it matters

- **It's the general mechanism.** CLAUDE.md §1: "a records engine that a LeetCode tracker is a
  forty-line configuration of". Records made *trackers* configuration; automations make the
  *connections between* trackers, workspaces, schedules, fetchers and notifications configuration.
  Without it, every workflow is a feature request.
- **It closes the MVP chain without coupling.** §1.3's chain (a posting found → saved as an
  application → reminded later) needs glue nobody owns. Each link is a rule, not a change to a module.
- **One rules system.** Notify's "message me when X" (ADR 0032 §5) and its built-in alerts (§4) are
  rules whose action is `notify.send`; they live here (§10).
- **It's what kits (#79) and an assistant (#239) would build:** a few rules, as files a person can
  read, edit and delete afterwards.

### What two real flows need

Designed against the flows it has to carry, not in the abstract:

- **Job hunt:** a posting is added (or fetched) → you're told; you apply → three days later you're
  reminded to follow up; it reaches interviewing → its own prep workspace is made and opened; it
  reaches an offer → you're told; it closes → the prep workspace goes.
- **LeetCode:** weekdays at 09:00 the practice workspace opens (a schedule); a problem completed →
  a quiet "Solved"; 60 days later → "time to redo it"; three or more attempts → "this one's giving
  you trouble".

Both need several actions per event (§1), actions that wait (§4), and ready-made rules (§9).

## Decision

### 1. A rule

One TOML file per rule, `data/automations/rules/<id>.toml`, readable and editable by hand (§1.4):

```toml
[rule]
label = "Interviewing: open prep and tell me"
enabled = true
on = "records.item.updated"                     # a topic, or a pattern: records.item.*, *
when = { collection = "jobs", "item.stage" = "interviewing" }

[[do]]
op = "workspaces.activate"                      # a canonical op (§15), never an alias
params = { id = "interview-prep" }

[[do]]
op = "notify.send"
params = { title = "Interview stage: {title}", priority = "high" }
```

- **`on`:** a topic or a subscription pattern, matched exactly as an IPC subscription is
  (`a.b.c`, `a.*`, `*`; `docs/protocol.md`). `shimmer automations events` lists every topic with a
  description (§5.5), so a rule can be written without reading the protocol.
- **`when`** (optional): conditions on the event's payload, in **records' filter language**
  (ADR 0019, ADR 0024 §5): a plain value is equality; an object holds operators that must all pass:
  `eq`, `ne`, `lt`, `lte`, `gt`, `gte`, `in`, `set`, `contains`, `has`; a top-level
  `or = [{…}, {…}]` passes when any one of its alternatives does (§5.3); relative dates (`today+3`)
  resolve in the local timezone. Keys are payload fields; `item.<field>` reaches into a record
  event's record (ADR 0017 §7); dotted paths reach nested fields (`detail.workspace`).
- **`do`:** one or more actions, each an op and its params. **All of a rule's actions start
  together and run independently:** one failing doesn't stop the others. For steps that must happen
  in order, a rule starts a workspace, whose steps already run in order and stop on a failure
  (§10.1 of CLAUDE.md); or two rules chain through an event (§9's `stage-creates-workspace`).
  Writing two rules on the same event works too; several actions in one rule only keep a shared
  condition in one place.
- **Params are filled from the event:** a value that is exactly `"{field}"` takes that payload value
  **with its type** (a number stays a number, a list stays a list); `{field}` inside other text is
  filled in as text, with `[optional]` parts, by `core::text` (§5.4): `"Solved: {title}"`.
- **Validated when created, edited and loaded:** the pattern is valid, the operators exist, every op
  is in `core.manifest`, and no op is `automations.*` (§6). A rule naming an op that later
  disappears stays, and fails visibly when it fires. A file that doesn't parse is reported
  (`automations status`, doctor via `automations.health`, ADR 0033), never silently skipped.

### 2. How a rule runs

1. `on_event` receives every event, as for every module. Matching is plain functions over plain data
   (§12 rule 10): `on`, then `when`, then filling the params. It needs no lane: it's a quick check.
2. On a match, each action is **enqueued** with `ctx.queue`, never run inline, **in the op's own
   lane**: a launch in `workspaces` (one at a time), a message in `notify`, a record change in
   `default`, exactly as if a person had run it. A separate automations lane would break what lanes
   protect (two launch scripts at once). Each shows in `shimmer queue list` with origin
   `module: automations`.
3. **Every matching rule fires, with no order between rules.** Their actions land in different
   lanes that run independently, so an order between rules couldn't order the results anyway;
   "otherwise" logic belongs in `when`.
4. `automations.rule.fired {rule, event, tasks}` is emitted, and a line goes to the history (§8).
5. **No retry** (§11.2). A failed action is `queue.task.failed` like any other task: visible, in the
   log, and the lane moves on (§11.1).

### 3. Loops: events carry their cause

The risk: "when a record is updated, update a record" re-triggers itself forever, a task (and maybe
a popup) each time; or A triggers B triggers A.

- **Every event emitted while a queued task runs carries that task's id** (`Event.cause`, §5.1).
- Automations remembers each task it enqueued as `task id → (rule, chain)`. An event whose `cause`
  is one of those tasks continues that chain:
  - **a rule already in the chain doesn't fire again** (a cycle: A → B → A);
  - **a chain stops at the depth limit: 3 by default**, adjustable from 1 to 10 in automations'
    settings (`shimmer automations depth N`, §8).
- Events with no automations cause start a **new chain**: anything a person does, and anything a
  schedule fires. Real flows stay shallow because of this: in the job-hunt flow the deepest point is
  2 (a posting found → saved → told and opened), since applying and marking done each start fresh.
- **A delayed action continues its chain** (§4): it carries the chain while it waits, so a rule
  whose delayed action re-causes its own event ("re-set the stage in 3 days") is stopped as a
  cycle, not run once per delay forever.
- **Where chains are kept:** the `task id → chain` map is in memory, bounded to the 10,000 newest
  tasks (a chain only matters while its task is queued or running, and the queue itself is in
  memory, #236). A delayed action's chain is stored with its schedule instead, so it survives a
  restart.
- A stopped chain emits `automations.chain.stopped {chain, rule, event}`, and the built-in rule
  `loop-stopped` (§10) turns that into a high-priority message. It's also marked where you'd look:
  `⚠ stopped a loop` in `automations list`, a line in `automations status`, a warning in doctor.
- **Your rules can't react to `automations.*` events** (refused when created), so "a rule fired"
  can never trigger a rule. The one exception is the built-in `loop-stopped`, which reacts to
  `automations.chain.stopped`; its action (`notify.send`) never emits an `automations.*` event, so
  it can't start a loop itself.

### 4. Delayed actions: "in 3 days"

An action may wait:

```toml
[[do]]
op = "notify.send"
params = { title = "Follow up with the recruiter: {title}" }
after = "3d"                      # 30m, 2h, 3d, 1w, 60d…
```

- The params are filled **when the rule fires** (the record as it was then), and the action becomes
  a **one-time schedule** owned by automations (§5.6), due after the wait. The scheduler does the
  waiting: it survives restarts and sleep, and catches up with `run-once` if the computer was off
  when it came due.
- **It always fires** after the wait. Cancelling it when what it was about has changed (you already
  followed up, the record was removed) is a later refinement, its own issue.
- **Pending ones are yours to see and remove:** they show **by default** in `shimmer scheduler list`
  (not only under `--all`, #244), marked `from: automation <rule>`, and `scheduler remove` removes
  one.
- **It's scheduled as automations' own `automations.deferred` op**, carrying the rule, the action's
  op and params, and the chain (§3). When it comes due, automations queues the action and
  remembers its task with that chain, so the loop guard sees through the wait. A rule removed or
  switched off meanwhile does nothing. Rules can't name `automations.*` ops, so only automations
  schedules it.

### 5. What `core` gains (Dev A)

1. **`Event.cause`:**
   ```rust
   pub struct Event {
       // …existing fields…
       /// The queued task this event was emitted from, if any. Omitted when none.
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub cause: Option<TaskId>,
   }
   ```
   `Ctx::for_task` takes the task id, and every event emitted while that task runs is stamped with
   it, through `ctx.bus` and store transactions' `tx.emit` alike. Additive: the event log, the bus
   and the IPC stream gain an optional field; old lines and old clients are unaffected.
2. **Topic patterns:** `is_valid_pattern` and `topic_matches` move from `crates/proto/src/topic.rs`
   to `core`; `proto` re-exports them, so clients are unchanged.
3. **The filter operators:** records' matcher (`crates/modules/records/src/query.rs`) is
   schema-aware. `core::filter` holds the **same operators over plain JSON values** (numbers compare
   as numbers, dates as ISO text, `has`/`contains` ignore case as in records); records keeps its
   schema checks on top and calls it, with behaviour and tests unchanged. The top-level `or` of
   alternatives moves with them, so rule `when` clauses and today's `{"or": [...]}` record
   filters both have it. Without a schema, enums compare as text, so rules use `in` for enum
   values.
4. **The text renderer:** records' `{field}` / `[optional part]` renderer (`render_title`,
   `title_parts`, `plain_parts` in `crates/modules/records/src/schema.rs`) moves to `core::text` as
   `Template::parse(&str)` / `Template::render(&Map<String, Value>)`. Records calls it, unchanged.
5. **Event descriptions:** `Manifest` gains an optional `topic_descriptions: BTreeMap<String,
   String>` beside `topics` (topic → one line, e.g. "a record was marked done"), filled by each
   module, shown by `automations events`. Chosen over turning `topics` into name-plus-description
   entries, which would break every reader of the manifest (mockd's fixtures, clients); a topic with
   no description shows "–". If this is refused, `events` shows names only.
6. **One-time schedules a module adds while running.** Today a module's schedules are only the fixed
   ones `Module::triggers()` declares at start, and the daemon drops module schedules no longer
   declared (`register_module_triggers`). `Ctx` gains a small handle to **add** a one-time schedule
   (op, params, time, label) and **remove** one of its own: ids carry the module's name, a module
   can never touch another's or a person's, and the daemon keeps them across restarts (marked as
   added at run time, so start-up doesn't drop them). `scheduler list` shows them with their label.
   - **Chosen: every Shimmer module gets it.** Only Shimmer's own modules run inside the daemon and
     get a `Ctx`; any of them can already enqueue any op now (`ctx.queue`), and this is the same
     power, later. External plugins are separate programs talking over the protocol (§12 rule 8),
     never through `Ctx`, so gating this would gate nothing.
   - **Not chosen: behind a capability** (`"schedule"`, like `"process"`, `"network"` and
     `"desktop"`). Revisit if non-Shimmer code ever runs inside the daemon: persistence is a
     different power from "now", and a capability would make it visible in a manifest.
   - It's also what a calendar or reminders feature would use.
   - **Not chosen: calling `scheduler.add`.** A module can't: `ctx.queue` takes module ops, and
     `scheduler.*` are the daemon's own ops, reached only over the socket. And a `scheduler.add`
     trigger is a person's (`usr-…`), removable by anyone and not tied to the module, with no
     per-module cap. These need the module's name in the id, removal by that module only, and a
     limit of 1,000 waiting per module, so a runaway rule fills its own quota, not the disk.
7. **A read-only catalog of ops and topics.** A module can't see `core.manifest`, yet automations
   checks that a rule's ops exist (§1) and lists what a rule can react to (`automations events`).
   `Ctx` gains `catalog`: every module op (its `CommandSpec`) and every topic with its description,
   the daemon's own `queue.*` and `scheduler.*` topics included. Fixed at daemon start, as the
   registry is. It grants nothing: it's what `core.manifest` already tells any client.
   - **Not chosen: checking in the CLI.** No `core` change, but clients would hold logic (§2), and a
     rule edited by hand would skip the check and only fail when it fires.

### 6. Trust: the same as a schedule

A rule runs what its author wrote, as a schedule does (ADR 0031): any op, **except `automations.*`**
(a rule that writes, removes or switches rules is a recipe for runaway behaviour; refused when the
rule is created). Ops that need a confirmation (`records.purge`, `notify.remove` in bulk…) answer
`confirmation_required`, which a rule can't give, so they fail visibly instead of running.

**Per-rule permissions come later, with #228.** Today every rule is yours. They'll matter when rules
come from elsewhere: a kit or marketplace bundle (#79, #155), or an assistant writing rules for you
(#239). A "Job hunt kit" rule meant to save postings (`records.add`) shouldn't also be able to start
a workspace (which runs scripts on your machine); with permissions, rules from that kit, or from the
assistant, could be limited to e.g. `records.*` and `notify.send`, and anything else fails as "not
allowed". Until then, `automations list` and `show` make every rule's actions plain to see.

### 7. Settings

Automations' own file, `data/automations/settings.toml`, as notify's (ADR 0032 §3): changed by
commands at once, no restart, still hand-editable.

```toml
depth = 3                 # §3, 1–10
[builtin]                 # §10: each built-in rule on unless switched off
workspace_dirty = true
```

### 8. Ops, topics, CLI

| Op | Execution | Params | Returns |
|---|---|---|---|
| `automations.list` | inline | `{}` | `{"rules":[…]}`: each with on/off, built in or not, when it last fired, `loop_stopped_at`? |
| `automations.get` | inline | `{"id"}` | one rule, and its file's text |
| `automations.create` | inline | `{"id","template"?,"values"?,"label"?}` | the rule; with no `template`, a commented starter rule (disabled until edited) |
| `automations.save` | inline | `{"id","text"}` | the rule, after checking it; a broken one is refused and the old one kept |
| `automations.remove` | inline | `{"id"}` | `{"removed": true}`; built-in rules refuse (disable them) |
| `automations.enable` / `.disable` | inline | `{"id"}` | the rule; works for built-in rules too |
| `automations.reload` | inline | `{}` | re-reads the rule files after hand edits: `{"rules": N, "problems": […]}` |
| `automations.test` | inline | `{"id","event"}` | what the rule would enqueue for that sample event, without enqueuing it |
| `automations.templates` | inline | `{"id"?,"file"?}` | as `records.templates` |
| `automations.events` | inline | `{}` | every topic in `core.manifest`, with its description |
| `automations.history` | inline | `{"id"?,"since"?,"limit"?}` | recent firings, newest first: rule, event, what it queued or scheduled, stopped loops |
| `automations.status` | inline | `{}` | rules (on/off/built in), the depth limit, recent stopped loops, broken rule files |
| `automations.depth` | inline | `{}` shows; `{"depth": N}` sets (1–10) | `{"depth"}` |
| `automations.health` | inline | `{}` | ADR 0033's shape: unreadable rules, rules naming ops that no longer exist, recent stopped loops |

Topics: `automations.rule.created`, `.edited`, `.removed`, `.enabled`, `.disabled`, `.fired`;
`automations.chain.stopped`; `automations.settings.changed`.

**History** is automations' own small file (the last few hundred firings, oldest dropped, like
notify's inbox cap), since a module can't read the event log; the full record stays in the event log.

```
shimmer automations [list]                         your rules, built-in ones marked, ⚠ where a loop was stopped
shimmer automations show ID
shimmer automations new ID [--from TEMPLATE [--set Q=A]…]
                                                    from a template, or (without --from) a starter
                                                    file opened in your editor
shimmer automations edit ID                        open its file in your editor; checked on save
shimmer automations remove ID
shimmer automations enable ID | disable ID
shimmer automations test ID --event FILE           dry run against a sample event
shimmer automations templates [CATEGORY] | peek TEMPLATE
shimmer automations events                         everything a rule can react to
shimmer automations history [ID] [--since DATE]    what your rules did
shimmer automations status
shimmer automations depth [N]
shimmer automations reload                         after editing rule files by hand
```

- **Writing and editing:** `new` without `--from` and `edit` open the rule in your editor (as
  `records edit` does); on save the CLI sends the text with `automations.save`, which checks it and
  refuses a broken rule with what's wrong, keeping the old one, so a half-written rule never runs.
  A file changed by hand outside these commands counts after `automations reload`; watching files
  automatically needs a file-watching ability modules don't have, and can come later.
- Pack targets and aliases for each command (ADR 0029), about twelve. Creating a rule shows ADR
  0034's one-time note: rules only run while the daemon runs.

### 9. Templates

In the records/workspaces template format (ADR 0025 §1, ADR 0030 §1, #202 later): a header with
notes and examples (the lines under `How it works:`), a category, and `[[question]]`s whose answers
fill the rule. Every template is
tested end to end, and one asking consequential questions shows the rule before creating it (ADR
0025 §6a).

| Template | Makes a rule that… |
|---|---|
| `notify-on-add` | messages you when a record is added to a collection |
| `notify-on-complete` | messages you when a record is done |
| `notify-on-failure` | messages you when a task fails (optionally one lane) |
| `stage-message` | messages you when a field reaches a value (offer, rejected…) |
| `field-threshold` | messages you when a number field goes over a limit (attempts > 3) |
| `follow-up-reminder` | N days after a record is done, reminds you (a delayed action, §4) |
| `spaced-review` | N days after a record is done, reminds you to redo it (§4) |
| `stage-opens-workspace` | opens a fixed workspace when a field reaches a value |
| `stage-creates-workspace` | makes a workspace **for that record** from a workspace template, with answers filled from the record, opens it, and stops it when the record closes |
| `posting-to-application` | saves a fetched posting as a record (ships with fetchers) |

`stage-creates-workspace` makes **three rules** from one set of answers, because creating and
opening must happen in order and a rule's actions start together (§1). `when` is a condition on the
record as it is now, so each rule also needs `changed = "<field>"` (records' update event names the
fields that changed): without it, any edit to a record already at that stage would fire again. If a
record comes back to the stage later, `workspaces.create` meets the workspace it made before and
refuses (`conflict`, recorded as a failed action in the rule's history); the existing workspace is
left as it is.

```toml
# 1: the record reaches the stage → its own workspace, from a workspace template
on = "records.item.updated"
when = { collection = "jobs", "item.stage" = "interviewing", changed = "stage" }
[[do]]
op = "workspaces.create"
params = { id = "{id}-prep", template = "interview-prep", values = { COMPANY = "{title}", JOB_URL = "{item.url}" } }
```
```toml
# 2: that workspace was created → open it (a chain of 2, §3)
on = "workspaces.workspace.created"
when = { template = "interview-prep" }
[[do]]
op = "workspaces.activate"
params = { id = "{workspace}" }
```
```toml
# 3: the record closes → the workspace stops (its cleanup runs; it stays in your list)
on = "records.item.updated"
when = { collection = "jobs", "item.stage" = { in = ["offer", "rejected", "withdrawn"] }, changed = "stage" }
[[do]]
op = "workspaces.stop"
params = { id = "{id}-prep" }
```

Rule 2 opens **any** new workspace made from that workspace template, one you make yourself
included; the template says so when you create it. Rule 3 **stops** the workspace rather than
removing it: `workspaces.remove` refuses a workspace
that's active (ADR 0026), and rule 2 has opened it, so a remove would fail every time. A rule's
actions start together, so "stop, then remove" can't be one rule; a fourth rule removing any
stopped `*-prep` workspace was considered and rejected, since it would also remove one you stop by
hand. Removing it stays yours (`shimmer workspaces remove`), from a list where it's `ready`.

**Two walkthroughs** go in the README as "try this", built from these templates: the job hunt and
LeetCode flows of the Context (the LeetCode one's "weekdays at 09:00" is a schedule, `scheduler add
weekdays 09:00 … workspaces activate practice`, since rules react to events, not times). When kits
(#79) arrive, each walkthrough becomes one kit.

### 10. Built-in rules, and notify's alerts moving here (amends ADR 0032 §4)

**Built-in rules** ship with Shimmer: on by default, shown in `automations list` marked built in,
switched with `disable` / `enable` (stored in `[builtin]`, §7), never removable, written in the same
rule language as yours (`automations show` shows them). They aren't templates: templates are
optional starting points; built-in rules are always there.

| Built-in rule | Reacts to | Message (high) |
|---|---|---|
| `workspace-dirty` | `queue.task.failed` with code `workspace_dirty`, `fallback` false | "site couldn't launch: step 2/5 failed. Log: …. Then: {detail.fix}" |
| `workspace-half-launched` | `workspaces.session.dirty` with `recovered` true | "site was left half-launched…" |
| `scheduled-task-failed` | `queue.task.failed`, origin scheduler, not `workspace_dirty`, `fallback` false | "Scheduled records.complete failed: …" |
| `scheduled-task-interrupted` | `scheduler.trigger.interrupted` (ADR 0004 amendment, #263) | "Scheduled notify.digest was cut off: … not run again" |
| `loop-stopped` | `automations.chain.stopped` | "Automation X was stopped: it would have triggered itself" |

- **The fields they match exist on the event:** `queue.task.failed` carries `code` (protocol.md's
  error codes) and `fallback` (ADR 0032 §4) besides CLAUDE.md §11.1's summary of task id, lane,
  origin, error and attempts. `workspaces.session.dirty` carries `recovered: true` when it reports a
  launch the daemon never finished (found at start-up); that came with notify (ADR 0032 §4, #245).
  Like every built-in that messages you, `workspace-half-launched` loads only once notify is
  installed, so it never runs without the field.
- **Notify's own work is never alerted on:** notify's tasks run in the `notify` lane, so the first
  three add `lane ≠ notify`, as notify's alert code did (ADR 0032 §4).
- **The exact fix in the message:** workspaces' `workspace_dirty` detail gains `fix`, the command
  that clears it: `shimmer workspaces cleanup site` when it has a cleanup script, `reset` when it
  doesn't (workspaces knows which). The rule shows `{detail.fix}`; doctor and the CLI can use the
  same field. Additive, in the workspaces module.
- **When automations ships, notify loses** its alert code, the `[builtin_alerts]` part of its
  settings, and the `builtin-alerts` command and op with their pack aliases. Nothing is carried over
  or redirected: no released Shimmer ever had notify's alerts. Everything else in notify stays.
- **"Message me when X" is an automation** whose action is `notify.send` (`notify-on-complete` and
  friends, §9); ADR 0032 §5 already leaves those here.

### 11. How it keeps CLAUDE.md's rules

| Rule | How |
|---|---|
| §1.3 Decoupled; no module calls another | Automations never knows a module: topics and ops are strings from a rule, like a schedule's op. Delete workspaces: a rule naming `workspaces.activate` fails when it fires, visibly; nothing fails to compile. Delete automations: nothing else notices. |
| §1.3 Plugins work the same way | It subscribes and enqueues through the same channels, with no special access. |
| §1.4 Files are the truth | Rules, settings and history are files; each change is the file plus its event in one transaction (§7.1). |
| §1.5 No identity | Rules belong to the Shimmer folder, not to a user. |
| §1.6 Extensible | A new reaction is a file, not code. |
| §2 Clients hold no logic | Matching, filling, enqueuing and checking rules are in the module; the CLI shows, opens the editor and sends. |
| §3 rules 1–2 | Depends on `core` only; what it needs moves into `core` (§5). |
| §5 `Ctx` is a security decision | Two new fields: one-time schedules (§5.6), same power as `ctx.queue`, later, for Shimmer's own modules only; and `catalog` (§5.7), read-only, what `core.manifest` already tells any client. `Event.cause` is data, not a power. |
| §6.2 Ordering | Enqueued like any task, in the op's lane; no `Overridden`. |
| §11.1–11.2 Failure, no retry | A failed action is an ordinary `queue.task.failed`; automations never retries. |
| §12 rule 4 | No `std::fs`, no network, no `Utc::now()`: store, queue, schedules and clock through `Ctx`. |
| §12 rule 10 | Matching, filling and the loop guard are pure functions, tested without a runtime. |
| §12 rule 13 | Tests against the in-memory `Ctx`, including a stopped loop and a delayed action. |
| §12 rule 16; §15 | Canonical op names only; aliases never reach a rule. |

## Consequences

- People connect modules without code: developers write mechanisms, people write workflows. The job
  hunt and LeetCode flows are configuration.
- One rules system: notify keeps delivery only; its alerts become built-in rules.
- `core` gains `Event.cause`, three moved helpers, `Manifest.topic_descriptions`, one-time module
  schedules and `Ctx.catalog`; the daemon stamps causes and keeps those schedules; workspaces adds `fix` to its dirty
  detail; CLAUDE.md §1.1 gains a row: `automations` · owns: reactions between events and ops,
  written as data · specialised by: rule templates.
- Built after sign-off, in small PRs into an `integration/automations` umbrella, one at a time: the
  `core` changes (§5) → the module: rules, matching, enqueuing, the loop guard → delayed actions →
  settings, history, status → the CLI → templates → built-in rules and notify's alerts moving here
  (after notify is in master) → docs.

## Not done here

- **Cancelling a delayed action when what it was about changes** (`unless`, cancel-on events,
  replace instead of stack): its own issue.
- **Per-rule permissions:** #228 (§6).
- **Rules on a clock** ("if nothing happened by 8pm, remind me"): rules react to events, and an
  absence isn't an event; a scheduled check could come later.
- **Opening a workspace with values for one run** (`activate` with answers, without making a
  workspace): a workspaces change to the script interface, its own ADR if wanted;
  `stage-creates-workspace` covers the common case today.
- **Watching rule files for hand edits:** `reload` until modules can watch files.
- **Kits** (#79): bundles of a collection, workspace, schedules and rules; the walkthroughs become
  kits then.

## Alternatives considered

- **A core service instead of a module.** Rejected: nothing here needs the daemon's internals, and a
  module is what a plugin would be (§1.3).
- **Scripts on the socket** (a workspace step running `shimmer` when something happens). Works today
  for one-offs, but needs a program running and polling; rules are data, checked, listed and
  switched off in one place.
- **Each module owning its own reactions.** Rejected: reactions between modules have no owner
  without coupling them (§1.3).
- **Replaying events missed while the daemon was down.** Rejected: nothing changes while it's down
  (every change goes through it), and replay could repeat actions.
- **An automations lane.** Rejected: it would let a rule's launch run alongside a person's (§2).
- **An order between rules / "first match wins".** Rejected: actions run in independent lanes, so
  rule order can't order results (§2).
- **Ordered actions in one rule** ("then, after that…"). Rejected for now: workspaces already run
  steps in order with fail-stops, and two rules chaining through an event cover the rest (§1, §9).
