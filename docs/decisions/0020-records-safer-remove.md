# 0020. Removing a record without losing it: trash, restore, and a full removal event

Status: accepted (records roadmap, #101) · Raised by Dev B (records roadmap, item 6) · Signed off:
Dev A (an `records.item.removed` payload change, a new op and topic, docs/protocol.md) · Dev C
notified · No `core` or `proto` change

## Context

`records.remove` (ADR 0008) deletes the record's file at once, and `records.item.removed`
carries only `{collection, id}`. Two problems:

1. **A mistaken remove is permanent.** The record's last state survives only inside an earlier
   `created` / `updated` / `completed` event in the log, which a person can't easily get back.
2. **Listeners can't say what went.** A calendar dropping a reminder, or a notification ("you
   removed Amazon: SDE Intern"), gets an id and nothing else.

The roadmap (#82, item 6) suggested restoring from the event log. That isn't possible without a
`core` change: a module's store only reaches its own namespace (`crates/core/src/store.rs`), and
the event log lives outside it. Giving modules read access to the log would be a new capability
in `Ctx`, which CLAUDE.md §5 treats as a security decision, for one feature. This ADR keeps the
removed record inside records' own namespace instead.

## Decision

### 1. Removing moves the record to a trash folder

`records.remove` moves `items/<collection>/<id>.toml` to `trash/<collection>/<id>.toml`, both in
records' namespace (`data/records/`), and emits `records.item.removed`, all in **one store
transaction** (CLAUDE.md §7.1).

- The trashed file is the record exactly as it was: still a human-readable file (§1.4), still
  in `$SHIMMER_HOME`, still copied and committed with everything else. Nothing is lost by moving
  machines.
- Removing a record with the same id again replaces the earlier trashed copy: the trash holds the
  **latest** removed version of each id.
- `records.list`, `get` and every other op ignore the trash; a removed record is gone from the
  collection in every way that matters.
- Emptying the trash is deleting files in `trash/` by hand for now (see "Not done here").

### 2. `records.item.removed` carries the full record

The payload becomes `{collection, id, title, deadlines, item}`, the same shape as the other item
events (ADR 0017 §7), with `item` the record as it was when removed. A listener knows what went
and what to call it.

### 3. `records.restore`

| Op | Params | Result |
|---|---|---|
| `records.restore` | `{"collection","id"}` | the record, back in the collection |

- Moves `trash/<collection>/<id>.toml` back to `items/…` and emits **`records.item.restored`**
  (`{collection, id, title, deadlines, item}`), in one transaction.
- `not_found` if nothing with that id is in the trash.
- `conflict` if a record with that id exists again (someone added a new `two-sum` after removing
  the old one), or if a `unique` value of the restored record has since been taken by another
  record (ADR 0017 §4). Rename one of them first (ADR 0018).
- Restoring brings back the record exactly as removed: same status, same values, same stamps.

### 4. `records.trash`

| Op | Params | Result |
|---|---|---|
| `records.trash` | `{"collection"}` | `{"items":[…]}`: the removed records, by id, in the wire shape |

So a person can see what they could restore. Unreadable trash files are skipped, as in
`records.list`.

### 5. CLI

```
$ shimmer records remove jobs/amazon-sde-intern
removed jobs/amazon-sde-intern (undo: shimmer records restore jobs/amazon-sde-intern)

$ shimmer records trash jobs
ID                 STATUS  COMPANY  POSITION    …
amazon-sde-intern  todo    Amazon   SDE Intern  …

$ shimmer records restore jobs/amazon-sde-intern
✓ jobs/amazon-sde-intern restored
```

- **No "are you sure?" prompt.** With the trash, a remove can be undone in one command, which
  is better than a question people learn to answer without reading, and nothing changes for
  scripts.
- **Command packs.** `records restore` gets one alias per built-in pack (ADR 0013's test):
  `okaeri` (anime-tropes, "welcome back"), `revert` (ship-it), `salvage` (starship), `rundo`
  (short). `records trash` is a listing and, like `packs` commands, gets no alias.

## Consequences

- `crates/modules/records`: `remove` moves instead of deleting; `records.restore` and
  `records.trash`; the fuller removed payload. Tested against the in-memory `Ctx` (§12 rule 13).
- `crates/cli`: `records restore`, `records trash`, the undo hint after `records remove`, one
  alias per built-in pack.
- `docs/protocol.md`: the payload change, two ops, one topic. `crates/mockd/fixtures/core.json`
  lists them. Dev C told: a TUI "delete" can offer "undo".
- `data/records/trash/` is new user-visible data; the user guide should mention it.

## Not done here

- **Emptying the trash** (`records.purge`, or removing trashed records after N days). Deleting
  the files by hand works; an op can come when someone wants it, with a confirmation that names
  how many records go for good.
- **Keeping every removed version** of an id (timestamped trash). The latest is what people
  undo; the event log still records every change.
- **Removing whole collections** (roadmap item 10).
- **Restoring from the event log.** Needs log access in `Ctx`, a `core` change; not worth it while
  the trash covers the need.
