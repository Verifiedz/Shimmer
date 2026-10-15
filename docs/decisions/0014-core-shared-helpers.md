# 0014. Shared helpers in `core`: params decoding and the module write lock

Status: accepted (M3, #73) · Raised by Dev B (issue #67, from Dev A's reviews of #55 and #56) ·
Signed off: Dev A (`core` is change-controlled, CLAUDE.md §4; also touches `crates/daemon`) · No
`proto` change, nothing user-visible

## Context

Reviewing the workspaces module (#55, #56), Dev A found the same small rules copied between
crates. Each copy is a few lines, but two copies of one rule drift apart silently: a fix applied to
one and forgotten in the other, with no compiler error or test to notice. Every module still to
come (fetchers, notify, calendar) would add another copy.

Three were found:

1. **The id charset check** `[a-z0-9][a-z0-9_-]*`, in records (`check_id`) and workspaces
   (`valid_name`). **Already solved:** #65 added `core::ids::is_valid_id` (charset only; each
   caller keeps its own length cap), records and the launcher use it, and it reached master in
   #71. Only workspaces is left to switch, which needs no decision.
2. **Decoding an op's params**: absent params read as `{}`, anything that doesn't fit becomes
   `invalid_params`. Copied three times: records `lib.rs`, workspaces `module.rs` and the daemon's
   `core.rs`, comments included.
3. **The module write lock**: `write: Mutex<()>` plus a `lock()` that ignores poisoning, which
   serialises a module's read-check-write sequences (ADR 0008, "Concurrency"). Copied in records
   and workspaces.

Items 2 and 3 have no home in `core` yet, so adding one is a change to the shared contract and
needs this ADR.

## Decision

Add two small modules to `crates/core`. Both are moved, not redesigned: the code is what all
the copies already do.

### `core::params::decode`

```rust
pub fn decode<T: DeserializeOwned>(params: Value) -> Result<T>
```

`null` (params absent on the wire) reads as `{}`, so an op whose params are all optional can be
called with none. Anything that doesn't deserialize into `T` is `Error::invalid_params`, carrying
serde's message.

### `core::sync::WriteLock`

```rust
#[derive(Debug, Default)]
pub struct WriteLock(Mutex<()>);

impl WriteLock {
    pub fn lock(&self) -> MutexGuard<'_, ()>;
}
```

A module holds one and calls `lock()` around each read-check-write sequence. It guards no data,
so a lock poisoned by a panicking holder carries no information: `lock()` recovers it rather
than failing every later request.

This is only for that pattern. The daemon's own locks (queue, scheduler) guard real data, where
poisoning does mean something, and they are not changed.

### Callers

Records, workspaces and the daemon use the two helpers and delete their copies; workspaces'
`valid_name` becomes `name.len() <= MAX_NAME_LEN && is_valid_id(name)`, keeping its 64-character
cap. New modules use the helpers from the start.

### Nothing changes for anyone using Shimmer

Same rules, same error codes, same messages, same caps. Every existing test is left unchanged
and must still pass, which is how this is checked.

## Why `core`, not somewhere else

These are rules every module needs. Modules may depend only on `core` (CLAUDE.md §3), so `core`
is the only crate they could all share. Both helpers are plain functions and types with no I/O,
no identity (§1.5) and nothing that widens what a module can do through `Ctx`.

## Alternatives considered

- **Leave the copies.** Rejected: each new module adds one more, and they drift.
- **A shared helper crate for modules.** Rejected: a new crate in the dependency rules (§3) for
  two dozen lines, and the daemon also needs `decode`.
- **Put `decode` on `Ctx`.** Rejected: it needs nothing from `Ctx`, and `Ctx`'s fields are a
  capability list (§5), which a parsing helper isn't.
