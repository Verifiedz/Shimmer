# 0016. Records completion lifecycle: reopen, complete with fields, repeated completions

Status: accepted (records roadmap, #101) · Raised by Dev B (records roadmap, item 3) · Signed off:
Dev A (a new op and topic in `records`, docs/protocol.md) · Dev C notified (new op, new topic, new
collection key) · No `core` or `proto` change

## Context

ADR 0008 gave records one way to finish something: `records.complete {collection, id}` sets
`status = "done"` and, when the collection names one, stamps today's local date (ADR 0009) into
its `stamp_on_complete` field. Using it for anything beyond LeetCode exposes three gaps, all
checked against `crates/modules/records/src/lib.rs`:

1. **A completion can't be undone.** `status` is reserved (`schema.rs`, `RESERVED`), and
   `records.update` refuses it ("collection '…' has no field 'status'"). A record completed by
   mistake stays `done` unless its file is edited by hand, and then the event log disagrees with
   the file.
2. **Completing can't record anything else.** `records.complete` takes only `{collection, id}`.
   Finishing an interview round and noting that it was passed is two requests, two transactions
   and two events (`completed`, then `updated`), with a moment in between where the record says
   "done" with no outcome.
3. **Completing again always re-stamps.** A second `records.complete` on a `done` record moves
   the stamp to today and emits another `records.item.completed`. For LeetCode that is right:
   solving a problem twice is two solves, and the heatmap should count both (ADR 0008). For a
   job application it is wrong: "I applied" happens once, and a second completion silently
   moves the applied date and counts the application twice.

These are behaviour gaps, not format gaps: no collection file has to change to benefit, and
records created before this ADR keep working as they do.

## Decision

### 1. `records.reopen`: done → todo

| Op | Params | Result |
|---|---|---|
| `records.reopen` | `{"collection","id","clear_stamp"?}` | the item, now `todo` |

- Sets `status = "todo"` and writes the record and a **`records.item.reopened`** event
  (`{collection, id, item}`, the full wire item as the other item events carry) in one store
  transaction (CLAUDE.md §7.1).
- **The stamp is kept by default.** `last_solved = "2026-10-04"` stays true after reopening: the
  problem *was* solved that day, and is now due again. `"clear_stamp": true` removes the
  collection's `stamp_on_complete` field as well, for a completion that was simply a mistake.
  `clear_stamp` on a collection with no `stamp_on_complete` is accepted and does nothing.
  The stamp field itself can't be `required` (a collection file that says so is rejected when
  it's read): a `todo` record has no stamp yet, and `clear_stamp` removes it.
- **Reopening a `todo` record is `conflict`** ("'two-sum' in 'leetcode' is not done"), not a
  silent success: a script that reopens the wrong record should find out.
- A dedicated topic, not `records.item.updated` with `changed: ["status"]`, because the two mean
  different things to a listener: a heatmap or a streak counter cares that a completion was taken
  back; a field watcher doesn't.
- **Earlier completions stay in history.** The `records.item.completed` events already in the log
  are not removed or rewritten (§1.4: the log is append-only). Whatever reads them, the M4 index
  or a heatmap, decides whether a reopen cancels the completion before it. This ADR only makes
  sure the information is there.

### 2. `records.complete` accepts `fields`

| Op | Params | Result |
|---|---|---|
| `records.complete` | `{"collection","id","fields"?}` | the item, now `done` |

- `fields` follows exactly the rules of `records.update` (ADR 0008): every key must be a field of
  the collection, every non-null value must fit its type, `null` unsets, and the record must still
  have every required field afterwards. The checks run **before** anything is written.
- Order: apply `fields`, then set `status = "done"`, then stamp. One transaction, **one**
  `records.item.completed` event whose `item` already holds the new values.
- **An explicit value for the stamp field wins over today's date.**
  `records complete jobs/amazon-sde-intern --applied-on 2026-10-03` records that you applied
  yesterday. This is the one way to back-date a completion without a second request.
- `fields` absent or `{}` behaves as today, with one difference: the required-fields check above
  always runs, so if a collection file later marks a field `required`, completing an older record
  that lacks it is `invalid_params` until the field is set (in the same `complete`, with
  `fields`). That is the same rule `records.update` already follows.

### 3. `repeat_complete`: what completing a `done` record means

A new optional key in the collection file:

```toml
[collection]
id = "job-applications"
label = "Job applications"
stamp_on_complete = "applied_on"
repeat_complete = "refuse"     # or "restamp" (the default)
```

| Value | Completing a record that is already `done` |
|---|---|
| `"restamp"` (default) | Today's behaviour: re-stamp (or use an explicit stamp value from `fields`), apply `fields`, emit another `records.item.completed`. Right for things you repeat: LeetCode problems, workouts, reviews. |
| `"refuse"` | `conflict`: "'amazon-sde-intern' in 'jobs' was already completed on 2026-10-04" (or "is already done" when there is no stamp). Nothing is written. Right for things that happen once: applying, attending, merging. To complete it again, reopen it first, on purpose. |

- Any other value is an error naming the file, as with every other key (ADR 0008).
- **The default is `"restamp"`**, so every existing collection, including LeetCode, behaves
  exactly as before. `leetcode.toml` does not gain the key.
- `records.collections` includes `repeat_complete` in each collection's JSON, always (with its
  default filled in), so clients don't have to know the default.

### 4. CLI

```
$ shimmer records reopen leetcode/two-sum
✓ leetcode/two-sum reopened (todo)

$ shimmer records reopen jobs/amazon-sde-intern --clear-stamp
✓ jobs/amazon-sde-intern reopened (todo); cleared applied_on

$ shimmer records complete interviews/g-phone-1 --outcome passed
✓ interviews/g-phone-1 done (done_on 2026-11-04)

$ shimmer records complete jobs/amazon-sde-intern --applied-on 2026-10-03
✓ jobs/amazon-sde-intern done (applied_on 2026-10-03)

$ shimmer records complete jobs/amazon-sde-intern
shimmer: conflict: 'amazon-sde-intern' in 'jobs' was already completed on 2026-10-03
```

- `records complete` takes field flags and `--unset FIELD` exactly as `records update` does
  (`--FIELD VALUE`; `--last-solved` finds `last_solved`).
- `records reopen` takes the same `<collection>/<id>` or `<collection> <id>` forms as
  `complete`, plus `--clear-stamp`.
- **Command packs.** `records reopen` is a new canonical command, so each of the four built-in
  packs gains one alias (ADR 0013's test requires every built-in pack to name every command):

  | Pack | Alias |
  |---|---|
  | `anime-tropes` | `mou-ikkai` ("one more time") |
  | `ship-it` | `regression` |
  | `starship` | `re-entry` |
  | `short` | `rreopen` |

  User packs don't have to name it; `shimmer records reopen` always works.

## Consequences

- `crates/modules/records`: `records.reopen`; `fields` on `records.complete`; `repeat_complete`
  in `schema.rs`; the new topic in the manifest. The decisions (may this be completed? reopened?
  what does the record look like after?) are plain functions over `Item` and `Collection`, tested
  without a runtime (§12 rule 10); the ops are tested against the in-memory `Ctx` (§12 rule 13).
- `crates/cli`: `records reopen`, field flags and `--unset` on `records complete`, one alias per built-in
  pack.
- `docs/protocol.md`: the records ops table gains `records.reopen` and the `fields` param; the
  topics table gains `records.item.reopened`; the collection format notes `repeat_complete`.
  Dev C is told.
- `crates/mockd/fixtures/core.json` lists `records.reopen` in `core.manifest`, so the TUI can see
  it against the mock daemon (Dev A's crate; small fixture change).
- ADR 0008's collection format gains one optional key; files without it behave as before.

## Not done here

- **What a heatmap counts after a reopen.** The events are all there; the index (M4) or the view
  decides.
- **`changed` in events** (it lists every key sent, not only real changes): records roadmap item 2.
- **Collections that can't be completed at all** (`completable = false`): roadmap item 9. When it
  lands, `records.reopen` is refused there too.
- **Undoing other operations** (update, remove): remove is roadmap item 6; general undo is not
  planned.
