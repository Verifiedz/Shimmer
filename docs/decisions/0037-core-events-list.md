# 0037. `core.events.list`: reading an event's history over IPC

Status: proposed · Raised by Dev A · Needs sign-off: Dev A **and** Dev B (changes
`crates/proto`, CLAUDE.md §4) · Dev C notified (one new op; additive, no version bump)

## Context

This was built inside the `fetchers` branch as ADR 0028 §12, to back `shimmer fetchers items`.
#126's re-review was right that it doesn't belong there: `crates/proto` is change-controlled
(CLAUDE.md §4), and "read an event history" is not fetchers' idea — `records`, `workspaces`,
`notify` and `automations` all emit events with the identical shape, and none of them has a way
to read their own history back either. Carrying a proto change inside a module's umbrella PR
also meant it never got the separate review and the Dev C notification §4 asks for.

So it is split out here, on its own, against `master`. The `shimmer fetchers items` command that
first motivated it stays with `fetchers` — it is a client of this op, not part of it.

## Decision

### 1. One op, module-agnostic

```
core.events.list   {"topic"?, "limit"?}  ->  {"events": [Event, ...]}
```

- **Inline** (CLAUDE.md §5's `Execution::Inline`): a bounded index query, no I/O beyond the
  store, so it must not go through the queue.
- `topic` is a subscription-style pattern — `a.b.c`, `a.*`, `*` — validated with
  `crates/proto`'s own `is_valid_pattern`, the **same grammar a live subscription already
  uses**, so "what can I subscribe to" and "what can I read back" never drift apart. Anything
  else is `invalid_params`. Default `*`.
- `limit` defaults to 100 and is clamped to 1000 (`DEFAULT_EVENTS_LIMIT` /
  `MAX_EVENTS_LIMIT`). The cap exists because the index holds a single connection behind a
  `Mutex`: one client must not be able to hold it for an unbounded scan.
- Events come back **oldest first**, which is the event log's own order. A client that wants
  newest-first (a feed, a log tail) reverses it, and `shimmer fetchers items` does exactly that
  — the op keeps the log's ordering rather than baking one display choice into the protocol.

### 2. Answered from the index, never a log scan

`crates/store`'s new `events_matching_recent` is a sibling of the existing `events_matching`:
the existing one answers "the oldest N matching", which is not what a history view wants.
Backed by the derived SQLite index the indexer task already keeps current (CLAUDE.md §1.4), so
it stays fast however much history has piled up, and nothing is read that isn't also in the
event log files.

`Core` now holds the shared index handle (`SharedIndex`) — opened before `Core::new` rather
than only after it — which is the one structural change in the daemon.

### 3. A latent ordering bug this surfaced, fixed in `store`

`events_matching` ordered by the event's `id`, assuming a ULID's sort order reflects insertion
order. That is only true *between* milliseconds: `Ulid::from_datetime` fills the remainder with
random bits, so two events committed inside the same millisecond — a real shape, since one
`fetchers` run emits every item in a single transaction — could come back in either order.
Caught by this change's own test coming back shuffled. Both methods now order by SQLite's
`rowid`, which always reflects insertion order. `events_matching`'s existing behaviour and tests
are unaffected (nothing depended on same-millisecond tie order).

### 4. Why no new capability or permission gate

`core.events.list` exposes every module's event history to any local client. That is the same
posture the live subscription stream already has (`docs/protocol.md`), and the same posture the
event log files on disk already have — anything that can reach the socket can already read
`events/*.jsonl` directly. So this op widens *convenience*, not reach.

It does make one existing rule load-bearing rather than theoretical: **nothing secret may ever
enter an event payload.** That was already true (CLAUDE.md §1.5, and ADR 0036 keeps credential
values out of events entirely), but it is now the property that keeps this op safe, so it is
written down here as a constraint on every future module rather than left implicit.

A real permission model for local clients — which matters once an MCP/AI layer can call ops on
the user's behalf — is #228's, not this op's. When it lands, this op is one of the reads it will
want to gate.

## Protocol change

Additive: a new op and its response struct (`EventsListData`). `Event` itself is unchanged and
already shared and serializable. **No version bump** — `docs/protocol.md`'s own rule is that
`v` moves only for a breaking change, and a client that never asked for `core.events.list`
before sees no difference.

## Not done here

- **`shimmer fetchers items`**, and any other client of this op. They belong with their own
  module.
- **Per-module history ops** (`records.events`, etc.). One module-agnostic op is the point;
  a module that wants a narrower view passes a `topic` pattern.
- **Filtering on payload contents.** `topic` and `limit` only. A caller wanting "items from
  source X" filters client-side today, which #126's review correctly flags as logic in the
  client — but the fix for that is a daemon-side filter designed with its first real consumer,
  not a speculative `where` clause on this op now.
- **Pagination.** `limit` plus a cap, no cursor. Add one when something needs to walk more than
  1000 events, with a real cursor rather than an offset.
