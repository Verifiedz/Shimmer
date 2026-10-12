# 0032. Notify: an inbox and desktop notifications, built for more ways to reach you

Status: proposed · Raised by Dev B · Needs sign-off: Dev A (§1 adds a `Ctx` capability and moves a
helper into `crates/core`; §2 is daemon code; §1 and §7 change CLAUDE.md and `docs/protocol.md`,
CLAUDE.md §4) · Dev C notified (new ops and topics, §5–§6)

## Context

The MVP needs fetchers, notify and calendar: the chain in CLAUDE.md §1.3 ends with "`notify`
delivers it". Today nothing can tell the user anything:

- A scheduled launch that fails `dirty` is silent, though §10.3 says the user is "notified at
  elevated priority".
- ADR 0005 lets a scheduled task carry a fallback, and only to a `notify.*` op. With no notify
  module, every fallback ends in `queue.fallback.failed` (`unknown_op`).
- Nothing outside the terminal says a record was added or a task failed.

Constraints this has to meet:

- **No network and no account** for the core product (§1.5): the first ways to reach the user must
  work offline.
- **Modules never run programs** (§12 rule 4), but a desktop notification needs one (`notify-send`,
  `osascript`) or the OS's message bus.
- **No module calls another** (§1.3): notify learns what happened from events, and other modules
  reach it only through the queue (ADR 0005's fallbacks) or events.
- **Files are the truth** (§1.4): what was sent, and what couldn't be, is a file someone can read.
- **Modules write only inside their own namespace** (`data/<module>/`): §7's
  `notifications/failed.jsonl`, at the root of `$SHIMMER_HOME`, can't be written by a module.

Decided before this ADR (with the team, in the planning for this):

- The MVP's ways to reach you are **desktop notifications and an inbox in Shimmer**, working as one
  system. Phone (ntfy), email, chat webhooks and the rest come later, each as one small sink.
- Notify templates use **the same format as records' and workspaces'** for now; #202 makes one
  shared convention for every module after the MVP.
- **When** to remind ("a day before the interview") is calendar's and the scheduler's job, not
  notify's. Calendar is its own ADR, later.

## Decision

### 1. `core`: a desktop capability, and the text renderer shared

**`Ctx.desktop`**, the same pattern as `Ctx.launcher` (ADR 0010) and `Ctx.http` (ADR 0027):

```rust
pub struct DesktopNote { pub title: String, pub body: String, pub urgency: Urgency }
pub enum Urgency { Low, Normal, Critical }

#[async_trait]
pub trait DesktopBackend: Send + Sync {
    async fn show(&self, note: &DesktopNote) -> Result<()>;
}

pub struct Desktop(Arc<dyn DesktopBackend>);   // in crates/core/src/desktop.rs
impl Desktop {
    pub fn unavailable() -> Self;              // every call fails `unavailable`
    pub async fn show(&self, note: &DesktopNote) -> Result<()>;
}
```

- `Ctx::new` gives every module `Desktop::unavailable()`. The daemon replaces it only for a module
  whose manifest declares the `"desktop"` capability (`crates/daemon/src/core.rs`, beside
  `"process"` and `"network"`). Fails closed, like the others.
- A module can show a note and nothing else: no program, no path, no handle.
- `crates/core/src/testing.rs` gains `FakeDesktop`, which records every note shown (or fails, on
  request), for module tests against the in-memory `Ctx` (§12 rule 13).

**`core::text`**: records' `{field}` / `[optional part]` renderer (ADR 0017 §2, ADR 0024 §5;
`render_title`, `title_parts` and `plain_parts` in `crates/modules/records/src/schema.rs`) moves to
`crates/core/src/text.rs` as `Template::parse(&str)` and `Template::render(&Map<String, Value>)`.
Records calls it; its behaviour, errors and tests are unchanged. Notify renders its messages with
it, so a message template and a record title read the same way. Two modules needing the same
helper is the case ADR 0014 put helpers in `core` for.

### 2. Daemon: the real desktop backend

`crates/daemon/src/desktop.rs`, one backend shared by every module that has the capability:

- **Linux:** `notify-send --app-name=Shimmer --urgency=<low|normal|critical> -- <title> <body>`
  (the `libnotify-bin` package; most desktops have it).
- **macOS:** `osascript -e 'on run argv' -e 'display notification (item 2 of argv) with title (item
  1 of argv)' -e 'end run' <title> <body>`. The text is passed as arguments, never put into the
  script, so nothing in a message can run as AppleScript.
- `std::process::Command` with arguments, no shell; a 5-second timeout.
- A missing tool, or no desktop session (Shimmer started over SSH), is `unavailable`, with a message
  that says which.
- **`config.toml` `[desktop] show = false`** logs each note to the daemon log instead of showing it:
  for machines with no screen, and for the end-to-end tests, which must never pop anything up on a
  developer's screen.

### 3. The `notify` module (`crates/modules/notify/`)

**Manifest:** id and namespace `notify`; capabilities `["desktop"]`; one lane, `notify`, with
`max_concurrent = 2` (§6.1: it must never be starved by long fetches, because fallbacks depend on
it).

**The inbox is the record of every message.** One TOML file per message,
`data/notify/inbox/<id>.toml`, the id a ULID:

```toml
id = "01M4…"
at = 2026-10-14T13:00:00Z
priority = "high"            # low | normal | high
title = "site couldn't launch"
body = "Step 2/5 (install) failed. Log: logs/site-01M4….log"
source = "alert:workspace_dirty"   # send | fallback | alert:<name> | rule:<id>
state = "delivered"          # pending | held | delivered | failed
read = false

[[delivery]]
medium = "desktop"
at = 2026-10-14T13:00:01Z
ok = true
```

- Every message lands in the inbox **first**; desktop is one delivery of it. A message no medium
  could deliver stays, with state `failed`: nothing is ever dropped (§11.3).
- This replaces §7's `notifications/failed.jsonl`: a failed message is an inbox file like any other,
  inside notify's own namespace. CLAUDE.md §7 and §11.3 are amended to say so.
- The inbox keeps the newest 1000 messages; older **read** ones are removed in the same transaction
  that adds a new one. Unread messages are never removed.

**Sinks** (§8): one trait, one small `impl` per way of reaching you.

```rust
#[async_trait]
trait Sink: Send + Sync {
    fn id(&self) -> &str;                       // "desktop"
    fn fallback(&self) -> Option<&str>;         // another sink's id, tried once
    async fn deliver(&self, message: &Message, ctx: &Ctx) -> Result<()>;
}
```

The MVP has one sink, `desktop` (through `ctx.desktop`). A later ntfy, email or webhook sink is one
more `impl` in the module's registry, reaching the network only through `ctx.http`, retrying with
`ctx.retry_with_backoff` (the queue never retries, §11.2). Which priorities go to which sinks becomes
configuration when there is more than one; today every priority goes to desktop.

**Secrets** (later sinks only; the MVP has none): `config.toml` holds only the *name* of an
environment variable or a keychain/keyring entry, never the secret, because the Shimmer folder is
meant to be committable to git (§1.4). The VM template's `SSH_PASSWORD_ITEM` works this way already.

### 4. Delivery, quiet hours and alerts

**`notify.send`** (queued, lane `notify`):

```
{"title", "body"?, "priority"?: "low"|"normal"|"high", "rule"?, "fields"?}
```

1. Build the message: `title` and `body` as given; or, with `rule`, that rule's templates rendered
   with `fields`. A call from ADR 0005's fallback carries `failure_code`, `failure_reason` and
   `failure_detail`; with no `title`, notify writes one from them ("Scheduled task failed: …"). The
   `priority` key the queue stamps is the message's priority.
2. Write it to the inbox (`notify.message.received`).
3. **Quiet hours:** if it's `low` or `normal` and now is inside them, mark it `held`
   (`notify.message.held`) and stop. `high` always goes through.
4. Deliver to each sink for its priority; record each delivery on the item; then
   `notify.message.sent`, or, when every sink (and its fallback) failed, state `failed` and
   `notify.message.exhausted`.
5. **The task succeeds even when delivery fails**: the inbox has the message, so the queue has
   nothing to retry or report as a failure.

**Quiet hours:** `[modules.notify] quiet_hours = "22:00-08:00"`, in `[general] local_timezone`
(ADR 0009), and may cross midnight. Unset means none. When quiet hours end, the held messages become
**one** summary popup ("3 messages while you were away: …") and are marked delivered.

- The summary is sent by **`notify.flush`** (queued, lane `notify`), run by a module trigger every
  hour (`Schedule::Every`, `CatchUp::Skip`), on daemon start, and on the first event notify sees
  after quiet hours end. A trigger can't be set for the configured end time itself:
  `Module::triggers()` runs before `init`, with no config. So the summary is at most an hour late,
  and the event log gets four entries an hour, not hundreds. When calendar brings a way for a module
  to set its own one-time triggers, this becomes one trigger at the end of quiet hours, only on
  nights with held messages.

**Built-in alerts**, on by default (a fresh install should still warn about a dirty workspace,
§10.3), each one off with `[modules.notify.alerts]`:

| Alert | When | Priority |
|---|---|---|
| `workspace_dirty` | `workspaces.session.dirty` | high: "site couldn't launch: step 2/5 failed (see log)" |
| `scheduled_task_failed` | `queue.task.failed` with origin `scheduler`, no fallback, and code not `workspace_dirty` (that one has its own alert) | high |

- **`queue.task.failed` gains `"fallback": true|false`** (additive, daemon side), so the second alert
  can skip a task whose fallback already sends its own message. Without it, a failed scheduled launch
  with a fallback would notify twice.
- From `on_event`, notify **enqueues** `notify.send` (`ctx.queue`); it never delivers inline. A slow
  popup never holds up the event bus, and every send shows in `shimmer queue`.
- **Notify never reacts to its own events (`notify.*`), or to the failure of a `notify.*` task**, so
  a failing delivery can never set off a message about itself.

### 5. Rules and templates

A **rule** says "when this happens, send this message". It's a file the user can read and edit,
`data/notify/rules/<id>.toml`:

```toml
# Created from Shimmer's record-done template
[rule]
id = "leetcode-solved"
label = "A LeetCode problem solved"
on = "records.item.completed"      # a topic, or a pattern like records.item.*
when = { collection = "leetcode" } # payload fields that must match (optional)
priority = "low"

[message]
title = "Solved: {title}"
body = "[{difficulty}, ]{collection}/{id}"
```

- `on` takes the same patterns as an IPC subscription (`a.b.c`, `a.*`, `*`; `docs/protocol.md`).
- `when` matches the event's payload by equality; every key must match.
- The message's `{field}` and `[optional part]` are filled from the event's payload, with
  `core::text` (§1). For record events (whose payload is `{collection, id, title, deadlines, item}`),
  the record's own fields from `item` are available directly too (`{difficulty}`); where a name is
  in both, the payload's wins, so `{title}` is the record's readable title (ADR 0017).
- Rules load at `init` and reload when one is created or removed. They're matched in `on_event`, as
  the alerts are, under the same never-react-to-notify rule.

A **template** makes a rule, in the format records' and workspaces' templates use: one TOML file in
`crates/modules/notify/templates/`, a header comment with notes and "How it works" examples, a
category, and `[[question]]` blocks as in workspace templates (ADR 0025 §3). The answers replace
`${NAME}` in the rule when it's created, so the stored rule is plain and hand-editable (e.g. `when =
{ collection = "${COLLECTION}" }`). Built in, kept few (the template debloat comes right after the
MVP):

| Template | Makes a rule for |
|---|---|
| `record-done` | a record completed in one collection |
| `record-added` | a record added to one collection |
| `task-failed` | any task failing, optionally in one lane |
| `workspace-launched` | a workspace launched (low priority) |

Every built-in template is tested end to end, as ADR 0025 §6a requires of workspace templates.

### 6. Ops and topics

| Op | Execution | Params | Returns |
|---|---|---|---|
| `notify.send` | queued (`notify`) | §4 | the inbox item, with its deliveries |
| `notify.flush` | queued (`notify`) | `{}` | `{"delivered": N}` |
| `notify.list` | inline | `{"unread"?, "state"?, "limit"?}` | `{"items":[…]}`, newest first |
| `notify.get` | inline | `{"id"}` | one item |
| `notify.mark_read` | inline | `{"ids":[…]}` or `{"all": true}` | `{"marked": N}` |
| `notify.templates` | inline | `{"id"?, "file"?}` | `{"templates":[…],"categories":[…]}`, as `records.templates` |
| `notify.create_rule` | inline | `{"id","template","values"?,"label"?}` | the rule |
| `notify.rules` | inline | `{}` | `{"rules":[…]}` |
| `notify.remove_rule` | inline | `{"id"}` | `{"removed": true}` |

Topics: `notify.message.received`, `.held`, `.sent`, `.exhausted`, `.read`; `notify.rule.created`,
`.removed`.

**These replace the names CLAUDE.md and `docs/protocol.md` use for notify today** (`notify.sent`,
`notify.fallback_used`, `notify.exhausted`). `core` requires a topic to have at least three
segments, `<module>.<entity>.<verb>` (`validate_topic`, `crates/core/src/bus.rs`), so a two-part
name like `notify.sent` could never be emitted. CLAUDE.md §1.3 and protocol.md's topic catalogue are
amended. `notify.message.fallback_used` comes with the first sink that has a fallback.

### 7. CLI and docs

```
shimmer notify list [--unread] [--failed]     the inbox, newest first
shimmer notify show ID                        one message in full (marks it read)
shimmer notify read [ID…|--all]               mark as read
shimmer notify send TITLE [--body TEXT] [--priority low|normal|high]
shimmer notify test                           send "Shimmer can reach you" to every medium
shimmer notify templates [CATEGORY] · peek TEMPLATE · new ID --from TEMPLATE [--set Q=A]…
shimmer notify rules · remove-rule ID
```

- Built from the existing pieces: `render::table`, the templates listing and `peek`, workspace
  templates' question prompts, the colour roles (#189) for state (held = warning, failed = error).
- Every command gets a pack target and an alias in each built-in pack (ADR 0029).
- `notify send` is one op, so `shimmer schedule` can schedule it once ADR 0031 lands:
  `shimmer schedule weekdays 09:00 --catch-up skip notify send "Standup in 15 minutes"`.
- Docs: `docs/protocol.md` (§6's ops and topics, `queue.task.failed`'s `fallback`), mockd's fixture
  manifest, CLAUDE.md §5 (`Ctx.desktop`), §7 (`data/notify/`), §11.3, the README.

## Consequences

- Shimmer can reach the user with no network and no account; nothing it sends is lost.
- A scheduled task's fallback works for the first time (ADR 0005), and a dirty workspace warns as
  §10.3 asks.
- New ways to reach you are one sink each; new things to be told about are one rule or template,
  with no code.
- `core` gains one capability and one shared helper; the daemon gains one backend and one field on
  an event. Records' titles move onto the shared helper, unchanged.
- Built in this order, stacked in an `integration/notify` umbrella: this ADR → `core` (§1) → the
  daemon backend and `fallback` flag (§2) → the module: inbox, send, desktop, quiet hours, alerts,
  and the CLI for them (§3–§4) → rules and templates (§5) → docs (§7).

## Not done here

- **Reminders before a date** ("a day before the interview"): calendar's ADR. It needs a way for a
  module to set and remove its own one-time triggers, a `core` question for Dev A and Dev B.
- **Phone (ntfy), email, chat webhooks, Telegram, Pushover**: one sink each, with secrets by
  reference (§3).
- **Batching** beyond the quiet-hours summary, and routing priorities to particular sinks: config
  that waits until there's more than one sink.
- **An inbox view in the TUI**: Dev C's, built on `notify.list` and the `notify.message.*` topics.
- **Moving notify templates to #202's shared convention**, after the MVP.

## Alternatives considered

- **The module shows popups itself** (a crate like `notify-rust` talking to the OS's message bus).
  Rejected: it's I/O around `Ctx`, the thing §12 rule 4 forbids, and a module test would pop real
  notifications.
- **Show popups through the workspace launcher.** Rejected: the launcher runs a workspace's own
  scripts, by a workspace-relative step; bending it to run `notify-send` weakens the boundary ADR
  0010 drew.
- **Let clients (CLI, TUI) show the popups** from the event stream. Rejected: with no terminal
  open, nothing would ever reach you.
- **Keep `notifications/failed.jsonl`.** Rejected: a module can't write outside its namespace, and
  a second place for the same messages is a second thing to keep in step with the inbox.
- **Deliver inline from `on_event`.** Rejected: a slow or failing popup would hold up the event bus,
  and sends wouldn't show in the queue (§1.2).
