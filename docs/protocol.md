# IPC protocol

Version `1`. Change-controlled — see `CLAUDE.md` §4.

This document is the complete contract between the daemon and any client. A client
implemented only against this file must work. If you need to read daemon source to build a
client, this file has a gap; fix the file.

---

## Transport

Unix domain socket, stream mode.

| Platform | Socket path |
|---|---|
| Linux | `$XDG_RUNTIME_DIR/shimmer/daemon.sock`, falling back to `$SHIMMER_HOME/daemon.sock` |
| macOS | `$SHIMMER_HOME/daemon.sock` |
| Windows | Named pipe `\\.\pipe\shimmer-daemon` (deferred; not before M6) |

The path is also exported to workspace scripts as `SHIMMER_SOCKET`.

If the socket file exists but connecting returns `ECONNREFUSED`, the daemon died without
cleaning up. Clients should unlink it and start a daemon.

## Framing

Newline-delimited JSON. One complete JSON object per line, UTF-8, terminated by `\n`.
Objects must not contain literal newlines — JSON string escaping handles this.

Chosen for debuggability: a whole session can be driven from a shell.

```
$ socat - UNIX-CONNECT:$SOCK
{"v":1,"kind":"hello","client":"manual","client_version":"0.0.0"}
```

`$SOCK` is the socket path from the Transport table above. `socat` behaves the same on every
platform. `nc -U $SOCK` also works with the BSD/macOS and OpenBSD netcat, but not with
netcat-traditional (the `nc` on some Linux distributions), which has no `-U`.

Max line length 8 MiB. A longer line is a protocol error and the daemon closes the
connection.

---

## Handshake

The client sends `hello` first, before anything else. The daemon replies `welcome`, or
`error` and closes.

```jsonc
// →
{"v": 1, "kind": "hello", "client": "tui", "client_version": "0.3.1"}

// ←
{"v": 1, "kind": "welcome", "daemon_version": "0.3.0", "session": "01JD2K…"}
```

- `v` is the **protocol** version, an integer, bumped only on breaking changes.
- Daemon rejects an unknown `v` with `unsupported_version` and closes.
- `daemon_version` and `client_version` are informational. Never branch on them.

---

## Message kinds

Client → daemon: `hello`, `request`, `subscribe`, `unsubscribe`.
Daemon → client: `welcome`, `response`, `event`, `error`.

### `error`

A failure with no request to answer: a rejected handshake, or a line that could not be
parsed. It has no `id`. See `docs/decisions/0002-error-frame-and-ephemeral-progress.md`.

```jsonc
{"v":1,"kind":"error","error":{"code":"unsupported_version","message":"…","detail":{"supported":[1]}}}
```

After a rejected handshake or an over-long line the daemon closes the connection. After an
unparseable line it keeps the connection open.

### `request` / `response`

`id` is an opaque client-generated string, unique within the connection. The daemon echoes
it. Requests may be pipelined; responses may arrive out of order, so match on `id`.

```jsonc
// →
{"v":1,"kind":"request","id":"r1","op":"records.list",
 "params":{"collection":"leetcode","filter":{"status":"todo"},"limit":50}}

// ← success
{"v":1,"kind":"response","id":"r1","ok":true,
 "data":{"items":[…],"total":137}}

// ← failure
{"v":1,"kind":"response","id":"r1","ok":false,
 "error":{"code":"not_found","message":"no collection 'leetcode'","detail":null}}
```

`data` and `params` are arbitrary JSON, defined per-op. `error.detail` is an optional
object for structured context; clients must tolerate it being absent or unrecognised.

**All or nothing.** An inline request that returns `ok: false` has changed nothing — no
file, no event, no index row. A client may retry it without first checking whether it
partly applied. For queued ops the guarantee is per store transaction, not per task: a
failed task may have committed earlier transactions, and each one it did commit emitted
its own event. Clients learn exactly what happened from the event stream, never by
inference. See `CLAUDE.md` §7.1.

### Inline vs queued ops

Every op declares an `execution` in `core.manifest`:

- **`inline`** — answered synchronously. `data` is the result.
- **`queued`** — the daemon enqueues the work and responds immediately with a task
  handle. The result arrives later as events, not as this response.

```jsonc
// → a queued op
{"v":1,"kind":"request","id":"r2","op":"fetchers.fetch",
 "params":{"source":"hn-whoishiring"}}

// ← accepted, not completed
{"v":1,"kind":"response","id":"r2","ok":true,
 "data":{"queued":true,"task_id":"01JD2N…","lane":"fetchers","position":3}}
```

Clients must read `execution` from the manifest rather than assuming. An op that is inline
today may become queued later without a protocol version bump.

### Queue control on the request frame

Queue behaviour is orthogonal to the op, so it lives on the **frame**, not in `params`.
Ops with `execution: inline` ignore it.

```jsonc
{"v":1,"kind":"request","id":"r3","op":"fetchers.fetch",
 "params":{"source":"linkedin"},
 "queue":{"priority":"override","confirm":false}}
```

| Field | Values | Default |
|---|---|---|
| `priority` | `normal` \| `override` | `normal` |
| `confirm` | `true` \| `false` | `false` |
| `queue_version` | integer, echoed from a prior `confirmation_required` | absent |

`priority: "scheduled"` is not settable by clients. Only the scheduler produces it.

### `subscribe` / `event`

```jsonc
// →
{"v":1,"kind":"subscribe","id":"s1","topics":["records.*","queue.task.failed"]}
// ← acknowledgement
{"v":1,"kind":"response","id":"s1","ok":true,"data":{"subscribed":2}}

// ← pushed at any time thereafter, no id
{"v":1,"kind":"event","event":{
  "id":"01JD2K…","at":"2026-09-16T14:03:11Z","source":"fetchers",
  "topic":"fetchers.fetch.finished",
  "payload":{"source":"hn-whoishiring","new_items":12}}}
```

Topic patterns support a trailing `*` on a segment boundary: `records.*`,
`records.item.*`, or `*` for everything. No other globbing. `unsubscribe` takes the same
`topics` shape.

The event object is byte-identical to the one appended to
`$SHIMMER_HOME/events/YYYY-MM-DD.jsonl`. One definition in `shimmer-core`. The one exception to
"every streamed event is logged" is `queue.task.progress`, which is streamed only.

**Backpressure:** each connection has a bounded event queue (1024). If a client does not
drain it, the daemon drops the oldest and sends
`{"kind":"event","event":{"topic":"core.stream.lagged","payload":{"dropped":N}}}`. A client
receiving this should refetch rather than assume its state is current.

---

## Priority promotion: the confirmation round-trip

Promoting a task ahead of scheduled work is never silent. It is a two-step exchange, with
no server-side pending state — the daemon holds nothing between the two requests.

```jsonc
// → 1. ask to jump the lane
{"v":1,"kind":"request","id":"p1","op":"fetchers.fetch",
 "params":{"source":"linkedin"},
 "queue":{"priority":"override"}}

// ← 2. refused pending confirmation, with what it would cost
{"v":1,"kind":"response","id":"p1","ok":false,
 "error":{"code":"confirmation_required",
  "message":"would displace 3 scheduled tasks in lane 'fetchers'",
  "detail":{"lane":"fetchers","queue_version":41,
    "would_displace":[
      {"task_id":"01JD2P…","op":"fetchers.fetch","priority":"scheduled"},
      {"task_id":"01JD2Q…","op":"fetchers.fetch","priority":"scheduled"},
      {"task_id":"01JD2R…","op":"fetchers.fetch","priority":"scheduled"}]}}}

// → 3. user agreed; echo the queue_version you showed them
{"v":1,"kind":"request","id":"p2","op":"fetchers.fetch",
 "params":{"source":"linkedin"},
 "queue":{"priority":"override","confirm":true,"queue_version":41}}

// ← 4. accepted
{"v":1,"kind":"response","id":"p2","ok":true,
 "data":{"queued":true,"task_id":"01JD2S…","lane":"fetchers","position":0}}
```

`queue_version` increments on every mutation of that lane. If it changed between steps 2
and 3, the daemon returns `confirmation_required` again with the fresh list rather than
acting on a stale picture the user never saw. Optimistic concurrency, no locks.

Promotion emits `queue.task.promoted` carrying the displaced task ids — the audit trail
for "why did my scheduled fetch run late".

---

## Error codes

Stable strings. Clients may branch on `code`, never on `message`.

| Code | Meaning |
|---|---|
| `unsupported_version` | Protocol `v` not supported. Connection closes. |
| `bad_request` | Malformed frame, missing field, oversized line. |
| `unknown_op` | No module registered that op. |
| `invalid_params` | Op exists, params failed validation. |
| `not_found` | Referenced collection, record, workspace, task, trigger or source missing. |
| `conflict` | Concurrent modification, duplicate id, or stale `queue_version`. |
| `confirmation_required` | Action needs explicit user confirmation. See `detail`. |
| `workspace_dirty` | Workspace is in a half-configured state and cannot be activated. |
| `lane_unknown` | Requested lane is not declared by any module. |
| `not_cancellable` | Task already finished, or is running and does not honour cancel. |
| `module_error` | Module-specific failure. See `detail`. |
| `unavailable` | Module not initialised, or a dependency (network, script) failed. |
| `internal` | Bug. Always logged daemon-side with a backtrace. |

### `workspace_dirty` detail

```jsonc
{"code":"workspace_dirty",
 "message":"workspace 'deep-work' failed setup and was not cleaned up",
 "detail":{"workspace":"deep-work","failed_step":"3/7 tmux-session",
   "failed_at":"2026-09-16T09:12:44Z","log":"logs/deep-work-01JD2T.log",
   "has_cleanup_script":true}}
```

Clients should offer: run cleanup, force relaunch, or open the log. Never auto-force.

---

## Discovery

**Clients must not hardcode module knowledge.** Ask the daemon what exists.

```jsonc
// →
{"v":1,"kind":"request","id":"m1","op":"core.manifest","params":{}}

// ←
{"v":1,"kind":"response","id":"m1","ok":true,"data":{
  "protocol":1,
  "lanes":[
    {"id":"workspaces","max_concurrent":1},
    {"id":"fetchers","max_concurrent":8},
    {"id":"notify","max_concurrent":2},
    {"id":"index","max_concurrent":1},
    {"id":"default","max_concurrent":4}],
  "modules":[
    {"id":"records","version":"0.1.0","namespace":"records",
     "commands":[
       {"op":"records.list","summary":"List records in a collection",
        "execution":"inline","params_schema":{…}},
       {"op":"records.complete","summary":"Mark a record done and stamp its completion date",
        "execution":"inline","params_schema":{…}}],
     "topics":["records.item.created","records.item.updated",
               "records.item.completed"]}
  ]}}
```

Illustrative excerpt — `records` actually registers twenty ops (`records.collections`,
`.templates`, `.create_collection`,
`.add`, `.get`, `.list`, `.update`, `.complete`, `.reopen`, `.rename`, `.remove`, `.restore`,
`.trash`, `.purge`, `.check`, `.rename_field`, `.rename_collection`, `.remove_collection`,
`.restore_collection`, `.import`) and thirteen topics; see "Records
ops" below and `docs/decisions/0008-records-collections-and-storage.md` for the full list.
This example only shows two commands to keep the shape readable.

**The whole example is illustrative of a fully-loaded daemon — every module in CLAUDE.md's
architecture registered at once — not any one build.** `lanes` is not a fixed list: `default`
(`max_concurrent: 4`) is the only lane always present; every other lane (`workspaces`,
`fetchers`, `notify`, `index`, or one a plugin declares) appears **only when some registered
module's `Module::lanes()` declares it** (`crates/daemon/src/registry.rs`). A daemon that has
only registered `records` — true of `crates/app` as of M1 — returns `lanes: [{"id":"default",
"max_concurrent":4}]` and one module in `modules`, not the five-lane, three-module picture
above. Clients must read `core.manifest` at runtime rather than assume this example's shape,
same as the "Discovery" heading already says for module knowledge generally.

`params_schema` is JSON Schema. Advisory before M3 — the TUI may render forms from it
later but must not depend on it being complete yet.

This op is what makes the platform extensible: a module written after the TUI shipped still
appears in it, in the right lane, with the right execution semantics.

---

## Core ops

Always present, independent of which modules are registered.

| Op | Execution | Params | Returns |
|---|---|---|---|
| `core.ping` | inline | `{}` | `{"pong":true,"uptime_s":N}` |
| `core.manifest` | inline | `{}` | See above. |
| `core.shutdown` | inline | `{}` | `{"ok":true}`, then a clean exit. |

## Queue ops

| Op | Execution | Params | Returns |
|---|---|---|---|
| `queue.list` | inline | `{"lane"?}` | Lanes, each with `running` and `queued` task arrays and `queue_version`. |
| `queue.task` | inline | `{"task_id"}` | One task with status, progress, attempts, origin. |
| `queue.reorder` | inline | `{"lane","task_id","before":TaskId\|null,"queue_version"}` | New `queue_version`. |
| `queue.cancel` | inline | `{"task_id"}` | `{"cancelled":true}` or `not_cancellable`. |

Reordering is within a lane only. The running task cannot be reordered or displaced; it can
be cancelled. A stale `queue_version` returns `conflict`.

## Scheduler ops

| Op | Execution | Params | Returns |
|---|---|---|---|
| `scheduler.list` | inline | `{}` | Triggers with `next_due`, `last_run`, `catch_up`, `paused`. |
| `scheduler.get` | inline | `{"trigger_id"}` | One trigger, same shape as a `scheduler.list` entry, or `not_found`. |
| `scheduler.add` | inline | `{"schedule","op","params","catch_up","lane"?,"fallback"?}` | `{"trigger_id"}` |
| `scheduler.remove` | inline | `{"trigger_id"}` | `{"removed":true}` |
| `scheduler.pause` | inline | `{"trigger_id"}` | `{"paused":true}` |
| `scheduler.resume` | inline | `{"trigger_id"}` | `{"paused":false}` |

Schedules are `{"cron": "0 9 * * 1"}` (5 fields, Sunday = 0 or 7, Monday = 1, UTC),
`{"every": 172800}` (seconds) or `{"once": "2026-10-04T09:00:00Z"}`. A fired task's params
gain a `scheduled_for` timestamp when they are an object. See
`docs/decisions/0004-scheduler-semantics.md`.

`catch_up` is **required** on `scheduler.add`. There is no default — `skip`, `run_once` and
`backfill` behave very differently after a laptop sleeps, and guessing sends duplicate
emails.

## Workspace ops

| Op | Execution | Params | Returns |
|---|---|---|---|
| `workspaces.list` | inline | `{}` | `{"workspaces":[{"id","label","state"}]}`, sorted by id. `dirty_reason` when dirty; `error` when invalid. |
| `workspaces.status` | inline | `{"id"}` | `{"id","label","description"?,"state","steps":[{"index","name","mode","timeout_s"?,"description"?}],"cleanup_timeout_s"?,"has_cleanup_script","last_session"}`. When dirty also `dirty_reason`, `log` and `dirty:{reason,failed_step,failed_at,log}`; when launching, `running_step`; when invalid, `error`. |
| `workspaces.activate` | queued (`workspaces`) | `{"id"}` | Task handle. The task ends `{"id","state":"active","session_id","log"}`, or fails (see below). `log` is the last step's log (relative to `$SHIMMER_HOME`) when that step is supervised, else `""`: a template that ends with a summary step shows it this way (ADR 0025). |
| `workspaces.cleanup` | queued (`workspaces`) | `{"id"}` | Task handle. Runs `cleanup.{sh,ps1}` supervised; on success `dirty → ready`. The task ends `{"id","state":"ready","log"}`: the script's log, for a client to show what it did. |
| `workspaces.force_relaunch` | queued (`workspaces`) | `{"id"}` | Task handle. Launches a `dirty` workspace anyway; emits `workspaces.session.forced` before any step runs. |
| `workspaces.reset` | inline | `{"id"}` | `{"id","state":"ready"}`. Clears `dirty` without running cleanup. Last resort; also clears an unreadable `state.toml`. |
| `workspaces.stop` | queued (`workspaces`) | `{"id"}` | Task handle. Runs `cleanup.{sh,ps1}` supervised on an `active` workspace; success → `ready` and `workspaces.session.stopped`; the task ends `{"id","state":"ready","log"}` (`log` empty with no script). No cleanup script: `ready` at once, nothing run. Failure leaves it `active`; the task fails with the log in `detail`. `invalid_params` when ready or dirty, `conflict` while launching. Windows are never closed: the script stops processes. ADR 0026. |
| `workspaces.remove` | inline | `{"id"}` | `{"removed":true}`. Moves the folder to `data/workspaces/.removed/<id>/` (the latest removal of an id replaces an older one); emits `workspaces.workspace.removed`. `conflict` when active, launching or dirty ("stop it first" / "clean it up or reset it first"). Folders starting with `.` are never listed as workspaces. ADR 0026. |
| `workspaces.restore` | inline | `{"id"}` | The workspace, as `workspaces.status` shows it, back exactly as removed; emits `workspaces.workspace.restored`. `not_found` with nothing to restore; `conflict` if that id exists again. ADR 0026. |
| `workspaces.copy` | inline | `{"id","to","values"?,"label"?}` | `{"from","to","changed":[{"name","from","to"}]}`: a new workspace at `<to>` with every file of `<id>` but its state (it starts `ready`, no history), `values` as new answers (checked as `workspaces.reconfigure` checks them; `invalid_params` for a workspace made by hand) and `label` as its label. The original may be in any state: it's only read. Emits `workspaces.workspace.copied`. A copy that wouldn't be a valid workspace isn't made. ADR 0026 §2b. |
| `workspaces.rename` | inline | `{"id","to"}` | `{"from","to","notes":[…]}`: every file of the workspace moved to `data/workspaces/<to>/` in one transaction; emits `workspaces.workspace.renamed`. Only while `ready` (`conflict` when active, launching or dirty: stop and cleanup find what a launch started by its id). `to` must be a valid id (`invalid_params`), not the same, and free (`conflict`). `notes` names what stays under the old id outside the Shimmer folder (a separate browser window's profile) and how to move it. Schedules aren't changed: the CLI lists the ones naming the old id and moves them (`scheduler.add` again, then `scheduler.remove`) only when asked. ADR 0026 §2a. |
| `workspaces.templates` | inline | `{"id"?, "files"?}` | `{"templates":[{"id","label","description","category","needs":[…],"good_to_know":[…],"on_stop","steps":[{"name","mode","timeout_s"?,"description"?}],"questions":[{"name","prompt","kind","required","default"?,"choices"?,"help"?}]}],"categories":[{"id","label"}]}`: templates by category (in `categories`' order), then id; `categories` gives each heading. With `id`, only that template (`not_found` naming the others); with `files: true`, each template also has `"files":[{"path","text"}]`: every file a workspace made from it gets (not `template.toml`), for `peek --scripts`. `kind` is `text`, `command`, `folder`, `file`, `url`, `urls` or `choice`. ADR 0025. |
| `workspaces.answers` | inline | `{"id"}` | `{"id","template","questions":[…as workspaces.templates],"answers":{NAME: value or null}}`: the template the workspace was made from (its `.template` file, or the "Created from Shimmer's X template" line in `workspace.toml`) and its current answers. `invalid_params` for a workspace made by hand. ADR 0025 §9. |
| `workspaces.reconfigure` | inline | `{"id","values":{NAME: answer}}` | `{"id","template","changed":[{"name","from","to"}]}`. Only while `ready` (`conflict` when active, launching or dirty: cleanup reads the answers). Only `[env]` in `workspace.toml` is rewritten; changed answers are checked as `create` checks them; an empty one clears an optional question. Nothing changed: nothing written. ADR 0025 §9. |
| `workspaces.create` | inline | `{"id","template","values"?,"label"?}` | The new workspace, as `workspaces.status` shows it (state `ready`). `values` maps question names to strings; left out means the default, or empty. Every answer is written to the workspace's `[env]`. A missing required answer, an unknown question, or an answer that doesn't fit its kind (`folder`/`file` must be absolute, `url`s must start with `http://` or `https://`) is `invalid_params` naming the question; an unknown template is `not_found` naming the ones there are; anything already at `data/workspaces/<id>/` is `conflict`, never overwritten. One transaction writes the folder and emits `workspaces.workspace.created`. ADR 0025. |

`state` is one of `ready`, `launching`, `active`, `dirty`, or `invalid` when the workspace's
`workspace.toml` or `state.toml` can't be read; `error` then says why, one problem per line
when both files are broken (ADR 0012 §6). An invalid workspace carries no `dirty_reason`,
`dirty`, `log` or `running_step`, so a stale reason from before it broke can't hide the real
error. One invalid workspace never hides the others in `list`. **`active` means the launch succeeded
and nothing more** — there is no liveness check, so a client must not present it as
"currently running". `last_session` is `null` until the first launch, then
`{"id","started_at","forced","outcome"}` with `outcome` one of `launched`, `dirty`, `cancelled`.

**When `workspace_dirty` arrives.** `activate` is queued, so the request itself always
returns a task handle; the refusal or failure comes as the task's failure
(`queue.task.failed` with `code` and `detail`), shaped as in "`workspace_dirty` detail" above.
That is the same error whether the workspace was already dirty or a step has just failed.
Every other refusal (`invalid_params` for a bad `workspace.toml` or a missing script,
`conflict` while already launching, `not_found`) also arrives on the task. Nothing has run
when the task fails that way, and the same holds for a launch cancelled before its first step
(`module_error`, "cancelled before any step ran").

**Steps run one at a time** and the first failure stops the launch (ADR 0012 §5): a
supervised step fails on a non-zero exit, a timeout or a signal; a detached step succeeds
once started. The workspace is then `dirty`. If the very first step can't be started at all
(e.g. no launch backend), or the launch is cancelled before its first step, nothing ran, so
the workspace goes back to the state it had and `workspaces.session.abandoned` records why; the
task fails with that error (`unavailable`, or `module_error` when cancelled). Cancelling a launch
after a step has run leaves it `dirty`.

## Records ops

| Op | Execution | Params | Returns |
|---|---|---|---|
| `records.collections` | inline | `{}` | `{"collections":[...]}`, each collection definition as JSON (+ `"skipped"` naming each malformed collection file). |
| `records.add` | inline | `{"collection","id"?,"fields"?}` | The new item, which carries its `id`. Without an `id`, one is made from the collection's `title` (`"Two Sum"` → `two-sum`, then `two-sum-2`, …), or from today's date (`2026-10-05-1`) when there's no title to use. An explicit `id` that exists is `conflict`. ADR 0018. |
| `records.get` | inline | `{"collection","id"}` | The item. |
| `records.list` | inline | `{"collection","filter"?,"search"?,"sort"?,"limit"?,"offset"?}` | `{"items","total","titles"}` (+ `"skipped"` if a hand-edited file was unreadable). `titles` is `{id: title}` for the listed items (ADR 0024 §5). Ordered by `sort`, then by id. See below. |
| `records.update` | inline | `{"collection","id","fields"}` | The item. A `null` field value unsets it. A list field takes a whole new list or `{"add":[…],"remove":[…]}` (ADR 0024 §2). Setting a `unique` field to a value another record has is `conflict`, naming that record. |
| `records.complete` | inline | `{"collection","id","fields"?}` | The item, now `done`, with its `stamp_on_complete` field set. `fields` are applied first, with `records.update`'s rules; an explicit value for the stamp field wins over today (back-dating). One `records.item.completed`. When the record is already `done`: re-stamped again, unless the collection says `repeat_complete = "refuse"`, then `conflict`. |
| `records.rename` | inline | `{"collection","id","new_id"}` | The item under its new id. One transaction moves the file, rewrites every `ref` to it, and emits `records.item.renamed`. `conflict` if `new_id` exists; `not_found` if `id` doesn't. ADR 0018. |
| `records.reopen` | inline | `{"collection","id","clear_stamp"?}` | The item, now `todo`. The stamp is kept unless `clear_stamp`. `conflict` if it isn't `done`. ADR 0016. |
| `records.remove` | inline | `{"collection","id","force"?}` | `{"removed":true}`. The record moves to `data/records/trash/<collection>/<id>.toml` (the latest removed version of each id), out of every other op. ADR 0020. If other records refer to it through a `ref` field, `conflict` with `detail: {"referred_by":["collection/id",…]}` unless `force` (ADR 0024 §3). |
| `records.restore` | inline | `{"collection","id"}` | The record, back exactly as removed; emits `records.item.restored`. `not_found` if it isn't in the trash; `conflict` if the id, or one of its `unique` values, is taken again. |
| `records.trash` | inline | `{"collection"}` | `{"items":[…]}`: the collection's removed records, by id. |
| `records.purge` | inline | `{"collection","id"?,"confirm"?}` | `{"purged":N}`. Permanently deletes one trashed record, or the whole trash. Without a matching `confirm`, `confirmation_required` with `detail: {"records":N}`; re-send with `"confirm": {"records":N}`. `{"purged":0}` without asking when the trash is empty; `not_found` for an `id` not in it. Emits `records.trash.purged`. ADR 0024 §5. |
| `records.import` | inline | `{"collection","rows":[{…}],"dry_run"?,"skip_invalid"?}` | `{"added":[ids],"skipped":[{"row","reason"}],"invalid":[{"row","error"}],"written":bool}`. Each row (an object of field values, plus optional `id` and `status`) is checked like `records.add` and against the other rows; rows numbered from 1. A row whose id or `unique` value exists is skipped. Everything added is written in one transaction, one `records.item.created` each; nothing is written on a `dry_run`, or while any row is invalid unless `skip_invalid`. At most 5000 rows. ADR 0024 §4. |
| `records.templates` | inline | `{"id"?, "file"?}` | `{"templates":[…],"categories":[{"id","label"}]}`: each built-in template as `records.collections` shows a collection, plus `"category"`, `"notes":["…"]` (the header comment's paragraphs, each as one line) and `"examples":["…"]` (its "How it works" lines as written); by category (in `categories`' order), then id. With `id`, only that template (`not_found` naming the others); with `file: true`, each also has `"file"`: its text exactly as `records.create_collection` copies it. ADRs 0022, 0030 §1. |
| `records.create_collection` | inline | `{"id","template","label"?}` | The new collection. Copies the template's file (comments kept) with only `id` and `label` changed, checks it, and writes it with `records.collection.created` in one transaction. `conflict` if the id exists; `not_found` naming the templates for an unknown one. |
| `records.check` | inline | `{"collection"}` | `{"checked":N,"problems":[{"id","problems":["…"]}]}`: records that don't fit the collection (missing required, wrong type, unknown keys, unreadable, `ref`s to records that don't exist). Writes nothing. ADR 0021. |
| `records.rename_field` | inline | `{"collection","from","to"}` | `{"collection":{…},"updated":N}`. One transaction renames the field in the collection file (comments kept) and in every live and trashed record; emits `records.field.renamed`. |
| `records.rename_collection` | inline | `{"id","new_id"}` | The collection under its new id; moves its file, records and trash, and rewrites `collection = "…"` in every ref field pointing into it, in one transaction; emits `records.collection.renamed`. `conflict` if `new_id` exists. |
| `records.remove_collection` | inline | `{"id","confirm"?}` | `{"removed":true,"records":N}`. Without a matching `confirm`, `confirmation_required` with `detail: {"records":N}`; re-send with `"confirm": {"records":N}`. Moves everything to `data/records/removed-collections/<id>/`; emits `records.collection.removed`. |
| `records.restore_collection` | inline | `{"id"}` | The collection, back with its records and trash; emits `records.collection.restored`. |

All inline — nothing here is slow enough to queue. Items on the wire are flat:
`{"id","status",<every schema field>}`, with unset fields as `null`. `filter` on
`records.list` maps each key (a field, `id` or `status`) to either a plain value, exact equality
(`null` matches unset), or an object of operators that must all hold: `eq`, `ne` (unset
matches), `lt`/`lte`/`gt`/`gte` (int, date, and enum by listed order), `in` (a non-empty list),
`set` (`true`/`false`; `""` and `[]` count as unset), `contains` (text, ignoring case) and `has`
(list fields only: the list holds this item, strings ignoring case; a plain value on a list
means `has`). `"or": [{…},{…}]` holds alternative filters: a record matches if it matches any one,
and every other key still applies; `or` doesn't nest. On `date` and `datetime` fields a value may
be `"today"`, `"today+N"` or `"today-N"` (days), resolved by the daemon in the configured
timezone; a plain date compared with a `datetime` compares days. ADR 0024 §5. An unknown key
or operator, an operator the field's type doesn't take, or a value that doesn't fit is
`invalid_params`, so a typo never silently matches everything or nothing. `search` matches text
in the id or any string, enum or ref field (and their list items), ignoring case. `sort` is a
list of up to 5 keys, `-` for descending (lists can't be sorted; `datetime`s sort as instants, a
plain date as the start of its day); unset values come last in either direction, and ties fall
back to the id. ADR 0019. Each collection in
`records.collections` carries `repeat_complete` (`"restamp"`, the default, or `"refuse"`) and
`completable` (`false` for reference lists: their items have no `status`, and `complete`,
`reopen`, and filtering or sorting on `status` are `invalid_params`), and,
when set, `description`, `title` (e.g. `"{company}: {position}"`), `related` (collection ids),
`extra` (`[extra.<name>]` tables, passed through untouched) and per field `role` (`"deadline"`
on a date or datetime, `"url"` on a string), `unique`, `of` (what a `list` holds: `string`,
`enum` or `ref`) and `collection` (where a `ref` points). Field types (ADR 0024): `string`,
`int`, `bool`, `date` (`"2026-10-20"`), `enum`, `datetime` (UTC RFC 3339 like
`"2026-10-21T06:59:00Z"`, or a plain date; input without an offset is read in the configured
timezone), `list` (a JSON array; empty means unset) and `ref` (the target record's id, checked
on write; a title renders a list as its items joined with `, `). A `unique` field's value can be taken by one
record only (`conflict` on `records.add`, `records.update`, or `records.complete` with
`fields`); unset and `""` don't count. See
`docs/decisions/0008-records-collections-and-storage.md`,
`docs/decisions/0016-records-completion-lifecycle.md` and
`docs/decisions/0017-records-integration-metadata.md`.

Module ops are namespaced `<module>.<verb>`. The daemon routes on the prefix; a collision
between two modules is a startup failure, not a runtime surprise.

**Aliases never appear on the wire.** `bankai`, `arise` and every other command-pack name
are resolved to canonical ops by the client before a frame is sent. The daemon rejects an
alias as `unknown_op`. Swapping or installing a pack therefore never requires a protocol
change or a daemon restart. See `CLAUDE.md` §15.1.

---

## Topic catalogue

Non-exhaustive — modules may add their own. Clients subscribe to patterns and must
tolerate unknown topics.

| Topic | Emitted when |
|---|---|
| `core.stream.lagged` | This connection dropped events (see Backpressure). |
| `queue.task.enqueued` | Task accepted into a lane. |
| `queue.task.started` | Task began executing. |
| `queue.task.progress` | Module called `ctx.progress`. Payload `{fraction, note}`. |
| `queue.task.finished` | Task succeeded. |
| `queue.task.failed` | Task failed. Payload includes `error`, `attempts`, `lane`, `origin`. |
| `queue.task.cancelled` | Task cancelled before or during execution. |
| `queue.task.promoted` | A task jumped the lane. Payload lists displaced task ids. |
| `queue.task.reordered` | A waiting task moved within its lane. Payload `{task_id, lane, before, queue_version}`. |
| `scheduler.trigger.fired` | Trigger fired and enqueued a task. |
| `scheduler.trigger.skipped` | Trigger fired while its previous task was still pending (`reason: "overlap"`), or its task could not be enqueued (`reason: "enqueue_failed"`). |
| `scheduler.trigger.missed` | A `catch_up` policy dropped due occurrences. Payload `{trigger_id, count, catch_up}`. |
| `scheduler.trigger.added` / `.removed` / `.paused` / `.resumed` | Trigger lifecycle. |
| `workspaces.session.launched` | All launch steps succeeded. Payload `{workspace, session_id, forced, steps}`. |
| `workspaces.session.dirty` | A launch step failed, a launch was cancelled part-way, or the daemon restarted mid-launch. Payload `{workspace, reason, failed_step, failed_at, log}` (+ `session_id`, `forced`, `has_cleanup_script` when a launch was running). |
| `workspaces.session.forced` | Force relaunch of a dirty workspace, before any step runs. Payload `{workspace, prior}`, `prior` being the dirty state it overrides. |
| `workspaces.session.cleaned` | Cleanup script succeeded; state back to `ready`. Payload `{workspace, log}`. |
| `workspaces.session.abandoned` | A launch (forced or not) was claimed but no step ran: cancelled first, or step 1 couldn't start. The workspace is back to the state it had. Payload `{workspace, reason, forced, back_to}`. |
| `workspaces.workspace.reset` | `workspaces.reset` cleared `dirty` without cleanup. Payload `{workspace, prior}`. |
| `workspaces.session.stopped` | `workspaces.stop` finished. Payload `{workspace, ran_cleanup, log}` (`log` empty when there was no cleanup script). ADR 0026. |
| `workspaces.workspace.removed` / `.restored` | `{workspace}`. ADR 0026. |
| `workspaces.workspace.created` | `workspaces.create` made a workspace from a template. Payload `{workspace, template}`. ADR 0025. |
| `workspaces.workspace.copied` | `workspaces.copy` made a workspace from another. Payload `{from, to, changed: [name…]}` (names, never values). ADR 0026 §2b. |
| `workspaces.workspace.renamed` | `workspaces.rename` gave a workspace a new id. Payload `{from, to}`. ADR 0026 §2a. |
| `workspaces.workspace.reconfigured` | `workspaces.reconfigure` changed a workspace's answers. Payload `{workspace, changed: [name…]}`: the names, never the values. ADR 0025 §9. |
| `records.item.created` / `.updated` / `.completed` / `.reopened` / `.renamed` / `.removed` / `.restored` | Record mutations. All carry `{collection, id, title, deadlines, item}` (for `.removed`, `item` is the record as it was): `item` is the full wire item, `title` the record's readable name (the collection's `title` template, or the id), `deadlines` `{field: date}` for every set field with `role = "deadline"` (ADR 0017). `.updated` also carries `changed`: the fields whose value actually changed, sorted (`[]` when nothing did). `.renamed` also carries `new_id` (`id` is the old one; `item` is under the new one) and `references` (how many `ref` values were rewritten to point at the new id). |
| `records.collection.created` | `records.create_collection` made a collection from a template. Payload `{collection, template}`. Nothing is created on first start (ADR 0023). |
| `records.field.renamed` | `{collection, from, to}`. |
| `records.collection.renamed` / `.removed` / `.restored` | `{collection, new_id}` / `{collection, records}` / `{collection}`. |
| `records.trash.purged` | `records.purge` deleted trashed records for good. `{collection, records}`, plus `id` when one record was purged. ADR 0024 §5. |
| `fetchers.item.found` | A source returned a new, deduplicated item. |
| `fetchers.fetch.finished` / `.failed` | A fetch run ended. |
| `calendar.date.registered` | A dated entry was stored. |
| `notify.sent` | Delivery succeeded, naming the sink used. |
| `notify.fallback_used` | Primary sink failed; the fallback delivered. |
| `notify.exhausted` | Every sink failed. Written to `notifications/failed.jsonl`. |

`queue.task.failed` is the topic a dashboard most needs: it is how failures become visible
without polling, and it is how the elevated-priority dirty-workspace warning reaches the
user.

---

## Mock daemon

`crates/tui` develops against the mock daemon, `shimmer mockd`: a subcommand of the `shimmer` binary
(`app` is the only binary, CLAUDE.md §3), implemented in `crates/mockd`, which depends on
`core` and `proto` only. It speaks this protocol, serving canned responses from
`crates/mockd/fixtures/*.json` and replaying a scripted event timeline. Built at M1. The
fixture format is documented in `crates/mockd/fixtures/README.md`.

It must be able to simulate, because these are the paths hardest to reach against a real
daemon: a `confirmation_required` round-trip, a `workspace_dirty` failure, a
`core.stream.lagged` drop, and a long queued task emitting progress.

```
$ shimmer mockd --fixtures crates/mockd/fixtures --socket /tmp/shimmer-mock.sock
$ shimmer tui --socket /tmp/shimmer-mock.sock
```

Both flags are required, so the mock can never silently take over the real daemon's socket.

This is what decouples the TUI work. Dev C should never be blocked waiting for a
daemon-side op to land — write the fixture (a normal PR to `crates/mockd`), build against it,
swap to the real daemon at M5.

---

## Worked example

Connect, discover, subscribe, enqueue, watch it run.

```jsonc
→ {"v":1,"kind":"hello","client":"tui","client_version":"0.1.0"}
← {"v":1,"kind":"welcome","daemon_version":"0.1.0","session":"01JD2KQ…"}

→ {"v":1,"kind":"request","id":"1","op":"core.manifest","params":{}}
← {"v":1,"kind":"response","id":"1","ok":true,"data":{"protocol":1,"lanes":[…],"modules":[…]}}

→ {"v":1,"kind":"subscribe","id":"2","topics":["queue.*","fetchers.*"]}
← {"v":1,"kind":"response","id":"2","ok":true,"data":{"subscribed":2}}

→ {"v":1,"kind":"request","id":"3","op":"fetchers.fetch","params":{"source":"hn-whoishiring"}}
← {"v":1,"kind":"response","id":"3","ok":true,
   "data":{"queued":true,"task_id":"01JD2N…","lane":"fetchers","position":1}}

← {"v":1,"kind":"event","event":{"topic":"queue.task.started",
   "payload":{"task_id":"01JD2N…","lane":"fetchers"}, …}}
← {"v":1,"kind":"event","event":{"topic":"queue.task.progress",
   "payload":{"task_id":"01JD2N…","fraction":0.4,"note":"page 2 of 5"}, …}}
← {"v":1,"kind":"event","event":{"topic":"fetchers.item.found",
   "payload":{"source":"hn-whoishiring","id":"c-41883"}, …}}
← {"v":1,"kind":"event","event":{"topic":"queue.task.finished",
   "payload":{"task_id":"01JD2N…","new_items":12}, …}}

// meanwhile, a scheduled workspace launch fails in another lane
← {"v":1,"kind":"event","event":{"topic":"queue.task.failed",
   "payload":{"task_id":"01JD2M…","lane":"workspaces","origin":"scheduler",
     "error":"step 3/7 exited 1","workspace_dirty":true}, …}}
← {"v":1,"kind":"event","event":{"topic":"notify.sent",
   "payload":{"sink":"desktop","reason":"fallback","priority":"high"}, …}}
```

Two things to read out of the last four frames. The failing workspace task did not stall
the `fetchers` lane — different lanes, independent. And the scheduled trigger still reached
the user, as a notification instead of a launch, because its `fallback` fired. That is the
whole design working: decoupled, observable, and degrading rather than dropping.
