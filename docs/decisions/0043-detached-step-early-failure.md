# 0043. A detached step that fails instantly should say so (#269 finding 7)

Status: proposed · Raised by: #269 finding 7 (dog-fooding/robustness audit) · Needs sign-off:
Dev A, Dev B (changes `core`: `crates/core/src/launcher.rs`, and the module's `failure()` in
`crates/modules/workspaces/src/launch.rs`) · No protocol change (additive field only, see
"Decision") · No version bump

## Context

`SpawnMode::Detached` is deliberately fire-and-forget. ADR 0010 §3 states it plainly: "`run`
returns as soon as the process is spawned — it does not wait for an exit code, matching 'no
handle retained' (§10.2)." ADR 0012 §5 restates the consequence for whoever writes a
`workspace.toml`: "A detached step succeeds as soon as it starts. Shimmer does not know if
the editor crashes a second later; `active` means 'the launch succeeded', nothing more... Put
anything whose success matters in a supervised step before the detached ones." CLAUDE.md
§10.2 encodes the same thing as a hard property: "No handle retained."

That design is right for its actual target — an editor, a terminal, a browser left running
for hours, where holding a handle buys nothing and the daemon has no business waiting on it.
But issue #269 (finding 7) found a gap it creates: a detached step whose command is simply
wrong — a typo in a launch line, a missing binary, the wrong Steam app id — exits in
milliseconds, and today that is indistinguishable from the editor that opened fine and is
still running an hour later. The launch reports `active`. Nothing is on screen. There is no
log, because `run_detached` sends stdout/stderr to `/dev/null` by design (ADR 0010 §3) and
never looks at the exit status at all.

This is not a hypothetical for Shimmer's own templates: `daily-dev` and `gaming` (both
`crates/modules/workspaces/templates/`) are almost entirely `Detached` steps — an editor, a
chat app, Steam, a game, now also a VM console (`VM_NAME`, added alongside this audit).
`GAME_COMMAND`'s own `help` text warns the Steam app id "from its store page's url" is
something to get right; today, getting it wrong fails silently.

**Why this needs an ADR instead of a same-commit fix (#269 finding 1 got one; this did not):**
the fix the issue proposes — "watch the first second. If the process exits non-zero within
it, report the step as failed" — means retaining a handle for part of a `Detached` step's
life, which is exactly what ADR 0010 §3 and CLAUDE.md §10.2 say never happens, and reporting
a real exit code for a step whose `StepOutcome.exit_code` is documented (ADR 0010 §2) as
"`None` for `Detached` (not waited on)". Both are decisions on record, not gaps in the
writing. CLAUDE.md §4 is explicit that changing what `core` guarantees needs Dev A and Dev
B's agreement and an ADR, not a workaround in the daemon alone — so this document is that
ADR, and the fix stays out of #269 finding 1's commit.

## Decision (proposed)

**A `Detached` step gets a brief, bounded startup watch — 1 second — during which (and only
during which) the backend keeps a handle and captures stderr.** If the process exits within
that window, its exit is reported as a failure. If it is still running when the window
closes, the backend lets it go exactly as today: no handle, no further observation, `/dev/null`
for stdout/stderr from that point on. The "no handle retained" property in CLAUDE.md §10.2
and ADR 0010 §3 is amended to **"no handle retained beyond a one-second startup watch, used
only to catch a process that fails within it"** — not removed.

### Why a new field, not a changed one

`StepOutcome.exit_code`'s doc comment ("`None` for `Detached`, not waited on") stays
literally true under this proposal — `exit_code` keeps meaning exactly what it means for a
`Supervised` step, and nothing new is layered onto it. Instead:

```rust
// core::launcher — additive change to StepOutcome
pub struct StepOutcome {
    pub session_id: String,
    pub exit_code: Option<i32>,   // unchanged: still None for Detached
    pub timed_out: bool,          // unchanged: still false for Detached
    pub log_path: String,
    /// `Some(reason)` only for a `Detached` step observed to exit — non-zero or by signal —
    /// within its one-second startup watch (ADR 0043). `None` either because the step is
    /// `Supervised` (its own fields above already cover failure) or because a `Detached`
    /// step outlived the watch window, overwhelmingly the common case, which reports exactly
    /// as it always has. `log_path` is populated alongside it with up to 64KB of the step's
    /// captured stderr, the same shape a `Supervised` failure's log already takes.
    pub early_exit: Option<String>,
}
```

`failure()` in `crates/modules/workspaces/src/launch.rs` (currently `let SpawnMode::Supervised
{ timeout } = mode else { return None };` — a `Detached` step can never fail, by
construction) gains one more case: a `Detached` step whose `StepOutcome.early_exit` is
`Some` fails with that reason, the same way a `Supervised` step fails on a non-zero
`exit_code`. Nothing about *how* a failure is handled once `failure()` returns `Some`
changes — the workspace still goes `dirty` with the step's `index`/`count`/`name` and
`log_path`, through the exact same path ADR 0010 §8 already describes ("flagging the
workspace `dirty` is the module's job, not the launcher's").

This keeps the change additive at the wire level too: `docs/protocol.md`'s `workspace_dirty`
detail shape is unaffected (an early-exit `Detached` failure is reported exactly like a
`Supervised` one already is — `failed_step`, `reason`, `log`), so no version bump.

### The cost: up to one second added per `Detached` step

Steps run one at a time, in order (ADR 0012 §5), so a workspace with several `Detached`
steps (`daily-dev`'s editor, monitors, Obsidian, chat, browser; `gaming`'s Steam, game, VM)
pays up to one second of added latency *per step*, serially, since `run` cannot return until
either the process exits or the watch window closes. A five-`Detached`-step workspace's
`activate` gets up to five seconds slower. This is the real trade-off finding 7 asks for:
catching instant failures costs launch speed for every workspace, including the overwhelming
majority of steps that are not failing.

Two ways to resolve this, for Dev A and Dev B to pick between — **this ADR does not pick
one**:

1. **Accept the latency (recommended).** One second per `Detached` step is not nothing, but
   it is a fixed, bounded, one-time cost per `activate`, not a recurring one, and it buys
   exactly the signal #269 asked for with no new machinery: `run` still returns a complete
   `StepOutcome` the module can act on immediately, through the interface that already
   exists. Simpler is worth a second.
2. **Report asynchronously instead.** Have `run` return immediately as it does today, and
   watch in a background task; if that task observes an early exit, emit an event (e.g.
   `workspaces.step.failed_after_start`) instead of returning it in `StepOutcome`. This keeps
   launches fast but is a materially bigger change: it needs a new event topic, a module
   handler that can retroactively flag an already-`active` workspace `dirty` (today nothing
   transitions `active → dirty` outside of `activate` itself — see ADR's workspace state
   machine, CLAUDE.md §10.3), and an answer for what happens if the workspace was stopped or
   relaunched in the interval between the detached step starting and the async report
   arriving. None of that exists today and all of it is new design, not a bounded addition.

## Consequences

- `RealLaunchBackend::run_detached` (`crates/daemon/src/launcher.rs`) changes from "spawn,
  `tokio::spawn` a reaper, return immediately" to "spawn, race `child.wait()` against a
  1-second `tokio::time::sleep`, and only then do the existing reaper handoff if the process
  is still running" — the reaping task itself is unchanged, just started later in the few
  cases where the watch doesn't already have the exit status in hand.
- Capturing up to 64KB of stderr during the watch means `run_detached` needs the same kind of
  bounded, non-blocking capture `run_supervised` already has for its own log (#269 finding 1,
  implemented without an ADR since it touched no `core` contract) — reusable in shape, not
  necessarily in code, since the cap and the write-once-at-the-end behavior both apply here
  too, just over a one-second window instead of a step's whole lifetime.
- `FakeLauncher` (`core::testing`) needs no interface change beyond what `StepOutcome`
  already gains — a test scripts `early_exit: Some("exit code 127".into())` on an otherwise
  normal `Detached` outcome, the same way it scripts `timed_out`/`exit_code` today.
- `docs/protocol.md` is unaffected (see "additive at the wire level" above).

## Not done here

- Which of the two latency-resolution options (above) to take. Flagged, not decided.
- The real implementation in `crates/daemon/src/launcher.rs` and
  `crates/modules/workspaces/src/launch.rs` — waits on this ADR's sign-off, same posture ADR
  0010 itself took for its own real `LaunchBackend`.
- Windows: `SpawnMode::Detached`'s Windows spawn (`DETACHED_PROCESS`) is sketched, not
  implemented, per ADR 0010 §7 — this amendment's watch window is platform-agnostic
  (`child.wait()` plus a timeout race), so it carries over once Windows support itself lands,
  but is not tested there now.
