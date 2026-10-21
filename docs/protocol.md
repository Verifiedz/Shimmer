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

The path is also exported to workspace scripts as `SHIMMER_SOCKET`. A client reads that
variable too: when `SHIMMER_SOCKET` is set and non-empty it wins over the table above
(`crates/proto/src/paths.rs`), so a script calling `shimmer` reaches the daemon that launched
it. The CLI's `--socket PATH` overrides both, and a command given `--socket` never auto-starts a
daemon.

If the socket file exists but connecting returns `ECONNREFUSED`, the daemon died without
cleaning up. Start a daemon: it clears the stale file when it binds, so a client need not
(ADR 0007).

## Framing

Newline-delimited JSON. One complete JSON object per line, UTF-8, terminated by `\n`.
Objects must not contain literal newlines — JSON string escaping handles this.

Chosen for debuggability: a whole session can be driven from a shell.

```
$ socat - UNIX-CONNECT:$SOCK
{"v":2,"kind":"hello","client":"manual","client_version":"0.0.0"}
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
{"v": 2, "kind": "hello", "client": "tui", "client_version": "0.3.1"}

// ←
{"v": 2, "kind": "welcome", "daemon_version": "0.3.0", "session": "01JD2K…"}
```

- `v` is the **protocol** version, an integer, bumped only on breaking changes.
  - **v2** (ADR 0038 §6): every op is named by its CLI word. `records.get`, `workspaces.status`,
    `scheduler.get`, `notify.get` and `queue.task` became `.show`; `records.create_collection` and
    `workspaces.create` became `.new`; `queue.reorder` became `queue.move`; `fetchers.fetch` became `fetchers.run`; `scheduler.pause` /
    `.resume` became `scheduler.disable` / `.enable` (a trigger shows `enabled`, not `paused`);
    `records.trash` became `records.list {"removed": true}`. Topics: `records.trash.purged` became
    `records.item.purged`, `scheduler.trigger.paused` / `.resumed` became `.disabled` / `.enabled`.
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
{"v":2,"kind":"error","error":{"code":"unsupported_version","message":"…","detail":{"supported":[1]}}}
```

After a rejected handshake or an over-long line the daemon closes the connection. After an
unparseable line it keeps the connection open.

### `request` / `response`

`id` is an opaque client-generated string, unique within the connection. The daemon echoes
it. Requests may be pipelined; responses may arrive out of order, so match on `id`.

```jsonc
// →
{"v":2,"kind":"request","id":"r1","op":"records.list",
 "params":{"collection":"leetcode","filter":{"status":"todo"},"limit":50}}

// ← success
{"v":2,"kind":"response","id":"r1","ok":true,
 "data":{"items":[…],"total":137}}

// ← failure
{"v":2,"kind":"response","id":"r1","ok":false,
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
{"v":2,"kind":"request","id":"r2","op":"fetchers.run_task",
 "params":{"id":"hn-whoishiring"}}

// ← accepted, not completed
{"v":2,"kind":"response","id":"r2","ok":true,
 "data":{"queued":true,"task_id":"01JD2N…","lane":"fetchers","position":3}}
```

Clients must read `execution` from the manifest rather than assuming. An op that is inline
today may become queued later without a protocol version bump.

### Queue control on the request frame

Queue behaviour is orthogonal to the op, so it lives on the **frame**, not in `params`.
Ops with `execution: inline` ignore it.

```jsonc
{"v":2,"kind":"request","id":"r3","op":"fetchers.run_task",
 "params":{"id":"linkedin"},
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
{"v":2,"kind":"subscribe","id":"s1","topics":["records.*","queue.task.failed"]}
// ← acknowledgement
{"v":2,"kind":"response","id":"s1","ok":true,"data":{"subscribed":2}}

// ← pushed at any time thereafter, no id
{"v":2,"kind":"event","event":{
  "id":"01JD2K…","at":"2026-09-16T14:03:11Z","source":"fetchers",
  "topic":"fetchers.fetch.finished",
  "payload":{"source":"hn-whoishiring","new_items":12},
  "cause":"01JD2N…"}}
```

Topic patterns support a trailing `*` on a segment boundary: `records.*`,
`records.item.*`, or `*` for everything. No other globbing. `unsubscribe` takes the same
`topics` shape.

The event object is byte-identical to the one appended to
`$SHIMMER_HOME/events/YYYY-MM-DD.jsonl`. One definition in `shimmer-core`. The one exception to
"every streamed event is logged" is `queue.task.progress`, which is streamed only.

`cause` is the id of the queued task the event was emitted from, and is absent for a request
answered inline (ADR 0035 §5.1). The queue's own events about a task (`queue.task.*`, and
`queue.fallback.*` about the failed task) carry that task as their `cause`, so a rule set off by
its own action's `queue.task.finished` continues its chain (#251's review).
Automations uses it to follow a chain of rules; a client may ignore it. Older log lines have no
`cause` and read unchanged.

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
{"v":2,"kind":"request","id":"p1","op":"fetchers.run_task",
 "params":{"id":"linkedin"},
 "queue":{"priority":"override"}}

// ← 2. refused pending confirmation, with what it would cost
{"v":2,"kind":"response","id":"p1","ok":false,
 "error":{"code":"confirmation_required",
  "message":"would displace 3 scheduled tasks in lane 'fetchers'",
  "detail":{"lane":"fetchers","queue_version":41,
    "would_displace":[
      {"task_id":"01JD2P…","op":"fetchers.run_task","priority":"scheduled"},
      {"task_id":"01JD2Q…","op":"fetchers.run_task","priority":"scheduled"},
      {"task_id":"01JD2R…","op":"fetchers.run_task","priority":"scheduled"}]}}}

// → 3. user agreed; echo the queue_version you showed them
{"v":2,"kind":"request","id":"p2","op":"fetchers.run_task",
 "params":{"id":"linkedin"},
 "queue":{"priority":"override","confirm":true,"queue_version":41}}

// ← 4. accepted
{"v":2,"kind":"response","id":"p2","ok":true,
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
   "has_cleanup_script":true,"fix":"shimmer workspaces cleanup deep-work"}}
```

Clients should offer: run cleanup, force relaunch, or open the log. Never auto-force. `fix` is the
command that clears it: `cleanup` when there's a cleanup script, `reset` when there isn't (ADR 0035
§10).

---

## Discovery

**Clients must not hardcode module knowledge.** Ask the daemon what exists.

```jsonc
// →
{"v":2,"kind":"request","id":"m1","op":"core.manifest","params":{}}

// ←
{"v":2,"kind":"response","id":"m1","ok":true,"data":{
  "protocol":2,
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

Illustrative excerpt — `records` actually registers twenty-three ops (`records.collections`,
`.templates`, `.create_collection`,
`.add`, `.get`, `.list`, `.update`, `.complete`, `.reopen`, `.rename`, `.remove`, `.restore`,
`.trash`, `.purge`, `.check`, `.rename_field`, `.rename_collection`, `.copy_collection`,
`.remove_collection`, `.restore_collection`, `.purge_collection`, `.import`, `.health`) and fifteen topics; see "Records
ops" below and `docs/decisions/0008-records-collections-and-storage.md` for the full list.
This example only shows two commands to keep the shape readable.

**The whole example is illustrative of a fully-loaded daemon — every module in CLAUDE.md's
architecture registered at once — not any one build.** `lanes` is not a fixed list: `default`
(`max_concurrent: 4`) is the only lane always present; every other lane (`workspaces`,
`fetchers`, `notify`, `index`, or one a plugin declares) appears **only when some registered
module's `Module::lanes()` declares it** (`crates/daemon/src/registry.rs`). Today `crates/app`
registers `records` and `workspaces`, so a real daemon returns two lanes, `default` (4) and
`workspaces` (1), and those two modules, not the five-lane picture above. Clients must read `core.manifest` at runtime rather than assume this example's shape,
same as the "Discovery" heading already says for module knowledge generally.

`params_schema` is JSON Schema. Advisory: the daemon doesn't check params against it (each op
checks its own), so the TUI may render forms from it but must not depend on it being complete.

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
| `core.events.list` | inline | `{"topic"?,"limit"?}` | `{"events":[Event…]}`, oldest-first, capped to the `limit` most recent matches (default 100, max 1000). `topic` is a subscription-style pattern (`a.b.c`, `a.*`, `*`; default `*`) — `invalid_params` if it doesn't parse as one. Backed by the derived index (`crates/store/src/index.rs`'s `events_matching_recent`), not a log scan, so it stays fast regardless of history size. Module-agnostic: any event any module ever emitted, not just one module's own. |

## Queue ops

| Op | Execution | Params | Returns |
|---|---|---|---|
| `queue.list` | inline | `{"lane"?}` | Lanes, each with `running` and `queued` task arrays and `queue_version`. |
| `queue.show` | inline | `{"task_id"}` | One task with status, progress, attempts, origin. |
| `queue.move` | inline | `{"lane","task_id","before":TaskId\|null,"queue_version"}` | `{"queue_version","task_id"}`: the new `queue_version`, and the moved task's full id. |
| `queue.cancel` | inline | `{"task_id"}` | `{"cancelled":true,"was":"queued"\|"running","task_id"}`, or `not_cancellable`. A queued task is gone; a running one has been asked to stop and ends at its next cancellation check. |

Reordering is within a lane only. The running task cannot be reordered or displaced; it can
be cancelled. A stale `queue_version` returns `conflict`.

**Short ids (ADR 0031 §4).** Every `task_id` the queue ops take (and `before`) may be the full
id or a unique ending of one, case insensitive: a ULID's start is its creation time, its end is
random. A whole id matches at any length; an ending needs at least 4 characters (shorter is
`invalid_params`), so a stray keystroke can't remove or cancel something. Several matches return
`conflict` naming them; none returns `not_found`. The same holds for every `trigger_id` the
scheduler ops take. Replies that act on one carry its full id (`task_id` / `trigger_id`), so a
client can say which one it hit.

## Scheduler ops

| Op | Execution | Params | Returns |
|---|---|---|---|
| `scheduler.list` | inline | `{"removed"?}` | `{"triggers":[…]}`, each `{"id","op","params","schedule","catch_up","lane","fallback","next_due","last_run","enabled","done","source"}`. `done` is a `once` trigger that has fired; `source` is `{"type":"user"}` or `{"type":"module","id"}`. With `"removed": true`, the removed triggers instead, same shape (ADR 0038). |
| `scheduler.show` | inline | `{"trigger_id"}` | One trigger, same shape as a `scheduler.list` entry, or `not_found`. |
| `scheduler.add` | inline | `{"schedule","op","params","catch_up","lane"?,"fallback"?}` | `{"trigger_id"}` |
| `scheduler.edit` | inline | `{"trigger_id","schedule","op","params","catch_up","lane"?,"fallback"?}` | The trigger, same shape as a `scheduler.list` entry: a new schedule for the same id, keeping whether it's enabled and its `last_run`, counting from now. Checked as `scheduler.add` is. A module's own trigger is `conflict`. Emits `scheduler.trigger.edited` (payload as `.added`). ADR 0038 §6. |
| `scheduler.remove` | inline | `{"trigger_id"}` | `{"removed":true,"trigger_id"}` (the full id). Moves the trigger to `data/scheduler/removed/<id>.json`, where it never fires (the latest removal of an id replaces an older one). A module's own trigger is `conflict`: disable it instead. ADR 0038. |
| `scheduler.restore` | inline | `{"trigger_id"}` | The trigger, same shape as a `scheduler.list` entry, back as removed (paused or not). Counts from now, like `resume`: a recurring trigger's `next_due` is its next time after now; a `once` keeps its time, and its `catch_up` decides if that has passed. `trigger_id` may be a unique ending of a removed one's. `not_found` with nothing to restore; `conflict` if that id exists again; the `scheduler.add` errors if its op or lane has gone. Emits `scheduler.trigger.restored`. ADR 0038. |
| `scheduler.purge` | inline | `{"trigger_id"?,"confirm"?}` | `{"purged":N}`. Permanently deletes one removed trigger, or every one. Without a matching `confirm`, `confirmation_required` with `detail: {"triggers":N}`; re-send with `"confirm": {"triggers":N}`. `{"purged":0}` without asking when nothing is removed; `not_found` for a `trigger_id` that isn't removed. Emits `scheduler.trigger.purged`. ADR 0038. |
| `scheduler.disable` | inline | `{"trigger_id"}` | `{"enabled":false,"trigger_id"}`: it won't fire until enabled. ADR 0038 §6. |
| `scheduler.enable` | inline | `{"trigger_id"}` | `{"enabled":true,"trigger_id"}`: counts from now; time spent off is never caught up. ADR 0038 §6. |

`trigger_id` may be a unique ending of the id (see "Short ids" above).

Schedules are `{"cron": "0 9 * * 1"}` (5 fields, Sunday = 0 or 7, Monday = 1, UTC),
`{"cron_local": "0 9 * * 1"}` (the same, read in config.toml's `[general] local_timezone`, so 9am
stays 9am across a clock change; ADR 0031 §2), `{"every": 172800}` (seconds) or
`{"once": "2026-10-04T09:00:00Z"}`. A client should show a schedule shape it doesn't know rather
than fail on it: more may be added without a version bump. A fired task's params
gain a `scheduled_for` timestamp when they are an object. See
`docs/decisions/0004-scheduler-semantics.md`.

`catch_up` is **required** on `scheduler.add`. There is no default — `skip`, `run_once` and
`backfill` behave very differently after a laptop sleeps, and guessing sends duplicate
emails.

## Workspace ops

| Op | Execution | Params | Returns |
|---|---|---|---|
| `workspaces.list` | inline | `{"removed"?}` | `{"workspaces":[{"id","label","state"}]}`, sorted by id. `dirty_reason` when dirty; `error` when invalid. With `"removed": true`, the removed workspaces instead, `[{"id","label"?}]`, to restore or purge (ADR 0038 §6). |
| `workspaces.show` | inline | `{"id"}` | `{"id","label","description"?,"state","steps":[{"index","name","mode","timeout_s"?,"description"?}],"cleanup_timeout_s"?,"has_cleanup_script","last_session"}`. When dirty also `dirty_reason`, `log` and `dirty:{reason,failed_step,failed_at,log}`; when launching, `running_step`; when invalid, `error`. |
| `workspaces.activate` | queued (`workspaces`) | `{"id"}` | Task handle. The task ends `{"id","state":"active","session_id","log"}`, or fails (see below). `log` is the last step's log (relative to `$SHIMMER_HOME`) when that step is supervised, else `""`: a template that ends with a summary step shows it this way (ADR 0025). |
| `workspaces.cleanup` | queued (`workspaces`) | `{"id"}` | Task handle. Runs `cleanup.{sh,ps1}` supervised; on success `dirty → ready`. The task ends `{"id","state":"ready","log"}`: the script's log, for a client to show what it did. |
| `workspaces.force_relaunch` | queued (`workspaces`) | `{"id"}` | Task handle. Launches a `dirty` workspace anyway; emits `workspaces.session.forced` before any step runs. |
| `workspaces.reset` | inline | `{"id"}` | `{"id","state":"ready"}`. Clears `dirty` without running cleanup. Last resort; also clears an unreadable `state.toml`. |
| `workspaces.stop` | queued (`workspaces`) | `{"id"}` | Task handle. Runs `cleanup.{sh,ps1}` supervised on an `active` workspace; success → `ready` and `workspaces.session.stopped`; the task ends `{"id","state":"ready","log"}` (`log` empty with no script). No cleanup script: `ready` at once, nothing run. Failure leaves it `active`; the task fails with the log in `detail`. `invalid_params` when ready or dirty, `conflict` while launching. Windows are never closed: the script stops processes. ADR 0026. |
| `workspaces.remove` | inline | `{"id"}` | `{"removed":true}`. Moves the folder to `data/workspaces/.removed/<id>/` (the latest removal of an id replaces an older one); emits `workspaces.workspace.removed`. `conflict` when active, launching or dirty ("stop it first" / "clean it up or reset it first"). Folders starting with `.` are never listed as workspaces. ADR 0026. |
| `workspaces.restore` | inline | `{"id"}` | The workspace, as `workspaces.show` shows it, back exactly as removed; emits `workspaces.workspace.restored`. `not_found` with nothing to restore; `conflict` if that id exists again. ADR 0026. |
| `workspaces.purge` | inline | `{"id"?,"confirm"?}` | `{"purged":N}`. Permanently deletes one removed workspace, or every one. Without a matching `confirm`, `confirmation_required` with `detail: {"workspaces":N}`; re-send with `"confirm": {"workspaces":N}`. `{"purged":0}` without asking when nothing is removed; `not_found` for an `id` that isn't removed. Emits `workspaces.workspace.purged`. ADR 0038. |
| `workspaces.copy` | inline | `{"id","to","values"?,"label"?}` | `{"from","to","changed":[{"name","from","to"}]}`: a new workspace at `<to>` with every file of `<id>` but its state (it starts `ready`, no history), `values` as new answers (checked as `workspaces.reconfigure` checks them; `invalid_params` for a workspace made by hand) and `label` as its label. The original may be in any state: it's only read. Emits `workspaces.workspace.copied`. A copy that wouldn't be a valid workspace isn't made. ADR 0026 §2b. |
| `workspaces.rename` | inline | `{"id","to"}` | `{"from","to","notes":[…]}`: every file of the workspace moved to `data/workspaces/<to>/` in one transaction; emits `workspaces.workspace.renamed`. Only while `ready` (`conflict` when active, launching or dirty: stop and cleanup find what a launch started by its id). `to` must be a valid id (`invalid_params`), not the same, and free (`conflict`). `notes` names what stays under the old id outside the Shimmer folder (a separate browser window's profile) and how to move it. Schedules aren't changed: the CLI lists the ones naming the old id and moves them (`scheduler.add` again, then `scheduler.remove`) only when asked. ADR 0026 §2a. |
| `workspaces.templates` | inline | `{"id"?, "files"?}` | `{"templates":[{"id","label","description","category","needs":[…],"good_to_know":[…],"on_stop","steps":[{"name","mode","timeout_s"?,"description"?}],"questions":[{"name","prompt","kind","required","default"?,"choices"?,"help"?}]}],"categories":[{"id","label"}]}`: templates by category (in `categories`' order), then id; `categories` gives each heading. With `id`, only that template (`not_found` naming the others); with `files: true`, each template also has `"files":[{"path","text"}]`: every file a workspace made from it gets (not `template.toml`), for `peek --scripts`. `kind` is `text`, `command`, `folder`, `file`, `url`, `urls` or `choice`. ADR 0025. |
| `workspaces.answers` | inline | `{"id"}` | `{"id","template","questions":[…as workspaces.templates],"answers":{NAME: value or null}}`: the template the workspace was made from (its `.template` file, or the "Created from Shimmer's X template" line in `workspace.toml`) and its current answers. `invalid_params` for a workspace made by hand. ADR 0025 §9. |
| `workspaces.reconfigure` | inline | `{"id","values":{NAME: answer}}` | `{"id","template","changed":[{"name","from","to"}]}`. Only while `ready` (`conflict` when active, launching or dirty: cleanup reads the answers). Only `[env]` in `workspace.toml` is rewritten; changed answers are checked as `create` checks them; an empty one clears an optional question. Nothing changed: nothing written. ADR 0025 §9. |
| `workspaces.new` | inline | `{"id","template","values"?,"label"?}` | The new workspace, as `workspaces.show` shows it (state `ready`). `values` maps question names to strings; left out means the default, or empty. Every answer is written to the workspace's `[env]`. A missing required answer, an unknown question, or an answer that doesn't fit its kind (`folder`/`file` must be absolute, `url`s must start with `http://` or `https://`) is `invalid_params` naming the question; an unknown template is `not_found` naming the ones there are; anything already at `data/workspaces/<id>/` is `conflict`, never overwritten. One transaction writes the folder and emits `workspaces.workspace.created`. ADR 0025. |
| `workspaces.health` | inline | `{}` | `{"checks":[…]}` in the shape ADR 0033 sets: a `fail` for each workspace that can't be read and each dirty one (fix: `workspaces.cleanup`, or `workspaces.reset` with no cleanup script), a `warn` for each with a detached step and no cleanup script, or one `ok` line. Writes nothing. |

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
| `records.collections` | inline | `{"removed"?}` | `{"collections":[...]}`, each collection definition as JSON (+ `"skipped"` naming each malformed collection file). A collection made from a built-in template also has `"template"`: its id, read from the "Created from Shimmer's X template" line its file starts with (absent for one made by hand, or once that line is edited away). ADR 0030 §4. With `"removed": true`, the removed collections instead, by id: `{"collections":[{"id","label"?,"records","trash"}]}` (no `label` when its file doesn't parse). ADR 0021 §5a. |
| `records.add` | inline | `{"collection","id"?,"fields"?}` | The new item, which carries its `id`. Without an `id`, one is made from the collection's `title` (`"Two Sum"` → `two-sum`, then `two-sum-2`, …), or from today's date (`2026-10-05-1`) when there's no title to use. An explicit `id` that exists is `conflict`. ADR 0018. |
| `records.show` | inline | `{"collection","id"}` | The item. |
| `records.list` | inline | `{"collection","filter"?,"search"?,"sort"?,"limit"?,"offset"?,"removed"?}` | `{"items","total","titles"}` (+ `"skipped"` if a hand-edited file was unreadable). `titles` is `{id: title}` for the listed items (ADR 0024 §5). Ordered by `sort`, then by id. See below. With `"removed": true`, the collection's removed records instead, by id: `{"items":[…]}` (+ `"skipped"`); it takes no filter, search, sort or paging (`invalid_params`). ADR 0038 §6. |
| `records.update` | inline | `{"collection","id","fields"}` | The item. A `null` field value unsets it. A list field takes a whole new list or `{"add":[…],"remove":[…]}` (ADR 0024 §2). Setting a `unique` field to a value another record has is `conflict`, naming that record. |
| `records.complete` | inline | `{"collection","id","fields"?}` | The item, now `done`, with its `stamp_on_complete` field set. `fields` are applied first, with `records.update`'s rules; an explicit value for the stamp field wins over today (back-dating). One `records.item.completed`. When the record is already `done`: re-stamped again, unless the collection says `repeat_complete = "refuse"`, then `conflict`. |
| `records.rename` | inline | `{"collection","id","new_id"}` | The item under its new id. One transaction moves the file, rewrites every `ref` to it, and emits `records.item.renamed`. `conflict` if `new_id` exists; `not_found` if `id` doesn't. ADR 0018. |
| `records.reopen` | inline | `{"collection","id","clear_stamp"?}` | The item, now `todo`. The stamp is kept unless `clear_stamp`. `conflict` if it isn't `done`. ADR 0016. |
| `records.remove` | inline | `{"collection","id","force"?}` | `{"removed":true}`. The record moves to `data/records/trash/<collection>/<id>.toml` (the latest removed version of each id), out of every other op. ADR 0020. If other records refer to it through a `ref` field, `conflict` with `detail: {"referred_by":["collection/id",…]}` unless `force` (ADR 0024 §3). |
| `records.restore` | inline | `{"collection","id"}` | The record, back exactly as removed; emits `records.item.restored`. `not_found` if it isn't in the trash; `conflict` if the id, or one of its `unique` values, is taken again. |
| `records.purge` | inline | `{"collection","id"?,"confirm"?}` | `{"purged":N}`. Permanently deletes one trashed record, or the whole trash. Without a matching `confirm`, `confirmation_required` with `detail: {"records":N}`; re-send with `"confirm": {"records":N}`. `{"purged":0}` without asking when the trash is empty; `not_found` for an `id` not in it. Emits `records.item.purged`. ADR 0024 §5. |
| `records.import` | inline | `{"collection","rows":[{…}],"dry_run"?,"skip_invalid"?}` | `{"added":[ids],"skipped":[{"row","reason"}],"invalid":[{"row","error"}],"written":bool}`. Each row (an object of field values, plus optional `id` and `status`) is checked like `records.add` and against the other rows; rows numbered from 1. A row whose id or `unique` value exists is skipped. Everything added is written in one transaction, one `records.item.created` each; nothing is written on a `dry_run`, or while any row is invalid unless `skip_invalid`. At most 5000 rows. ADR 0024 §4. |
| `records.templates` | inline | `{"id"?, "file"?}` | `{"templates":[…],"categories":[{"id","label"}]}`: each built-in template as `records.collections` shows a collection, plus `"category"`, `"notes":["…"]` (the header comment's paragraphs, each as one line) and `"examples":["…"]` (its "How it works" lines as written); by category (in `categories`' order), then id. With `id`, only that template (`not_found` naming the others); with `file: true`, each also has `"file"`: its text exactly as `records.new` copies it. ADRs 0022, 0030 §1. |
| `records.new` | inline | `{"id","template","label"?}` | The new collection. Copies the template's file (comments kept) with only `id` and `label` changed, checks it, and writes it with `records.collection.created` in one transaction. `conflict` if the id exists; `not_found` naming the templates for an unknown one. |
| `records.check` | inline | `{"collection"}` | `{"checked":N,"problems":[{"id","problems":["…"]}]}`: records that don't fit the collection (missing required, wrong type, unknown keys, unreadable, `ref`s to records that don't exist). Writes nothing. ADR 0021. |
| `records.rename_field` | inline | `{"collection","from","to"}` | `{"collection":{…},"updated":N}` (+ `"skipped"`: each record file that couldn't be read or parsed, which keeps the old name). One transaction renames the field in the collection file (comments kept) and in every live and trashed record it can read; emits `records.field.renamed`. |
| `records.rename_collection` | inline | `{"id","new_id"}` | The collection under its new id; moves its file, records and trash, and rewrites `collection = "…"` in every ref field pointing into it, in one transaction; emits `records.collection.renamed`. `conflict` if `new_id` exists. |
| `records.copy_collection` | inline | `{"id","to","label"?,"with_records"?}` | The new collection, plus `"records":N`. Copies the collection file with only `id` (and `label`) changed, comments kept, and `ref` fields pointing at the collection itself pointed at the copy. With `with_records`, every live record file too, as written (not the trash). One transaction; emits `records.collection.copied {"collection","from","records"}`, then one `records.item.created` per copied record. `conflict` if `to` exists, `not_found` for an unknown `id`. ADR 0030 §3. |
| `records.remove_collection` | inline | `{"id","confirm"?}` | `{"removed":true,"records":N}`. Without a matching `confirm`, `confirmation_required` with `detail: {"records":N}`; re-send with `"confirm": {"records":N}`. Moves everything to `data/records/removed-collections/<id>/`; emits `records.collection.removed`. |
| `records.restore_collection` | inline | `{"id"}` | The collection, back with its records and trash; emits `records.collection.restored`. |
| `records.purge_collection` | inline | `{"id","confirm"?}` | `{"purged":true,"records":N}`. Permanently deletes a removed collection: its file, its records and its trash, `N` counting both. Without a matching `confirm`, `confirmation_required` with `detail: {"records":N}`; re-send with `"confirm": {"records":N}`. `not_found` when there's no removed collection by that id; a live collection with the same id is never touched. Emits `records.collection.purged`. ADR 0021 §5a. |
| `records.health` | inline | `{}` | `{"checks":[…]}` in the shape ADR 0033 sets: a `fail` for each collection file that won't load, a `warn` for each collection with records `records.check` finds problems in (fix: `records.check` on it), or one `ok` line. Writes nothing. |

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

## Fetchers ops

A source is a file, `data/fetchers/sources/<id>.toml` (ADR 0028 §4): `[source]` (`label`?,
`kind`, `enabled`, `interval`, `filter`?) and `[kind]` (that kind's own fields — `rss` needs
`url`; `json-api` needs `url`/`items_path`/`id_field`/`title_field`/`url_field`;
`hn-whoishiring` needs none; `wwr` needs `category`). There is no fixed, compiled-in
registry (ADR 0028 §10): every source names a `kind`, however it got there — written by
hand, through `fetchers.new`'s structured shape, or from one of the five starter templates
`fetchers.templates`/`.peek` list. A fresh install's `fetchers.list` is empty.

| Op | Execution | Params | Returns |
|---|---|---|---|
| `fetchers.run` | inline | `{"id","filter"?}` | `{"queued":true,"task_id","lane","position"}`. Checks the source exists before enqueuing `fetchers.run_task` for the real fetch, so an unknown id is refused with `not_found` immediately rather than acking "queued" and failing silently once the task runs (ADR 0028 §9). `filter` (a `core::filter::Filter`, ADR 0028 §8/§14), when given, replaces — never merges with — the source's own file filter for this one call. |
| `fetchers.run_task` | queued (`fetchers`) | `{"id","filter"?}` | `{"source","new_items"}`. Internal — `fetchers.run` enqueues this after checking the source exists, and this is also what a source's own schedule fires directly (ADR 0028 §6: no fixed heartbeat; each enabled source keeps exactly one pending `ctx.schedules` entry of its own, recomputed whenever the source is created/edited/enabled/disabled/renamed/restored, and after every run finishes). Fetches one source, through its configured `kind` (ADR 0028 §2a/§10), through to completion: pages (capped at 10), dedupes against that source's own seen-id set, and emits one `fetchers.item.found` per genuinely new item. `not_found` for an unknown source id; `module_error` ("already fetching") if a run for the same source is already in flight. Always records the attempt (even on failure) so a broken source waits a full interval before being retried. |
| `fetchers.list` | inline | `{"removed"?}` | `{"sources":[{"id","label","method","kind","enabled","interval_s","filter","last_run_at","running"}…],"problems":[{"id","problem"}…]}`, one entry per configured source (ADR 0028 §10), read from the module's own in-memory cache (ADR 0028 §5), not re-scanned from disk on every call. `last_run_at` is `null` for a source never fetched. A fresh install's `sources` is empty. With `"removed":true`: `{"removed":[id…]}` instead — every removed source's id, to restore or purge (ADR 0038 §3). Backs `shimmer fetchers list`. |
| `fetchers.show` | inline | `{"id"}` | A source in full: the same per-source shape `fetchers.list` has, plus `"text"` (its file's raw contents). `not_found` for an unknown id; `invalid_params` if its file exists but doesn't parse. Backs `shimmer fetchers show`. |
| `fetchers.status` | inline | `{}` | `{"sources","enabled","disabled","problems":[{"id","problem"}…]}` — the whole feature at a glance (how many sources exist, how many are enabled), not per-source (that's `fetchers.list`/`fetchers.show`). Backs `shimmer fetchers status`. |
| `fetchers.test` | queued (`fetchers`) | `{"id","filter"?}` | `{"source","items":[{source_name,source_id,title,url,raw_data}…]}`. Fetches exactly one page and returns its parsed items — never touches `seen.toml`, `last_run.toml`, or the overlap guard, and emits no `fetchers.item.found`; a dry run only. `filter` works exactly as in `fetchers.run` above. Backs `shimmer fetchers test`. |
| `fetchers.new` | inline | `{"id","kind","interval","label"?,"enabled"?,"params"?,"filter"?}` **or** `{"id","template","values"?,"preview"?}` (ADR 0028 §10) | A source's view (`fetchers.show`'s own shape, minus `"text"`), under `"source"` for the template shape. `params`/`[kind]` is checked by that kind's own `Kind::parse` before anything is written; a template's answers are checked by its own questions, the same way. `preview: true` (template shape only) returns `{"preview":true,"template","source":{"id","text"}}` without writing anything. `conflict` if `id` already exists; `invalid_params` for anything that fails validation, or for mixing the two shapes (nothing is written either way). Backs `shimmer fetchers new`. |
| `fetchers.templates` | inline | `{}` | `{"categories":[{"id","label"}…],"templates":[{"id","label","description","category","kind","good_to_know","questions","text"}…]}` — the five starter templates (ADR 0028 §10). Backs `shimmer fetchers templates`. |
| `fetchers.peek` | inline | `{"id"}` | The same shape as `fetchers.templates`, with exactly one template. `not_found` naming the others when there's no such template. Backs `shimmer fetchers peek`. |
| `fetchers.edit` | inline | `{"id","text"}` | `fetchers.show`'s own shape. The *whole* replacement file text, same as `automations.edit` (amended by #305's review — a structured shape like `fetchers.new`'s meant re-passing every flag just to change one, with whatever you forgot to retype silently reset). There is no template path for `edit`. `not_found` for an unknown id; `invalid_params` for text that doesn't parse (the old file is kept). Backs `shimmer fetchers edit`. |
| `fetchers.health` | inline | `{}` | `{"checks":[…]}` in ADR 0033's shape: a `fail` for each source file that won't load (fix: edit or remove its file by hand), a `warn` for each source whose most recent run failed (fix: `fetchers.show` on it) or whose `last_run.toml` entry is unreadable, or one `ok` line. Cleared by the next successful run (ADR 0028 §11) — read fresh every call, never accumulated. Writes nothing. No CLI command of its own; discovered by `shimmer doctor` via `core.manifest`, the same as every other module's `.health`. |
| `fetchers.items` | inline | `{"source"?,"contains"?,"excludes"?}` | `{"items":[{source_name,source_id,title,url,raw_data}…]}`, most recent first. Answered from the module's own capped log (ADR 0042; `automations.history`'s precedent, not `core.events.list`), not the event log — up to the last 1000 items found across every source. `source` keeps only that source's items; `contains`/`excludes` are a plain case-insensitive title substring narrowing (OR'd within each list, excludes wins), not a `core::filter::Filter`. Backs `shimmer fetchers items`. |
| `fetchers.remove` | inline | `{"id"}` | `{"removed":true,"source"}`. Moves the source's file to `removed/` (restore brings it back); `not_found` if `id` doesn't exist; `conflict` if it's fetching right now. Backs `shimmer fetchers remove`. |
| `fetchers.restore` | inline | `{"id"}` | `{"restored":true,"source"?,"problem"?}` — `source` (the usual view) when the restored file still parses, otherwise `null` with `problem` naming why. `not_found` if nothing removed matches `id`; `conflict` if a source `id` already exists again. Backs `shimmer fetchers restore`. |
| `fetchers.purge` | inline | `{"id"?,"confirm"?}` | `{"purged":N}`. One removed source, or (with no `id`) every one — asks first, same shape as `records.purge`/`automations.purge`: without a matching `confirm`, `confirmation_required` with `{"sources":N}` in `detail`; re-sent with that same object to actually delete. Also deletes that source's dedup state (`<id>/seen.toml`) and its `last_run.toml` entry, so a new source reusing the id starts clean. `{"purged":0}` without asking when there's nothing removed; `not_found` for an `id` that isn't removed. Backs `shimmer fetchers purge`. |
| `fetchers.enable` / `.disable` | inline | `{"id"}` | The usual source view, `"enabled"` flipped. `not_found` for an unknown id. Backs `shimmer fetchers enable`/`disable`. |
| `fetchers.rename` | inline | `{"id","to"}` | The usual source view, under `to`. Moves the source's file, its dedup state (`<id>/seen.toml`) and its `last_run.toml` entry together (ADR 0028 §7), so none of that resets. `conflict` if `to` already exists, `id` equals `to`, or `id` is fetching right now (renaming mid-fetch would duplicate that run's items, #305's review); `not_found` if `id` doesn't exist. Backs `shimmer fetchers rename`. |
| `fetchers.copy` | inline | `{"id","to","label"?}` | The usual source view, under `to`, switched off — starting with no dedup or last-run state of its own (a new definition, not a continuation). `conflict` if `to` already exists, `id` equals `to`, or `id` is fetching right now; `not_found` if `id` doesn't exist. Backs `shimmer fetchers copy`. |
| `fetchers.reload` | inline | `{}` | `{"sources":N,"problems":[{"id","problem"}…]}`. Re-reads every `data/fetchers/sources/*.toml` (ADR 0028 §5) — needed after hand-editing a source file, since the module otherwise only reads them at daemon start. |

A fetch's failure reaches the client as `queue.task.failed` like any other queued op, and
additionally as `fetchers.fetch.failed` on the bus (see the topic catalogue below) — the
latter is what a dashboard or a configured `notify.*` fallback actually watches.

Module ops are namespaced `<module>.<verb>`. The daemon routes on the prefix; a collision
between two modules is a startup failure, not a runtime surprise.

**Aliases never appear on the wire.** `ikuzo`, `engage` and every other command-pack name
are resolved to canonical ops by the client before a frame is sent. The daemon rejects an
alias as `unknown_op`. Swapping or installing a pack therefore never requires a protocol
change or a daemon restart. See `CLAUDE.md` §15.1.

---

## Automations ops

Rules that react to events by queueing ops (ADR 0035). One file per rule,
`data/automations/rules/<id>.toml`; every op is inline. A rule's actions are queued with
`ctx.queue` in each op's own lane (origin `module: automations`), or added as a one-time schedule
(`after`, source `{"type":"added","id":"automations","label"}`). A rule is checked when created,
saved and loaded: its `on` pattern, its `when` filter, every op exists and none is
`automations.*`; a refused rule is `invalid_params` with what's wrong. Each rule is shown as
`{"id","label","enabled","builtin","on","actions":[{"op","params","after_s"}],"last_fired","loop_stopped_at"}`.

| Op | Execution | Params | Returns |
|---|---|---|---|
| `automations.list` | inline | `{"removed"?}` | `{"rules":[…],"problems":[{"file","error"}]}`: problems are rule files that couldn't be used. With `"removed": true`, the removed rules instead, `{"rules":[{"id","label"}]}`, to restore or purge (ADR 0038 §6). |
| `automations.show` | inline | `{"id"}` | One rule, plus `"text"`: its file. |
| `automations.new` | inline | `{"id","text"?,"label"?}` or `{"id","template","values"?,"preview"?}` | The rule. Without `text`, a commented starter rule, switched off. `conflict` if it exists. With `template`: `values` maps its questions to text; the rules it makes (`<id>`, or `<id>-<suffix>` for each when it makes several) are checked and made together, on, as `{"template","rules":[…]}`; with `preview: true`, `{"preview":true,"template","rules":[{"id","text"}]}` and nothing is made. `unavailable` naming the module when the template runs an op no installed module has; `conflict` naming every id already taken. |
| `automations.templates` | inline | `{"id"?}` | `{"categories":[{"id","label"}],"templates":[{"id","label","description","category","good_to_know","questions","rules":[{"suffix","text"}],"ops"}]}`. A rule's text takes answers as `${NAME}`; a question's `kind` is `text`, `name`, `number`, `choice`, `list` or `pairs`. ADR 0035 §9. |
| `automations.edit` | inline | `{"id","text"}` | The rule. A broken rule is refused and the old one kept. |
| `automations.remove` | inline | `{"id"}` | `{"removed":true,"rule"}`. Moves the file to `data/automations/removed/` (replacing an older removed rule of the same id); also removes a rule file that couldn't be used. |
| `automations.restore` | inline | `{"id"}` | `{"restored":true,"rule","problem"}`: the rule back as it was. One that no longer checks is still restored, with `rule: null` and the `problem` (also in `list`). `conflict` if a rule has that id, `not_found` if none was removed. |
| `automations.purge` | inline | `{"id"?,"confirm"?}` | `{"purged":N}`. Permanently deletes one removed rule, or every one. Without a matching `confirm`, `confirmation_required` with `detail: {"rules":N}`; re-send with `"confirm": {"rules":N}`. `{"purged":0}` without asking when nothing is removed; `not_found` for an `id` that isn't removed. Emits `automations.rule.purged`. ADR 0038. |
| `automations.rename` | inline | `{"id","to"}` | The rule under its new id (as `automations.show` without `text`); emits `automations.rule.renamed {from, to}`. A built-in rule is `conflict` (it has no file); so is a `to` that's taken. Its history moves to the new id, and a delayed action it set that is still waiting follows it (recorded in `renamed.toml`); a rule given the old id later keeps only its own. A `renamed.toml` that doesn't parse is `conflict`, never replaced. ADR 0038 §6. |
| `automations.copy` | inline | `{"id","to","label"?}` | The new rule, **switched off** so the same event doesn't fire both, with `label` if given; emits `automations.rule.copied {from, to}`. ADR 0038 §6. |
| `automations.enable` / `.disable` | inline | `{"id"}` | The rule; only `enabled` in its file changes. |
| `automations.reload` | inline | `{}` | `{"rules":N,"problems":[…]}`, after re-reading files edited by hand. |
| `automations.test` | inline | `{"id","event":{"topic","payload"}}` | `{"rule","enabled","matches","actions":[{"op","params","after_s"}]}`: what it would do, as if on. Runs nothing. |
| `automations.events` | inline | `{}` | `{"events":[{"topic","description"}]}`: everything a rule can react to. |
| `automations.history` | inline | `{"id"?,"since"?,"limit"?}` | `{"history":[…]}`, newest first (the last 300 kept): `rule.fired` and `chain.stopped` payloads plus `at` and `kind` (`fired` / `stopped`). |
| `automations.status` | inline | `{}` | `{"rules","enabled","depth","stopped":[…],"problems":[…]}`. |
| `automations.depth` | inline | `{}` or `{"depth":N}` (1–10) | `{"depth"}`: how many rules may follow each other in a chain (default 3). |
| `automations.health` | inline | `{}` | `{"checks":[…]}` in ADR 0033's shape: a `fail` per unusable file, a `warn` for a recently stopped loop, or one `ok` line. |

**Built-in rules** (ADR 0035 §10): `workspace-dirty`, `workspace-half-launched`,
`scheduled-task-failed` and `loop-stopped` are always listed (`"builtin": true`) once the ops they run
are installed (they send with `notify.send`). `get` shows their text; `enable`/`disable` switch them
in `settings.toml`'s `[builtin]` (and their event carries `"builtin": true`); `remove`, `save`, and
`create` or `restore` with their ids are `conflict`. A rule file of yours with a built-in's id is
reported in `problems`, not run.

**The loop guard:** an event whose `cause` is a task automations queued continues that task's
chain of rules. A rule already in the chain doesn't fire again, and a chain stops at the depth
limit; either way `automations.chain.stopped` is emitted. Everything else starts a new chain.

---
## Notify ops

ADR 0032. Every op that picks messages takes the same filters, all optional and combined:
`{"unread"?, "read"?, "state"?: "pending"|"held"|"delivered"|"failed",
"from"?: "send"|"schedules"|"fallbacks"|"automations", "priority"?: "low"|"normal"|"high",
"since"?, "before"?, "text"?}`. `since`/`before` are a date (`2026-10-01`, the start of that local
day), a local `2026-10-01 14:00`, or RFC 3339; `text` matches the title or body, ignoring case.

| Op | Execution | Params | Returns |
|---|---|---|---|
| `notify.send` | queued (`notify`) | `{"title","body"?,"priority"?,"from"?}`; a fallback's params (`failure_code`, `failure_reason`, `failure_detail`, `message`) are read too, unknown keys ignored | The task ends with the message (below). Never fails because delivery did: the message is kept as `failed`. |
| `notify.test` | queued (`notify`) | `{}` | `{"deliveries":[{"medium","at","ok","error"?}],"sinks":[…]}`: a test to every sink, past quiet hours and pause; not kept |
| `notify.resend` | queued (`notify`) | `{"ids":[…]}` or filters | `{"items":[…]}`: each tried again, the new deliveries added to it |
| `notify.flush` | queued (`notify`) | `{}` | `{"delivered":N}`: what quiet hours or a pause held, as one summary. Run hourly by notify's own trigger `notify.flush`. |
| `notify.list` | inline | filters, `"limit"`? (default 50) | `{"items":[…],"counts":{"total","unread","high","held","failed"},"removed_unread"?}`, newest first; each item also has `short`, the ending to show for it |
| `notify.count` | inline | filters | `{"count", "total","unread","high","held","failed"}` |
| `notify.show` | inline | `{"id"}` | the message |
| `notify.mark_read` / `notify.mark_unread` | inline | `{"ids":[…]}`, `{"all":true}` or filters | `{"marked":N}` |
| `notify.remove` | inline | `{"ids":[…]}`; or filters / `{"all":true}` with `"confirm":true` | `{"removed":N}`. A bulk remove without `confirm` answers `confirmation_required` with `detail: {"count","unread"}`. |
| `notify.pause` | inline | `{"until"?: RFC 3339 after now, "all"?}` | `{"paused":{"since","until"?,"all"}}`; holds low and normal (and high with `all`) until `resume` or `until` |
| `notify.resume` | inline | `{}` | `{"was_paused","held"}`; enqueues `notify.flush` when something waited |
| `notify.quiet` | inline | `{}` shows; `{"every_day"|"weekdays"|"weekends"|"mon"…"sun": "22:00-08:00" or null}` sets or clears; `{"off":true}` clears all | `{"quiet":{…},"quiet_now"}` |
| `notify.cap` | inline | `{}` shows; `{"max":N}` (at least 100) sets and applies it | `{"max","messages","removed_unread"?}` |
| `notify.status` | inline | `{}` | `{"paused","quiet","quiet_now","cap","counts","removed_unread","sinks","settings_problem"}` |
| `notify.health` | inline | `{}` | `{"checks":[…]}` in ADR 0033's shape: a `warn` for a `settings.toml` it can't use, each inbox file it can't read, messages no sink delivered (fix: `notify.resend {"state":"failed"}`) and a pause left on; otherwise one `ok`. Writes nothing. ADR 0038 §6. |

A message: `{"id","at","priority","title","body"?,"source","state","read","delivery":[{"medium","at","ok","error"?}]}`.
`source` is `send`, `schedule`, `fallback` or `automation:<rule>` (a rule's message, built-in ones too: ADR 0035 §10). Messages are files
in `data/notify/inbox/`; settings are `data/notify/settings.toml`.

Every `id` and `ids` entry may be a unique ending of a message id, as for tasks (see "Short ids"
above): at least 4 characters, `conflict` when several match, `not_found` when none does. A list
item's `short` is its last 6 characters, or more where another message in the inbox ends the same
way: unique across the whole inbox, not only the page returned, so it always works given back.

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
| `queue.task.failed` | Task failed. Payload includes `error`, `attempts`, `lane`, `origin`, and `fallback`: `true` only when a fallback is being sent for this failure (ADR 0005, 0032 §4). |
| `queue.task.cancelled` | Task cancelled before or during execution. |
| `queue.task.promoted` | A task jumped the lane. Payload lists displaced task ids. |
| `queue.task.reordered` | A waiting task moved within its lane. Payload `{task_id, lane, before, queue_version}`. |
| `queue.fallback.enqueued` | A failed task's fallback was enqueued (ADR 0005). Payload `{task_id, fallback_task_id, op, lane}`. |
| `queue.fallback.failed` | A failed task's fallback couldn't be enqueued, e.g. no `notify` module. Payload `{task_id, op, error, code}`. Never stalls the lane; nothing retries. |
| `scheduler.trigger.fired` | Trigger fired and enqueued a task. |
| `scheduler.trigger.skipped` | Trigger fired while its previous task was still pending (`reason: "overlap"`), or its task could not be enqueued (`reason: "enqueue_failed"`). |
| `scheduler.trigger.missed` | A `catch_up` policy dropped due occurrences. Payload `{trigger_id, count, catch_up}`. |
| `scheduler.trigger.added` / `.edited` / `.removed` / `.disabled` / `.enabled` / `.restored` | Trigger lifecycle. `.restored`: `{trigger_id}` (ADR 0038). |
| `scheduler.trigger.purged` | `scheduler.purge` deleted removed triggers for good. `{triggers}`, plus `trigger_id` when one was purged. ADR 0038. |
| `workspaces.session.launched` | All launch steps succeeded. Payload `{workspace, session_id, forced, steps}`. |
| `workspaces.session.dirty` | A launch step failed, a launch was cancelled part-way, or the daemon restarted mid-launch. Payload `{workspace, reason, failed_step, failed_at, log}` (+ `session_id`, `forced`, `has_cleanup_script` when a launch was running; `recovered: true` when the daemon restarted mid-launch, the one case no failed task reports, ADR 0032 §4). |
| `workspaces.session.forced` | Force relaunch of a dirty workspace, before any step runs. Payload `{workspace, prior}`, `prior` being the dirty state it overrides. |
| `workspaces.session.cleaned` | Cleanup script succeeded; state back to `ready`. Payload `{workspace, log}`. |
| `workspaces.session.abandoned` | A launch (forced or not) was claimed but no step ran: cancelled first, or step 1 couldn't start. The workspace is back to the state it had. Payload `{workspace, reason, forced, back_to}`. |
| `workspaces.workspace.reset` | `workspaces.reset` cleared `dirty` without cleanup. Payload `{workspace, prior}`. |
| `workspaces.session.stopped` | `workspaces.stop` finished. Payload `{workspace, ran_cleanup, log}` (`log` empty when there was no cleanup script). ADR 0026. |
| `workspaces.workspace.removed` / `.restored` | `{workspace}`. ADR 0026. |
| `workspaces.workspace.purged` | `workspaces.purge` deleted removed workspaces for good. `{workspaces}`, plus `workspace` when one was purged. ADR 0038. |
| `workspaces.workspace.created` | `workspaces.new` made a workspace from a template. Payload `{workspace, template}`. ADR 0025. |
| `workspaces.workspace.copied` | `workspaces.copy` made a workspace from another. Payload `{from, to, changed: [name…]}` (names, never values). ADR 0026 §2b. |
| `workspaces.workspace.renamed` | `workspaces.rename` gave a workspace a new id. Payload `{from, to}`. ADR 0026 §2a. |
| `workspaces.workspace.reconfigured` | `workspaces.reconfigure` changed a workspace's answers. Payload `{workspace, changed: [name…]}`: the names, never the values. ADR 0025 §9. |
| `records.item.created` / `.updated` / `.completed` / `.reopened` / `.renamed` / `.removed` / `.restored` | Record mutations. All carry `{collection, id, title, deadlines, item}` (for `.removed`, `item` is the record as it was): `item` is the full wire item, `title` the record's readable name (the collection's `title` template, or the id), `deadlines` `{field: date}` for every set field with `role = "deadline"` (ADR 0017). `.updated` also carries `changed`: the fields whose value actually changed, sorted (`[]` when nothing did). `.renamed` also carries `new_id` (`id` is the old one; `item` is under the new one) and `references` (how many `ref` values were rewritten to point at the new id). |
| `records.collection.created` | `records.new` made a collection from a template. Payload `{collection, template}`. Nothing is created on first start (ADR 0023). |
| `records.collection.copied` | `records.copy_collection` made a collection from another. Payload `{collection, from, records}`. ADR 0030 §3. |
| `records.field.renamed` | `{collection, from, to}`. |
| `records.collection.renamed` / `.removed` / `.restored` / `.purged` | `{collection, new_id}` / `{collection, records}` / `{collection}` / `{collection, records}`. `.purged`: a removed collection was deleted for good (ADR 0021 §5a). |
| `records.item.purged` | `records.purge` deleted trashed records for good. `{collection, records}`, plus `id` when one record was purged. ADR 0024 §5. |
| `automations.rule.created` / `.edited` / `.removed` / `.restored` / `.enabled` / `.disabled` | `{rule}`. ADR 0035. |
| `automations.rule.renamed` / `.copied` | `{from, to}`. ADR 0038 §6. |
| `automations.rule.purged` | `automations.purge` deleted removed rules for good. `{rules}`, plus `rule` when one was purged. ADR 0038. |
| `automations.rule.fired` | A rule reacted to an event. `{rule, event: {id, topic}, chain, depth, queued: [{op, task_id, lane}], scheduled: [{op, trigger_id, at}], failed: [{op, error}]}`. |
| `automations.chain.stopped` | The loop guard stopped a rule. `{chain, rules, rule, reason: "cycle" \| "depth", event: {id, topic}}`. |
| `automations.settings.changed` | `{depth}`. |
| `fetchers.item.found` | A source returned a new, deduplicated item (one per item, inside the same transaction that updates that source's seen-id set). Payload `{source_name, source_id, title, url, raw_data}`; `raw_data` past 8KiB is replaced with `{"truncated":true,"preview":…}`. |
| `fetchers.fetch.finished` | A fetch run completed without error. Payload `{source, new_items}`. |
| `fetchers.fetch.failed` | A fetch run failed. Payload `{source, error, reason}`. `reason` is one of `"network"` (a transport-level failure), `"http_error"` (a non-2xx, non-cooldown response), `"blocked"` (a `Method::Scrape` source's own robots.txt refused the fetch — the page was never requested), `"cancelled"`, or `"parse_empty"` (anything else: a malformed or unexpectedly-shaped response body, no matching "who is hiring" thread, or a corrupt local `seen.toml` — the last of these is a deliberate, already-tested tradeoff: a corrupt dedup file is a loud, reported error forever, never silently reset to an empty set). |
| `calendar.date.registered` | A dated entry was stored. Planned (M6): not emitted by any module yet. |
| `notify.message.received` | A message landed in the inbox. `{id, title, body, priority, source}`, plus `removed_unread` when the cap removed unread ones to fit. ADR 0032. |
| `notify.message.held` | Quiet hours or a pause held it. `{id}`. |
| `notify.message.sent` | At least one sink took it. `{id, mediums}`. |
| `notify.message.exhausted` | Every sink failed; it stays in the inbox as `failed`. `{id, errors:[{medium, error}]}`. |
| `notify.message.read` / `.unread` / `.removed` | `{ids}`; `.removed` also `reason`: `removed` or `cap`. |
| `notify.pause.started` / `.ended` | `{until, all}` / `{held}`. |
| `notify.settings.changed` | A setting changed: one of `{quiet}`, `{cap}`. |

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
$ shimmer --socket /tmp/shimmer-mock.sock manifest    # any CLI command works against it today
$ shimmer tui --socket /tmp/shimmer-mock.sock         # once crates/tui exists (M5)
```

Both flags are required, so the mock can never silently take over the real daemon's socket.

This is what decouples the TUI work. Dev C should never be blocked waiting for a
daemon-side op to land — write the fixture (a normal PR to `crates/mockd`), build against it,
swap to the real daemon at M5.

---

## Worked example

Connect, discover, subscribe, enqueue, watch it run.

```jsonc
→ {"v":2,"kind":"hello","client":"tui","client_version":"0.1.0"}
← {"v":2,"kind":"welcome","daemon_version":"0.1.0","session":"01JD2KQ…"}

→ {"v":2,"kind":"request","id":"1","op":"core.manifest","params":{}}
← {"v":2,"kind":"response","id":"1","ok":true,"data":{"protocol":2,"lanes":[…],"modules":[…]}}

→ {"v":2,"kind":"subscribe","id":"2","topics":["queue.*","fetchers.*"]}
← {"v":2,"kind":"response","id":"2","ok":true,"data":{"subscribed":2}}

→ {"v":2,"kind":"request","id":"3","op":"fetchers.run","params":{"id":"hn-whoishiring"}}
← {"v":2,"kind":"response","id":"3","ok":true,
   "data":{"queued":true,"task_id":"01JD2N…","lane":"fetchers","position":1}}

← {"v":2,"kind":"event","event":{"topic":"queue.task.started",
   "payload":{"task_id":"01JD2N…","lane":"fetchers"}, …}}
← {"v":2,"kind":"event","event":{"topic":"queue.task.progress",
   "payload":{"task_id":"01JD2N…","fraction":0.4,"note":"page 2 of 5"}, …}}
← {"v":2,"kind":"event","event":{"topic":"fetchers.item.found",
   "payload":{"source":"hn-whoishiring","id":"c-41883"}, …}}
← {"v":2,"kind":"event","event":{"topic":"queue.task.finished",
   "payload":{"task_id":"01JD2N…","new_items":12}, …}}

// meanwhile, a scheduled workspace launch fails in another lane
← {"v":2,"kind":"event","event":{"topic":"queue.task.failed",
   "payload":{"task_id":"01JD2M…","lane":"workspaces","origin":"scheduler",
     "error":"step 3/7 exited 1","workspace_dirty":true,"fallback":true}, …}}
← {"v":2,"kind":"event","event":{"topic":"notify.message.sent",
   "payload":{"id":"01JD2P…","mediums":["desktop"]}, …}}
```

Two things to read out of the last four frames. The failing workspace task did not stall
the `fetchers` lane — different lanes, independent. And the scheduled trigger still reached
the user, as a notification instead of a launch, because its `fallback` fired. That is the
whole design working: decoupled, observable, and degrading rather than dropping.
