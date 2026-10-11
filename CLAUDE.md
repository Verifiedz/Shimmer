# CLAUDE.md

Source of truth for this repository. Every agent session and every developer works from
this file. If code and this file disagree, **this file wins** — fix the code or propose a
change to this file, never both silently.

The product is **Shimmer**: command `shimmer`, crates `shimmer-*`, env vars `SHIMMER_*` (ADR 0011).
These names are final; `SHIMMER_*` and the data directory are part of the script ABI (§10.1).

---

## 1. What we are building

A local-first software engineering platform: workspaces, a task queue, a scheduler,
trackers, fetchers, a calendar, and notifications — in one place, on one machine, with no
server and no account required.

The product goal is **generality first**. We are not building a LeetCode tracker. We are
building a records engine that a LeetCode tracker is a forty-line configuration of. Every
feature request should be answered with "which general mechanism does this specialise?"
before any code is written.

### 1.1 The core mechanisms

Everything a user sees is a specialisation of one of these.

| Mechanism | Owns | Specialised by |
|---|---|---|
| `queue` | Executing work: lanes, ordering, concurrency, progress, cancellation | Nothing — every other mechanism feeds it |
| `scheduler` | *When* work happens: recurring, one-shot, calendar-driven triggers | Triggers registered by any module |
| `workspaces` | Launching configured sessions from user scripts | Per-setup scripts (Hyprland, macOS, Windows) |
| `records` | Typed collections: schema, storage, filtering, completion metrics | Job apps, LeetCode, OSS repos, outreach, docs, career fairs, saved links |
| `fetchers` | Reaching outward: scheduling, dedup, backoff (rate limits and caching live in `ctx.http`, ADR 0027) | Job boards, tech news, docs sites, transcript APIs |
| `notify` | Delivering messages: batching, retry, quiet hours, fallback sinks | Email, phone push, desktop, webhook |
| `calendar` | Shared dated entries other mechanisms read and write | Deadlines, career fairs, scheduled sessions |

`queue`, `scheduler` and the event bus are **core services** living in the daemon, reached
through `Ctx`. The rest are **modules**, registered at startup, each owning a data
namespace. A contributor needs to know which they are adding before writing a line —
services live in `crates/daemon`, modules in `crates/modules/`.

### 1.2 Queue and scheduler are separate

This split is load-bearing and easy to collapse by accident.

- **The scheduler owns time and nothing else.** It holds triggers — "every two days", "at
  09:00 Mondays", "once, on 2026-10-04" — and when one fires it *enqueues a task*. It
  never executes anything and never knows what a task does.
- **The queue owns execution.** It is the single pipeline every unit of work passes
  through, whether the user started it by hand, a scheduler trigger fired it, or a module
  reacted to an event. It enforces per-lane concurrency, orders by priority, reports
  progress, and is directly visible and reorderable by the user.

Because there is exactly one queue, "what is my machine doing right now" has exactly one
answer, and a new kind of work costs nothing in observability.

### 1.3 Integrated, but never coupled

Features must act on each other without knowing about each other. The only permitted
channel is the event bus.

Worked example — an OA arrives and its deadline eventually notifies you:

1. `fetchers` pulls a board or inbox and emits `fetchers.item.found`.
2. `records` is subscribed, matches the item to a job application the user saved earlier,
   writes the OA deadline onto it, emits `records.item.updated`.
3. `calendar` is subscribed to dated record changes, stores the date, emits
   `calendar.date.registered`.
4. `scheduler` is subscribed, registers a one-shot trigger for that date.
5. On the day, the trigger fires and enqueues a task.
6. `queue` runs it; `notify` delivers it and emits `notify.sent`.

Six mechanisms cooperated. **No module called another module.** Delete `calendar` and the
chain degrades rather than fails to compile. That property is the whole design, and it is
what makes third-party plugins possible: a plugin subscribes to topics and emits topics,
exactly like a first-party module, with no special access.

### 1.4 Why files are the truth and SQLite is an index

Everything durable is a human-readable file the user could edit by hand — TOML for
configuration and collections, Markdown for notes, JSONL for the append-only event log.
That is what makes `$SHIMMER_HOME` copyable between machines and committable to git.

It is also what does not scale. Rendering a two-year heatmap, or answering "which
applications go stale this week", by rescanning every file is fine at a hundred records
and unacceptable at fifty thousand.

So the daemon maintains a **derived** SQLite index: it reads the files and the event log
and keeps a queryable projection of them. Relational-speed queries for the TUI, without
giving up portability. The index is disposable by design — delete it, corrupt it, or
arrive on a fresh machine with only the files, and the daemon rebuilds it from the event
log. Nothing is lost because nothing was ever only in there.

Never sync `index.sqlite`. Never write to it outside the indexer. Never read a value from
it that does not also exist in a file.

### 1.5 No account, ever, for the core product

Optional network-backed features may arrive later. They do not change this:

- The app is **fully functional with no account and no network**, and must remain so.
- Any identity or sync lives in an **optional module** that can be absent or uninstalled.
- `crates/core` never learns what a user is. No `UserId`, no session, no token type.

If authentication ever reaches into `core`, local-first is over. Agents encountering a
task that seems to require it must stop and report rather than implement it.

### 1.6 Non-negotiable properties

| Property | What it means concretely |
|---|---|
| Local-first | No network needed for any core function. No account required, ever. |
| Files are truth | Everything durable is a human-readable file. SQLite is disposable. |
| Portable | Copy `$SHIMMER_HOME` to another machine and it works. Git-syncable. |
| One writer | Exactly one process mutates state: the daemon. |
| Decoupled | Features observe each other through events. No module calls another module. |
| Extensible | A new tracker, source or sink must not require changing existing code. |

### 1.7 Roadmap, explicitly not in scope yet

Heatmap-driven activity nudges, forced tracking mode (daily progress bar), locked-in mode
(separate privileged binary — §10.4), video transcription and summarisation, plugin
subprocess host, GUI, RPG/leaderboard layer.

---

## 2. Process model

One long-lived **daemon** owns all state. Everything else is a thin client talking to it
over a Unix domain socket.

```
CLI ─┐
TUI ─┼─→ unix socket ─→ daemon ─→ local files ─→ sqlite index (derived)
GUI ─┘   (bidirectional)
```

- Modules are **async tasks inside the daemon**, never subprocesses.
- The daemon is the only process that opens the data directory for writing.
- Clients hold no business logic. A client that computes something is a bug.
- The CLI auto-starts the daemon if the socket is dead, then retries once. The user never
  starts the daemon manually — same behaviour as `tmux` or `ssh-agent`.

The one exception: **workspace launching spawns real OS processes.** See §10.

---

## 3. Repository layout

```
crates/
  core/          Module trait, Ctx, Event, Task, Manifest, ids, errors, retry helper.
                 Depends on nothing internal.
  proto/         IPC wire types. Depends on core only.
  store/         Files (truth) + sqlite index (derived). Depends on core.
  daemon/        Core services: registry, event bus, queue, scheduler, HTTP gateway,
                 IPC server, indexer.
  modules/
    records/     Collection trait + generic record storage, filtering, metrics.
    fetchers/    Source trait + fetch execution, dedup.
    workspaces/  Workspace manifests, state machine, script execution.
    notify/      Sink trait + delivery, batching, fallback sinks.
    calendar/    Shared dated entries; registers triggers with the scheduler.
  cli/           Thin client (lib). Command parsing, alias table, output rendering.
  tui/           Thin client (lib). ratatui. Owned by Dev C.
  mockd/         Mock daemon (lib): fixtures + scripted events over the real protocol,
                 so clients build without a daemon. Run as `shimmer mockd`. Depends on
                 core and proto only. Owned by Dev A.
  app/           The only [[bin]]. Dispatches to daemon / mockd / cli / tui.
docs/
  protocol.md    IPC contract. Change-controlled.
  decisions/     ADRs, one file per decision, numbered.
```

This is the target layout, not all of it built yet. `crates/app`'s `run_daemon` lists the
modules actually registered.

### Dependency rules (enforced, not advisory)

```
core     → (nothing internal)
proto    → core
store    → core
modules/*→ core                    ← NOT store, NOT daemon, NOT each other
daemon   → core, proto, store, modules/*
cli, tui → core, proto             ← NOT store, NOT daemon, NOT modules
mockd    → core, proto             ← a server built like a client: no store, no daemon, no modules
app      → everything
```

Two rules matter more than the rest:

1. **No module may depend on another module.** Cross-module reaction happens through
   events. Reaching for a direct call means emitting an event instead.
2. **No client may depend on `store`, `daemon`, or `modules/*`.** This is what lets Dev C
   build the TUI against `proto` alone, in parallel, without merge conflicts.

Add a `cargo deny` / CI check for these before the tree is large enough to make violations
expensive.

---

## 4. Change-controlled crates

`crates/core` and `crates/proto` are the shared contract between three people working in
separate agent sessions. They are the only place a silent divergence can hurt us.

**Agents must not modify `crates/core` or `crates/proto` unless the task explicitly says
to.** If a task seems to require a change there, stop and report what change is needed and
why. Do not work around it by duplicating the type locally.

Human process: changes need agreement from Dev A and Dev B, and an ADR in
`docs/decisions/`. Protocol changes additionally need Dev C notified and a version bump.

---

## 5. Core types

Load-bearing definitions. Treat the signatures as fixed.

### `Module`

```rust
#[async_trait]
pub trait Module: Send + Sync + 'static {
    /// Identity, version, owned data namespace, declared capabilities.
    fn manifest(&self) -> Manifest;

    /// One-time setup: migrations, loading configuration from the store.
    async fn init(&self, ctx: &Ctx) -> Result<()>;

    /// Execution lanes this module needs. Empty = use the shared `default` lane.
    fn lanes(&self) -> Vec<LaneConfig> { Vec::new() }

    /// Recurring or one-shot triggers registered with the scheduler.
    /// A trigger enqueues a task; it never executes one.
    fn triggers(&self) -> Vec<TriggerSpec> { Vec::new() }

    /// Operations this module answers. Names are `<module>.<verb>`.
    /// Each declares whether it runs inline or is enqueued.
    fn commands(&self) -> Vec<CommandSpec> { Vec::new() }

    /// Single entry point for all work: inline IPC requests AND queued tasks.
    /// The daemon decides which by reading the op's `CommandSpec`.
    async fn handle(&self, op: &str, params: Value, ctx: &Ctx) -> Result<Value>;

    /// React to an event emitted by any module, including this one.
    async fn on_event(&self, ev: &Event, ctx: &Ctx) -> Result<()> { Ok(()) }
}
```

`handle` serves both paths deliberately — a queued task is just an op invoked by the queue
instead of by a client, so there is no second handler to keep in sync.

```rust
pub struct CommandSpec {
    pub op: String,
    pub summary: String,
    pub params_schema: Value,      // JSON Schema; advisory before M3
    pub execution: Execution,
}

pub enum Execution {
    /// Answered synchronously. Must be fast and must not block. No I/O beyond the store.
    Inline,
    /// Enqueued. Returns a task id immediately. May take minutes.
    Queued { lane: LaneId },
}
```

### `Ctx` — the capability object

The only thing a module receives. What is in it defines what a module can do, so adding a
field is a security decision, not a convenience.

```rust
pub struct Ctx {
    /// Store scoped to this module's namespace. Cannot read or write outside it.
    pub store: NamespacedStore,
    /// Rate-limited, on-disk-cached HTTP client (ADR 0027). The ONLY way out to the
    /// network. Populated only for a module whose manifest declares the `"network"`
    /// capability; every other module gets a stub that fails `unavailable`, same posture
    /// as `launcher` below.
    pub http: HttpGateway,
    /// Emit-only handle onto the event bus.
    pub bus: Emitter,
    /// Enqueue work. Available to any module, not just the scheduler.
    pub queue: QueueHandle,
    /// Injectable clock. Modules must never call `Utc::now()` directly.
    pub clock: Clock,
    /// The configured `[general] local_timezone` (ADR 0009). Use with `core::time::local_date`
    /// to derive a calendar date for stamping or display — never store a local-time timestamp;
    /// event and file timestamps stay UTC.
    pub local_tz: LocalTimezone,
    /// Spawn a workspace's numbered launch steps (`steps/<index>-<name>.{sh,ps1}`) and its
    /// `cleanup.{sh,ps1}` (ADR 0010 §2a). Populated only for a module whose manifest
    /// declares the `"process"` capability; every other module gets a stub that fails
    /// `unavailable`. Modules must never call `std::process::Command` directly (§12 rule 4).
    pub launcher: Launcher,
    /// Cooperative cancellation. Long tasks must poll this.
    pub cancel: CancellationToken,
    pub config: ModuleConfig,
    pub module_id: ModuleId,
}

impl Ctx {
    /// Report progress from inside a queued task. No-op when running inline.
    pub fn progress(&self, fraction: f32, note: &str);

    /// Shared retry with exponential backoff. Modules use this rather than
    /// hand-rolling a curve. The queue itself never retries (§11.2).
    pub async fn retry_with_backoff<T, F>(&self, policy: RetryPolicy, f: F) -> Result<T>;
}
```

Forbidden in `Ctx`, permanently: a raw `PathBuf` to the data root, a raw HTTP client, a
`rusqlite::Connection`, a handle to another module, anything identity-related (§1.5), a raw
process handle or a way to name a script outside a module's own workspace directory (ADR
0010 — `Launcher` takes a logical step and a namespace-relative id, never a path).

### `Event`

One definition serves three consumers: the in-process bus, the IPC subscription stream,
and the JSONL append log. Do not create a second event type for any of them.

```rust
pub struct Event {
    pub id: Ulid,                  // sortable, generated on emit
    pub at: DateTime<Utc>,
    pub source: ModuleId,
    pub topic: String,             // "<module>.<entity>.<verb>", verb in past tense
    pub payload: Value,
}
```

Past tense is not a style preference — it stops us writing events that are really commands
in disguise.

### `Task`

```rust
pub struct Task {
    pub id: TaskId,
    pub lane: LaneId,              // fixed at creation, never renegotiated
    pub op: String,
    pub params: Value,
    pub priority: Priority,
    pub origin: Origin,            // Scheduler { trigger_id } | User | Module { id }
    pub fallback: Option<Fallback>,// §11.3
    pub enqueued_at: DateTime<Utc>,
}

pub enum Priority {
    /// Scheduler-fired. The default source of truth for ordering.
    Scheduled,
    /// Manual or module-originated. Joins the lane in normal order.
    Normal,
    /// Explicitly promoted by the user, after confirmation. Audited.
    Overridden { by: String, at: DateTime<Utc> },
}
```

### Deliberately NOT designed yet: `ViewSpec`

Panels and widgets need a declarative view spec so one module implementation renders in
both TUI and GUI. **We are not designing it now.** Dev C builds the tables and heatmaps the
TUI actually needs first; we extract the spec from working code at M5. An abstraction
invented before there is a renderer will fit nothing.

Agents: do not add a `views()` method, a `ViewSpec` enum, or any rendering type to `core`.

---

## 6. Queue and scheduler

### 6.1 Lanes

The queue is not one pipeline. It is N independent lanes, each with its own concurrency
limit and its own ordering. A lane is a resource-contention boundary: two tasks share a
lane when running them simultaneously would corrupt something or thrash something.

```rust
pub struct LaneConfig {
    pub id: LaneId,
    pub max_concurrent: usize,     // 1 = strict FIFO
}
```

| Lane | `max_concurrent` | Why |
|---|---|---|
| `workspaces` | 1 | Two launch scripts touching shared state corrupt it. |
| `index` | 1 | Concurrent index rebuilds race. |
| `notify` | 2 | Must never be starved by long fetches — fallbacks depend on it. |
| `fetchers` | 8 | Network-bound and slow. Also rate-limited per-host by the gateway. |
| `default` | 4 | Anything that declares no lane. |

All values are defaults, overridable in `config.toml`. Modules declare lanes via
`lanes()`; the queue never hardcodes a module name.

### 6.2 Ordering and priority
 
- **Reordering is within a lane only.** Moving a fetch ahead of a workspace launch is
  meaningless — they do not compete. The queue UI groups by lane.
- **The currently running task cannot be reordered or displaced.** It can be cancelled.
- **`Scheduled` and `Normal` share one arrival-order queue.** A task's priority tier
  records *where it came from* (the scheduler vs. a user or module), not a rank — a
  scheduled task does not automatically run ahead of a manual one, or vice versa. Both
  join the lane in the order they were enqueued. The only way to run ahead of anything is
  `Overridden`, and that always goes through confirmation. See ADR 0003.
- **Promotion is explicit and confirmed.** Requesting `Overridden` returns
  `confirmation_required` with the list of tasks that would be displaced; the client shows
  that to the user and re-sends with confirmation. Never silent. Always audited in the
  event log via `queue.task.promoted`.

### 6.3 Scheduler triggers

```rust
pub struct TriggerSpec {
    pub id: TriggerId,
    pub schedule: Schedule,        // Cron(String) | Every(Duration) | Once(DateTime<Utc>)
    pub catch_up: CatchUp,
    pub op: String,                // enqueued when the trigger fires
    pub params: Value,
    pub lane: Option<LaneId>,
    pub fallback: Option<Fallback>,
}

pub enum CatchUp {
    Skip,       // missed firings are dropped. Polls, health checks.
    RunOnce,    // fire once on wake regardless of how many were missed.
    Backfill,   // fire once per missed period, with the correct historical timestamp.
}
```

`catch_up` is the question that matters on a laptop: the machine slept Friday to Monday —
does the 09:00 job fire three times, once, or not at all? Getting it wrong is how you send
three days of duplicate emails. Every `TriggerSpec` must state it; there is no default.

The scheduler reads time through `ctx.clock`, never `Utc::now()`, so the whole thing is
testable by advancing a fake clock rather than sleeping. It persists last-run and next-due
to the store, not memory.

A trigger never overlaps itself: if its previous task is still queued or running when it
fires again, the new firing is dropped and logged as `scheduler.trigger.skipped`.

---

## 7. Data layout on disk

`$SHIMMER_HOME`, defaulting to the XDG data dir (`~/.local/share/shimmer`) or
`~/Library/Application Support/shimmer` on macOS. Overridable by env var for tests.

```
$SHIMMER_HOME/
  config.toml              Global settings: lane overrides, local_timezone (ADR 0009).
  data/<module>/           Per-module namespace. A module sees only its own.
  data/records/collections/*.toml  Record collection definitions (§8, ADR 0008).
  data/records/items/<collection>/<id>.toml  One file per record.
  events/YYYY-MM-DD.jsonl  Append-only event log. One JSON Event per line.
  data/workspaces/<name>/  workspace.toml + steps/ + cleanup script + state.toml (ADRs 0010, 0012).
  notifications/failed.jsonl  Deliveries that exhausted every sink (§11.3).
  packs/<name>/            Command packs: aliases + animations (§15.1). Client-read only.
  .staging/<txid>/         In-flight store transactions (§7.1). Never edit by hand.
  cache/http/<module>/     Gateway response cache (ADR 0027), per module. Safe to delete.
  index.sqlite             DERIVED. Gitignored. Safe to delete.
  logs/                    daemon.log.<date>: rotated daily at UTC midnight, oldest deleted past
                           8 files (~a week), never archived elsewhere. A pre-rotation
                           daemon.log with no date suffix may be left over from before this
                           scheme and is safe to delete. Also holds workspace step logs (§10.2),
                           pruned to the newest 10 per workspace after each supervised step,
                           never deleting the one a `dirty` state's `log` still points at.
```

**SQLite is an index, never a source of truth.** A test asserts this directly: delete the
index, reindex, compare query results. If that test is hard to write, the design drifted.

Ship a `.gitignore` in `$SHIMMER_HOME` covering `index.sqlite`, `cache/`, `logs/` and
`.staging/` so the directory is committable as-is. That is the entire machine-to-machine
transfer story.

Client settings are not in `$SHIMMER_HOME`: the CLI keeps its own (the active command pack,
§15.1) in `~/.config/shimmer/cli.toml`, which only the CLI reads and writes (ADR 0013).

### 7.1 Atomicity: all or nothing

Every mutation either happens completely or not at all. There is no state in which a
record file was rewritten but its event was never logged, or half a TOML file is on disk
because the laptop lost power mid-write.

- **Single file writes** are write-to-temp in the same directory, `fsync`, then `rename`.
  Never write in place. A reader sees the old file or the new file, never a torn one.
- **Multi-file mutations go through a store transaction.** `ctx.store.transaction(|tx| …)`
  stages every write under `$SHIMMER_HOME/.staging/<txid>/`, fsyncs, writes a `COMMITTED`
  marker, then moves files into place and appends the event. The event id is assigned at
  staging time.
- **Recovery on daemon start:** a staging directory with a `COMMITTED` marker is rolled
  forward — finish the moves, append the event if its id is not already in the log. One
  without the marker is deleted. Roll-forward is idempotent, so a crash during recovery is
  also safe.
- **The index uses SQLite transactions.** An index update for one event commits entirely
  or not at all; a crash mid-reindex leaves the previous consistent index, and the rebuild
  simply runs again.
- **The atomic unit is a transaction, not a task.** A queued task that commits two
  transactions and fails on the third keeps the first two. Modules must make each
  transaction a meaningful whole — "update the record *and* its deadline" is one
  transaction, not two.
- **Process side effects cannot be rolled back.** A launched editor cannot be un-launched.
  That is exactly why workspaces have the `dirty` state (§10.3): where atomicity is
  impossible, we make the partial state explicit and block on it instead of pretending.

---

## 8. The general-before-specific rule

Three mechanisms are the same pattern: a host module owning the infrastructure, plus a
small trait implemented per specific thing.

| Host module | Owns (written once) | Trait | Implementations |
|---|---|---|---|
| `records` | schema, storage, filtering, completion metrics | `Collection` | leetcode, job apps, OSS repos, outreach |
| `fetchers` | scheduling, dedup | `Source` | job boards, HN, docs sites, transcripts |
| `notify` | subscription, batching, retry, quiet hours | `Sink` | email, push, desktop, webhook |

Rate limits and caching for outbound HTTP live in `ctx.http` itself (ADR 0027), not in
`fetchers` — every module that reaches the network shares the one gateway, so this is
written once at a layer below any specific host module, not per-module infrastructure.

**Adding a job board must not mean adding a module.** It means one `impl Source` of about
forty lines, registered in that module's registry.

For `records`, go further: LeetCode and job applications differ only in field names and
views, which is data, not logic. They are TOML files, not Rust:

```toml
[collection]
id = "leetcode"
label = "LeetCode"

[[field]]
name = "difficulty"
type = "enum"
values = ["easy", "medium", "hard"]

[[field]]
name = "last_solved"
type = "date"

[[view]]
kind = "heatmap"
source = "last_solved"
```

This gives user-defined trackers before we have a plugin system, and the table and heatmap
code is written exactly once. Leave an optional Rust hook for genuinely custom behaviour
(spaced-repetition review scheduling, say) but do not build it until something needs it.

---

## 9. Client / daemon contract

Full specification in `docs/protocol.md`. Summary:

- Newline-delimited JSON over a Unix domain socket. Debuggable with `nc -U`.
- Request/response **plus** a subscription stream. Both directions, from day one.
- Versioned handshake. Refuse mismatched major versions with a clear error.
- `core.manifest` returns every registered module, its commands (with `execution`), and
  its lanes. **Clients discover capabilities at runtime and must not hardcode module
  knowledge.** This is what makes the TUI work with modules that did not exist when it was
  written.

The subscription stream is why this is not a polling app: when a fetch finishes at 3pm, an
open dashboard updates because the daemon pushed.

---

## 10. Workspaces

### 10.1 Script ABI

User launch scripts are a permanent public interface. Whatever we pass on day one, we are
stuck with. Treat additions as breaking changes.

Scripts live in `$SHIMMER_HOME/data/workspaces/<name>/` (ADR 0010): an ordered list of numbered
launch steps, `steps/<index>-<name>.sh` on Linux/macOS or `steps/<index>-<name>.ps1` on
Windows (e.g. `steps/01-setup.sh`, `steps/02-editor.sh`) — each its own script with its own
spawn mode (§10.2) — plus a single `cleanup.sh` / `cleanup.ps1`, unchanged from before ADR
0010's §2a amendment. The daemon injects:

| Variable | Meaning |
|---|---|
| `SHIMMER_WORKSPACE_ID` | Workspace identifier. |
| `SHIMMER_WORKSPACE_DIR` | That workspace's own directory. |
| `SHIMMER_HOME` | Root data directory. |
| `SHIMMER_SOCKET` | Daemon socket path. |
| `SHIMMER_SESSION_ID` | Unique per launch, for correlating events. |
| `SHIMMER_PLATFORM` | `linux` \| `macos` \| `windows`. |

`SHIMMER_SOCKET` matters most: a user's Hyprland script can run
`shimmer records complete leetcode/two-sum` and participate in the platform rather than being
a dead-end launcher.

`workspace.toml` lists the steps in order and says how to run each one (ADR 0012). The id
is the folder name; step *N* must be `steps/<NN>-<name>.sh` (or `.ps1`), and any mismatch
between the list and the files is an error:

```toml
[workspace]
label = "Deep Work"

[[step]]
name = "setup"            # steps/01-setup.sh: waited on; failure or timeout → dirty
mode = "supervised"
timeout_s = 60

[[step]]
name = "editor"           # steps/02-editor.sh: started, outlives the daemon
mode = "detached"

[cleanup]                 # required exactly when cleanup.sh exists
timeout_s = 30

[env]                     # extra variables for every script; SHIMMER_* is reserved
PROJECT_DIR = "/home/me/code/shimmer"
```

`mode` has no default. Unknown keys are rejected. Steps run one at a time and the first
failure stops the launch; nothing is retried (§11.2). ADR 0012 has every rule and check.

Scripts run through their interpreter (`sh steps/01-setup.sh`, `powershell -File
steps/01-setup.ps1`), never via the executable bit — an atomic write (§7.1) does not reliably
preserve `chmod +x`, and a user editing a script by hand should never have to remember to
re-set it.

### 10.2 Spawn modes

Every launch step declares one. Confusing them is how we get orphaned processes.

- **`detached`** — editors, browsers, terminals. Must outlive the daemon. Spawned into its
  own process group on Unix (`DETACHED_PROCESS` on Windows), not a new *session* — `setsid`
  needs a `pre_exec` closure between `fork` and `exec`, which is `unsafe`, and §12 rule 8
  bans `unsafe` outright (ADR 0010 §3 amendment, #69). No handle retained.
- **`supervised`** — setup scripts whose exit code matters. Handle retained, timeout
  enforced, **kill the whole process group** on timeout, stdout/stderr captured to `logs/`.

### 10.3 State machine

```
ready ──activate──> launching ──all steps ok──> active ──stop (cleanup script)──> ready
                        │
                        └──step fails──> dirty
                                          │
                        ┌─────────────────┼─────────────────┐
                        │                 │                 │
                  cleanup script    force relaunch      stays dirty,
                  succeeds          (explicit)          surfaced in UI
                        │                 │
                        ▼                 ▼
                     ready            active (logged as forced)
```

- **`active` means "launch succeeded", nothing more.** There is no liveness claim and no
  health sweep. If the user closes a detached terminal we do not know and do not pretend
  to. Revisit only if this causes a real problem.
- **`dirty` blocks normal activation outright.** `workspaces.activate` on a dirty
  workspace returns `workspace_dirty` with the failed step and log path in `detail`. It is
  an error, not a warning the user can click through by accident. A half-configured
  workspace must never be launched into.
- **A failed workspace task does not block the lane** (§11.1). It is removed, logged, and
  the next queued task starts. The workspace is flagged `dirty` and the user is notified at
  elevated priority — an unattended dirty workspace is something you want to know about
  before walking away from the machine.
- **Recovery is by cleanup script.** `cleanup.sh` runs `supervised`, same env vars. Success
  moves `dirty → ready`. If there is no cleanup script, `dirty` clears only via explicit
  force relaunch or `workspaces.reset`. The same script is what `workspaces.stop` runs on an
  `active` workspace (`active → ready`): it stops the processes the workspace started, never
  the person's own windows (ADR 0026).
- **Force relaunch is loud.** A distinct op, not a casual flag. Emits
  `workspaces.session.forced` with the prior dirty reason attached, so if it breaks
  something worse there is a trail.

### 10.4 Out of scope

Locked-in mode (restricting access to distracting applications) needs elevated privilege —
hosts file or nftables on Linux, a privileged helper on macOS. When it is built it goes in
a **separate binary** with a deliberately tiny command surface (`block(list)`,
`unblock()`), which the unprivileged daemon asks. It must never live in the daemon.

---

## 11. Failure, retry and fallback

### 11.1 A failed task never blocks its lane

Uniform across all lanes. No per-lane failure policy.

```rust
pub enum TaskOutcome {
    Succeeded,
    Failed { error: String, retryable: bool },
    Cancelled,
}
```

On `Failed`, the queue:

1. Frees the lane slot **immediately** and starts the next queued task.
2. Emits `queue.task.failed` with task id, lane, origin, error and attempt count.
3. Writes the outcome to the event log. A failed fetch is exactly as much "what happened"
   as a successful one and belongs in the history the heatmap is built from.

No task is ever the reason a lane stalls.

### 11.2 The queue does not retry

Retry is the module's decision, made with `ctx.retry_with_backoff` so five modules do not
invent five slightly different curves. The queue's job is to run what is in front of it.

The reason this is not a shared default: a fetcher wants exponential backoff up to three
attempts; a workspace launch should not auto-retry at all, because re-running a
half-finished setup script is actively harmful. A queue that retries would have to know
which is which.

### 11.3 Degrade, don't drop
 
A scheduled task may carry a fallback, **set at trigger-creation time, not invented at
failure time**:
 
```rust
pub enum Fallback {
    /// `fallback_op` must be a `notify.*` op (e.g. selecting a sink or template).
    /// Any other namespace is rejected at `scheduler.add` time, not at failure time.
    /// A `notify.*` op MUST NOT itself declare a `fallback` — no nested fallbacks.
    Notify { fallback_op: String, params: Value, priority: NotifyPriority },
}
```
 
Fallback is deliberately not "run any op on failure." A fully generic fallback can itself
fail, which reopens the question of what *its* fallback is. Constraining the target to
`notify.*` — a namespace whose own failure path already terminates in
`notifications/failed.jsonl` (§11.3) rather than another fallback — closes that regress
instead of deferring it. See ADR 0005.
 
If the primary task fails for a structural reason (`workspace_dirty`, `unavailable`), the
queue additionally enqueues the fallback in the `notify` lane. So a calendar reminder that
was meant to open a workspace still reaches you as "your 2pm session couldn't launch —
dirty, see logs" instead of silence.
 
`notify` has its own fallback: each `Sink` declares an optional `fallback_sink_id`, tried
once before giving up. Desktop notification is the sane universal last resort — no network,
no external account. Deliveries that exhaust every sink append to
`notifications/failed.jsonl` rather than vanishing.
 
---

## 12. Agent rules

Read these before acting on any task in this repo.

1. Do not modify `crates/core` or `crates/proto` unless explicitly told to. Report instead.
2. Do not add a dependency between two modules. Use events.
3. Do not add a dependency from `cli` or `tui` onto `store`, `daemon` or `modules/*`.
4. Do not bypass `Ctx`. No `std::fs` in a module, no direct `reqwest`, no `Utc::now()`.
5. Do not add identity, auth, user or session types to `core` (§1.5). Stop and report.
6. Do not make the queue retry (§11.2), and do not add a per-lane failure policy (§11.1).
7. Do not hardcode a module name in the queue or scheduler. Lanes and triggers are
   declared by modules.
8. Do not use `unsafe`. Do not use dynamic libraries for plugins — Rust has no stable ABI,
   and a `cdylib` compiled with a different compiler version is undefined behaviour, not an
   error. External plugins will be subprocesses over JSON-RPC.
9. Do not invent `ViewSpec` or any rendering abstraction in `core` (§5).
10. Keep domain logic synchronous. `async` belongs at the I/O edges. A state transition
    should be a plain function over plain data a test can call without a runtime.
11. Long-running ops must poll `ctx.cancel` and report `ctx.progress`. A task that cannot
    be cancelled is a bug.
12. Before declaring a task done: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`,
    `cargo test --workspace`. All three must pass.
13. Every new module, `Collection`, `Source` or `Sink` gets a test, against an in-memory
    `Ctx`, not a real filesystem.
14. When a design question arises that this file does not answer, write an ADR in
    `docs/decisions/` rather than deciding silently in code.
15. Never write a file in place, and never mutate more than one file outside
    `ctx.store.transaction` (§7.1). Every mutation is all or nothing.
16. Never put an alias, theme name or animation reference in the daemon, `proto`, a
    module, or an error message. Canonical names only; packs live in the client (§15.1).
17. Commit your work before ending a session. Uncommitted changes are invisible to the
    next session and easy to lose.

---

## 13. Ownership

| Dev | Owns | Must not touch |
|---|---|---|
| A | `daemon/` (registry, bus, queue, scheduler, gateway, IPC, indexer), `store/`, `mockd/`, `app/` ² | `tui/` |
| B | `modules/*`, `cli/` | `tui/` |
| C | `tui/` | everything outside `crates/tui` ¹ |

¹ Exception: Dev C may contribute fixture cases to `crates/mockd/fixtures` by ordinary PR.
Dev A owns the crate.

² `crates/app` had no owner (flagged in ADR 0007); Dev A reviews it. Dev B may still add a
module-registration line in `run_daemon` (e.g. `Arc::new(shimmer_records::Records::default())`)
without Dev A review — anything else in `app/` needs Dev A.

Dev C works against `docs/protocol.md` and a **mock daemon** (`shimmer mockd`, built from
`crates/mockd`) serving canned responses from `crates/mockd/fixtures` and replaying a
scripted event stream. Build it at M1. It is the thing that actually decouples the TUI
work; without it Dev C is blocked on Dev A daily. Dev A owns the crate and its fixtures;
Dev C adds fixture cases by ordinary PR, as any contributor to a crate they don't own.

`core/` and `proto/` are shared and change-controlled (§4).

---

## 14. Milestones

CLI-first. The GUI is not in scope for any milestone below.

- **M0 — skeleton.** Workspace compiles. `core` and `proto` types exist. Daemon accepts a
  connection, answers `core.ping` and `core.manifest`. CLI round-trips it. CI runs fmt,
  clippy, test. *Done when a `shimmer` command reaches the daemon and back.*
- **M1 — records vertical slice.** Store (files + index). `records` with the LeetCode
  collection TOML. CLI add, list, complete, filter. Event log written. Mock daemon fixture
  for Dev C. *Done when a completed item survives a daemon restart.*
- **M2 — queue and scheduler.** Lanes with per-lane concurrency. Priority, promotion with
  confirmation, reordering, cancellation. Triggers with all three `catch_up` policies.
  Failure policy and `ctx.retry_with_backoff`. Subscription stream live over IPC.
  *Done when a fake clock advanced three days produces the right number of firings.*
- **M3 — workspaces.** Manifests, script ABI, both spawn modes, the state machine
  including `dirty` and cleanup, `SHIMMER_SOCKET` callback. Tested on a real Hyprland/Arch
  setup and on macOS.
- **M4 — index rebuild + heatmap.** Rebuild-from-files test passing. Heatmap query over
  the event log.
- **M5 — TUI usable.** Dev C's client against the real daemon. Extract `ViewSpec` from
  what was actually built.
- **M6 — fetchers, notify, calendar.** One `Source` (a job board), one `Sink` (email) with
  a desktop fallback, calendar entries registering triggers. The §1.3 chain end to end.

---

## 15. Command naming

Commands have a **canonical name** and, when a command pack is active, an **alias**. Both
work.

- Canonical names are used everywhere internal: IPC ops, `commands()`, logs, docs, tests,
  error messages. Boring on purpose — `records.list`, not `mite`.
- Aliases are a **client presentation concern only**, supplied by a **command pack**.
  Nothing in the daemon, the protocol, or any module knows they exist.

Keeping the theme out of the protocol is what lets us rename on a whim without touching the
daemon, and keeps stack traces readable at 2am.

### 15.1 Command packs

A pack is a themed set of aliases, plus optional animations. Packs are how the theme becomes
pluggable. ADR 0013 has every rule and check.

```
$SHIMMER_HOME/packs/<name>/
  pack.toml        Aliases and metadata.
  anim/            Optional animation frames (plain text / ANSI).
```

```toml
[pack]
id = "anime-tropes"
label = "Anime Tropes"

[alias]
"ikuzo"  = "workspaces.activate"
"nani"   = "workspaces.status"
"yatta"  = "records.complete"

[anim]
"workspaces.activate" = "anim/ikuzo.txt"
```

Rules:

- **Packs are data, never code.** TOML plus text assets. No scripts, no dylibs (§12 rule 8).
  A pack cannot add an op, only a name for an existing canonical one.
- **No pack is active by default.** A fresh install uses canonical names only. Canonical names
  always work, whichever pack is active.
- **One active pack**, chosen with `shimmer packs use <name>` and stored in the client's own
  `~/.config/shimmer/cli.toml` (§7), never in `config.toml`. `--pack <name>` uses one for a
  single command.
- **Built-in packs ship embedded in the binary and are unbranded**: `anime-tropes`, `ship-it`,
  `starship`, `short`. No built-in pack uses a name from another company's game, show,
  film or brand. Users add their own as folders in `packs/`; a folder replaces a built-in of the
  same id.
- **Packs are checked at load time.** An alias that is also a canonical command word, points at
  a command that doesn't exist, or breaks the charset, and an animation path that leaves the
  pack folder, all fail the pack with a clear error. Every built-in pack is checked in a test.
- **A broken pack never breaks a command.** The CLI prints one warning and runs with canonical
  names.
- **An alias replaces `shimmer` and the whole command**: `ikuzo deep-work` runs
  `shimmer workspaces activate deep-work`. `shimmer packs use` makes this work by linking each
  alias to the binary in `~/.local/bin` (`--no-link` to skip). Linking never overwrites a file
  or shadows a program, and only removes links it made.
- **Animations never gate execution.** The daemon request is sent immediately; the
  animation plays alongside or after. Animations are disabled automatically when stdout
  is not a TTY, and by `--no-anim` — a workspace script calling `shimmer` through
  `SHIMMER_SOCKET` must never sit waiting on a cutscene.
- Clients read `packs/` directly and **read-only**. It is presentation config, like a
  terminal theme; the daemon never loads it.

| Built-in pack | Sample aliases |
|---|---|
| `anime-tropes` | `ohayo` (ping), `ikuzo` (workspaces activate), `yatta` (records complete) |
| `ship-it` | `on-call` (ping), `deploy` (workspaces activate), `lgtm` (records complete) |
| `starship` | `comms` (ping), `engage` (workspaces activate), `landed` (records complete) |
| `short` | `up` (ping), `wgo` (workspaces activate), `rdone` (records complete) |

`shimmer packs list` shows every pack; `shimmer packs show <name>` its aliases;
`shimmer --help` shows the active pack's aliases and `shimmer --help --canonical` the real
names.
