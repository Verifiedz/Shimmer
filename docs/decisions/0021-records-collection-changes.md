# 0021. Collections over time: reference lists, checking, renaming, removing

Status: accepted (records roadmap, #101) · Raised by Dev B (records roadmap, items 9 and 10; issue
#81) · Signed off: Dev A (new ops and topics in `records`, docs/protocol.md, a new dependency) ·
Dev C notified (items can lack `status`) · No `core` or `proto` change

## Context

Two gaps about collections themselves, rather than their records:

**Reference lists (#81).** Records were designed around things you finish, so every record has
`status` (`todo`/`done`) and every collection accepts `records.complete`. Education history,
contacts, bookmarks you keep: these have nothing to finish. Every record sits at `todo` forever,
`complete` does something meaningless, and every list shows a status column that says nothing.

**Changing a collection once it has data.** Checked against the code, today:

| Change | What happens |
|---|---|
| Add a field | fine |
| Remove a field | old values stay in record files as unknown keys |
| Rename a field | old values stranded under the old name; the only fix is editing every file |
| Remove an enum value | safe: updates only check values being set |
| Make a field `required` | later updates of old records fail until it's filled in |
| Change a field's type | old values may no longer fit; nothing says so |
| Rename / remove a collection | no op; by hand, and the event log never hears about it |

## Decision

### 1. `[collection] completable = false`

Optional; the default `true` keeps every existing collection exactly as it is.

When `false`:

| | |
|---|---|
| `records.complete`, `records.reopen` | `invalid_params`: "'education' has no completion (completable = false)" |
| `stamp_on_complete` | not allowed in the same file |
| Items on the wire | no `status` key |
| Record files | `status` is not written for new records; a `status` line already in a file is kept as it is (never rewritten as a side effect of a setting) but ignored |
| `filter` / `sort` on `status` | `invalid_params`: "'education' has no status (completable = false)" |
| `records.collections` | always says `completable` (`true` or `false`), so clients know whether to show status |
| CLI | no status column in lists, no `(todo)` after a record's name |

### 2. `records.check`: what doesn't fit, without changing anything

| Op | Params | Result |
|---|---|---|
| `records.check` | `{"collection"}` | `{"checked": N, "problems": [{"id", "problems": ["…", …]}]}` |

For each record: required fields missing, values that don't fit their field's type (a retired
enum value, `"30"` in an `int` field, a bad date), keys the collection doesn't know (left over
from a removed or renamed field), and files that can't be read at all. Nothing is written. Run it
after editing a collection file by hand.

### 3. `records.rename_field`

| Op | Params | Result |
|---|---|---|
| `records.rename_field` | `{"collection","from","to"}` | `{"collection": {…}, "updated": N}`: the collection as `records.collections` shows it, and how many records changed |

- `to` must be a valid field name, not reserved, not already a field.
- **One transaction** rewrites the collection file and every record (live and in the trash) that
  has a value under `from`, and emits **`records.field.renamed`** (`{collection, from, to}`).
- **The collection file keeps its comments and layout.** It is edited with `toml_edit` (a new
  dependency of `crates/modules/records`, from the same family as `toml`, which already depends
  on its parser): the field's `name`, `stamp_on_complete`, every `{from}` in `title`, and any
  `[[view]]` `source = "from"` change; nothing else does.
- Record files are written by records anyway (ADR 0008), so they are simply re-written with the
  key renamed.
- One transaction for a large collection writes many files at once. That is the price of all or
  nothing (§7.1), and it is still far better than editing them by hand.

### 4. `records.rename_collection`

| Op | Params | Result |
|---|---|---|
| `records.rename_collection` | `{"id","new_id"}` | the collection under its new id |

One transaction moves the collection file (with its `[collection] id` changed, comments kept),
every record, and the collection's trash, and emits **`records.collection.renamed`**
(`{collection, new_id}`). `conflict` if `new_id` exists. Other collections' `related` lists and
workspace scripts that name the old id are not touched; they are the user's text.

### 5. `records.remove_collection` and `records.restore_collection`

Removing a collection removes every record in it, so it asks first, the same way queue
promotion does (CLAUDE.md §6.2):

| Op | Params | Result |
|---|---|---|
| `records.remove_collection` | `{"id","confirm"?}` | `{"removed": true, "records": N}` |
| `records.restore_collection` | `{"id"}` | the collection, back |

- Without `confirm`, or with a `confirm` that doesn't match, the answer is
  **`confirmation_required`** with `detail: {"records": N}`. The client shows "this removes
  `jobs` and its N records" and re-sends with `"confirm": {"records": N}`. If records were added
  or removed in between, the count no longer matches and it asks again, so a confirmation is
  never for a different collection than the one shown.
- Removing **moves** the collection file, its records and its trash into
  `removed-collections/<id>/` (next to `trash/`, so it can never clash with a collection named `collections`), in one transaction, and emits **`records.collection.removed`**
  (`{collection, records}`). Nothing is deleted.
- `records.restore_collection` moves it all back (`conflict` if a collection with that id exists
  again) and emits **`records.collection.restored`**.
- Removing every collection brings LeetCode back on the next start (ADR 0008's seeding); that
  stays as it is.

### 6. CLI

```
$ shimmer records check jobs
checked 42 records; 2 with problems
  acme      field 'stage': 'ghosted' is not one of screening, interviewing, offer, rejected
  initech   unknown key 'recruiter' (not a field of 'jobs')

$ shimmer records rename-field jobs recruiter contact
✓ jobs: field 'recruiter' is now 'contact' (3 records updated)

$ shimmer records rename-collection jobs applications
✓ jobs is now applications

$ shimmer records remove-collection applications
This removes 'applications' and its 42 records. They go to the trash:
  shimmer records restore-collection applications brings them back.
Remove it? [y/N] y
removed applications (42 records)
```

- `remove-collection` asks only at a terminal. Without one it refuses and says to add
  `--yes`, which sends the confirmation with the count it just read. It never waits for an answer
  nobody can see (CLAUDE.md §15.1).
- **No pack aliases** for these five. They are rare, and the destructive ones should be typed in
  full, on purpose. Packs alias everyday commands; ADR 0013's list of commands a pack can name
  doesn't include them, like `packs` and `records trash`.

## Consequences

- `crates/modules/records`: `completable` in `schema.rs` and `item.rs`; the pure parts (the
  check, the collection-file edits) tested without a runtime (§12 rule 10); the ops tested
  against the in-memory `Ctx` (§12 rule 13); `toml_edit` added.
- `crates/cli`: five commands; lists and records without status for reference collections.
- `docs/protocol.md`: `completable`, five ops, four topics. `crates/mockd/fixtures/core.json`
  lists them. Dev C told: items may have no `status`.

## Not done here

- **Changing a field's type** as an op (`string` → `enum` when every value fits). `records.check`
  shows what wouldn't fit; converting is a later op if needed.
- **Removing a field's values** from every record (the stranded keys `records.check` reports).
  Leaving them is harmless; an op can come later.
- **Emptying the trash**, including removed collections (ADR 0020).
- **Following a rename in other collections' `related`** or in workspace scripts.
