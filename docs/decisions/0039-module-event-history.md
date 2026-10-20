# 0039. Can a module read its own event history, not just emit?

Status: proposed · Raised by Dev B · Needs sign-off: Dev A (changes `core`, possibly `daemon`)
· Dev C notified if decided yes (a new `Ctx` field) · Open for wider discussion before a
decision is made — this document lays out the question, not a settled answer.

## Context

ADR 0028 §9 (the `fetchers` module rework) lists `fetchers.items` as a planned op: "answers
what the CLI's `fetchers items` used to compute itself... matches by source and reuses the
same title-filter rule `fetchers.run` already applies, in one place." Implementing it hit a
real wall: `Ctx` gives a module `bus: Emitter`, which is **emit-only**
("Emit-only handle onto the event bus" — CLAUDE.md §5). There is no way for a module's own
code, running inside `handle`/`on_event`, to read back events — its own or anyone else's —
that already happened.

This is not an oversight; it is the same shape the problem already took once. ADR 0037
(`core.events.list`) exists because `fetchers items`' first draft tried to read event history
from inside the `fetchers` **module**, and #126's re-review correctly pointed out that "read an
event history" isn't fetchers' idea, is not module-specific, and belongs in `crates/proto` —
change-controlled — rather than smuggled in under one module's PR. ADR 0037 solved that for
**clients**: `core.events.list` is a daemon-level, module-agnostic IPC op any client (the CLI,
eventually the TUI) can call. It explicitly does not touch `Ctx` — see its own "Not done here":
"Per-module history ops... one module-agnostic op is the point."

`fetchers.items` as ADR 0028 describes it — the computation moving server-side, so "the CLI
command becomes a thin call-and-render, like every other inline op" — needs the **module**,
not just the client, to be able to ask "what happened on topic X, recently". `core.events.list`
doesn't give a module that; it gives a *client* that, over the socket, and a module has no way
to call another daemon op on itself.

`automations` hit an adjacent need earlier (`automations.history`, `shimmer automations
history`) and solved it the other way: it keeps its own file, `data/automations/history.json`,
written at the moment each rule fires, oldest dropped once the file is capped. It never reads
the shared event log back. That is the only precedent in this codebase for "a module wants to
show its own past", and it is a self-persisted copy, not a read-back capability.

So there are two different-shaped answers already live in this codebase for "how does
something show history": clients get a real query (ADR 0037); the one module that needed this
before (`automations`) keeps its own copy. `fetchers.items` is the first time a *module itself*
plausibly wants the former.

## The question

Should a module be able to read back the event log (its own topics, or any), or should every
module that wants "show me what happened" keep its own capped copy, the way `automations`
already does?

## Options considered

### A. `fetchers` keeps its own capped items log (follows the `automations.history` precedent)

A new file under `data/fetchers/` (shape and retention undecided — e.g. last N per source, or
last 90 days), written in the same transaction `run_source` already uses to emit
`fetchers.item.found`. `fetchers.items` reads only this file. No `core` change at all.

- **For:** zero blast radius; proven precedent; ships inside the `fetchers` PR with no
  cross-team sign-off. Keeps modules strictly decoupled from the event-log's own storage
  details (CLAUDE.md §1.4 already treats the log as something the *daemon* owns and projects
  from, not something modules reach into).
- **Against:** duplicates data that already exists, verbatim, in the event log files — a
  second on-disk copy of the same facts, with its own retention bug surface, repeated per
  module. If `records`, `workspaces`, or a future module wants the same "recent activity"
  shape, each one reinvents it.

### B. A read-only `Ctx` capability: modules can query the event log, not just emit to it

Something like `ctx.events.query(topic, limit) -> Vec<Event>`, backed by the same
`events_matching_recent`/`SharedIndex` machinery ADR 0037 already built for
`core.events.list` — reusing the index, not re-implementing a query path.

- **For:** one implementation instead of N; it closes a real asymmetry (the event log is the
  system's one history mechanism per CLAUDE.md §1.4, "files are truth, the event log gives
  you the past" — right now only clients, not the code running inside the daemon, can ask it
  anything). Reusable for whatever the next module's version of this need turns out to be.
- **Against:** it is a capability every module gets, present and future — the `Ctx` doc
  comment calls adding a field "a security decision, not a convenience," and this is exactly
  that kind of decision, not a one-off. Needs its own design questions answered, not inherited
  from ADR 0037 by default:
  - **Scope**: any topic (including another module's), or only topics the calling module
    itself emits? Modules already observe each other's *live* events via `on_event`, so
    reading another module's *past* events is arguably the same coupling, not new — but it
    should be decided on purpose, not by default.
  - **Gating**: unconditional on every module (like `bus`/`clock`), or behind a declared
    manifest capability the way `"network"`/`"process"`/`"desktop"` already gate `http`/
    `launcher`/`desktop`? The existing capabilities gate *reaching out* (network, processes,
    the desktop); this would be the first one gating *reaching back in time*, which is a
    different kind of risk and may not fit the same on/off pattern.
  - **The ADR 0037 §4 constraint becomes load-bearing from a second direction.** "Nothing
    secret may ever enter an event payload" is already a rule (CLAUDE.md §1.5, ADR 0036); this
    option means a module's own code — not just a client with socket access — now relies on it
    holding, so it's worth re-stating rather than inheriting silently.

### C. Don't move `fetchers.items` server-side; leave it as the CLI's own client-side query

`shimmer fetchers items` keeps calling `core.events.list` itself and filtering client-side,
exactly as it does today. No `core` change, no new module storage either.

- **For:** genuinely zero new work; it already works, has tests, and nothing is broken by
  leaving it alone.
- **Against:** leaves ADR 0028 §9's own description of this op unmet (it was written as an
  inline **daemon** op, matching every other op in that table), and the CLI keeps carrying
  logic ("match by source, apply the title-filter rule") that arguably belongs server-side,
  the same complaint issue item 6 originally raised about it.

## Not decided here

This document exists to put the question in front of the people who need to weigh in, not to
settle it. `fetchers.items` (ADR 0028's step 8) is paused on this — step 8's other half,
`fetchers.health`, has no such dependency and proceeds independently.
