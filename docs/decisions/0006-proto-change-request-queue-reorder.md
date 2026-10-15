# 0006. Proto change request: `queue.reorder` op constant

Status: accepted (M2, #46; landed on master in #45) · Signed off: Dev A, Dev B · Dev C to be
notified (CLAUDE.md §4)

Raised by Dev A under CLAUDE.md §12 rule 1: a `crates/proto` gap is reported, not worked
around. No stopgap has been landed in the daemon.

## What is missing

`docs/protocol.md` already specifies the op (queue ops table):

| Op | Execution | Params | Returns |
|---|---|---|---|
| `queue.reorder` | inline | `{"lane","task_id","before":TaskId\|null,"queue_version"}` | New `queue_version`. |

with a stale `queue_version` returning `conflict`. `crates/proto` has the constants for the
other three queue ops but not this one:

```rust
// crates/proto/src/lib.rs, `pub mod ops`
pub const QUEUE_LIST: &str = "queue.list";
pub const QUEUE_TASK: &str = "queue.task";
pub const QUEUE_CANCEL: &str = "queue.cancel";
// missing:
pub const QUEUE_REORDER: &str = "queue.reorder";
```

## Requested change

1. **`crates/proto/src/lib.rs`**: add `pub const QUEUE_REORDER: &str = "queue.reorder";` to
   `ops`, next to `QUEUE_CANCEL`. No new types: the params are read with a daemon-local struct
   exactly as `queue.list` and `queue.cancel` are today.
2. **`docs/protocol.md`** (change-controlled): add a row to the topic catalogue for the event
   the op emits, which the catalogue lacks:

   | `queue.task.reordered` | A waiting task moved within its lane. Payload `{task_id, lane, before, queue_version}`. |

   No change to the reorder op's own row, which is already correct.

## Why the op needs it

`Core::handle_request` routes on `ops::*` constants (`crates/daemon/src/core.rs`). Every
op the daemon answers by name is named once, in `proto`, and a client that wants to send it
imports that same constant, so the daemon and the TUI cannot disagree on the spelling.
Landing `"queue.reorder"` as a string literal in the daemon would make it the one wire name
that lives outside the shared contract, and Dev C would have to guess whether to copy it or
wait. The doc already promises the op, so the constant is the only piece missing.

## Compatibility

Additive: one constant and one documented event topic. No frame shape changes and no removed or
renamed field, so **no protocol version bump** is needed (`PROTOCOL_VERSION` stays 1; clients
must already tolerate unknown topics per the topic catalogue's own rule). Dev C still gets
notified per §4, since the TUI will want to bind reorder to a key and subscribe to the new topic.

## Daemon side, already done and waiting

The ordering logic is implemented and tested, but deliberately not reachable from the wire:
`LaneState::reorder` in `crates/daemon/src/queue/state.rs`. It moves only waiting tasks (the
running task cannot be reordered, §6.2), keeps the promoted block ahead of ordinary tasks so
reorder cannot become an unaudited promotion, bumps `queue_version` only when the order
changes, and leaves state untouched on error.

Once `QUEUE_REORDER` exists, the remaining work is a handler of roughly fifteen lines in
`Core::handle_request`: parse params, compare `queue_version` (stale → `conflict`), call
`reorder`, emit `queue.task.reordered`, return the new version. It comes with an integration
test for the version check and for "the running task cannot be reordered".
