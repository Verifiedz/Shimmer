# 0018. Records without inventing ids: generated ids and renaming

Status: accepted (records roadmap, #101) · Raised by Dev B (records roadmap, item 5) · Signed off:
Dev A (a new op and topic in `records`, docs/protocol.md) · Dev C notified · No `core` or `proto`
change

## Context

A record's id is its file name (ADR 0008): `[a-z0-9][a-z0-9_-]*`, at most 128 characters. Two
things about it get in the way as soon as a collection has more than a handful of records:

1. **`records.add` requires an id.** Saving a job posting means inventing
   `amazon-sde-intern-s27` on the spot, at exactly the moment the person wants to save it
   quickly. For 200 applications that is 200 names to invent, and every one of them is a chance
   to typo or to clash.
2. **An id can never change.** There is no op for it. A typo (`amzon-sde`) is permanent unless the
   file is renamed by hand, and then the event log still talks about the old id and nothing tells
   a listener the record moved.

ADR 0017 gave every collection a readable title (`"{company}: {position}"`), which is exactly
what an id should be made from.

## Decision

### 1. `records.add` without an id

`id` becomes optional. When it is left out, records makes one:

1. **From the title.** If the collection has a `title` and at least one of its fields is set in
   the new record (ADR 0017 §2), the rendered title is turned into an id:
   - lowercased; letters `a-z` and digits `0-9` kept;
   - every run of anything else (spaces, punctuation, `:`, accented or non-Latin letters)
     becomes one `-`;
   - leading and trailing `-` removed;
   - cut to at most 120 characters at a `-` where possible, leaving room for a suffix.

   `"Amazon: SDE Intern"` → `amazon-sde-intern`; `"Two Sum"` → `two-sum`.
2. **Otherwise from today's date.** No title, no title field set, or a title with nothing left
   after step 1 (for example one written entirely in Arabic or Japanese): the local date
   (ADR 0009) and a counter, `2026-10-05-1`, `2026-10-05-2`, ….
3. **Never clashing.** If the id from step 1 is taken, `-2`, `-3`, … is added until it isn't
   (`amazon-sde-intern-2`). Date ids always carry a counter, starting at 1.

The new id is chosen and written under the module's write lock, so two adds at once can't pick
the same one. The result, as today, is the new item, which includes its `id`; the CLI prints
it so the person learns what to call it:

```
$ shimmer records add jobs --company Amazon --position "SDE Intern"
added jobs/amazon-sde-intern
```

**An id given explicitly is used exactly as before**: checked, and `conflict` if it exists. An
explicit id is never changed or suffixed.

An id is a name, not data: if the company later changes in the record, the id stays. Rename it
(§2) if it matters.

### 2. `records.rename`

| Op | Params | Result |
|---|---|---|
| `records.rename` | `{"collection","id","new_id"}` | the item, under its new id |

- `new_id` follows the id rules (ADR 0008); `invalid_params` otherwise, or when it equals `id`.
- `not_found` if `id` doesn't exist; `conflict` if `new_id` already does.
- **One store transaction** writes the record under its new name, deletes the old file, and
  emits **`records.item.renamed`** with `{collection, id, new_id, title, deadlines, item}`
  (`id` the old one, `item` under the new one; `title` and `deadlines` as in ADR 0017 §7). A
  crash leaves either the old file or the new one, never both or neither (CLAUDE.md §7.1).
- Field values, status and stamps are untouched, so no unique check is needed: the record's
  values don't change.
- Anything that remembers records by id (the calendar, the index in M4, references later,
  roadmap item 13) follows the rename from this one event.

### 3. CLI

```
$ shimmer records add leetcode --title "Two Sum" --difficulty easy
added leetcode/two-sum

$ shimmer records add leetcode --title "Two Sum"
added leetcode/two-sum-2

$ shimmer records rename jobs/amzon-sde amazon-sde
✓ jobs/amzon-sde renamed to jobs/amazon-sde
```

- `records add COLLECTION [ID] [--FIELD VALUE]…`: the id may be left out. `COLLECTION/ID` and
  `COLLECTION ID` keep working.
- `records rename COLLECTION/ID NEW_ID` (or `COLLECTION ID NEW_ID`).
- **Command packs.** `records rename` is a new canonical command, so each built-in pack gains one
  alias (ADR 0013's test): `kaimei` (anime-tropes, "name change"), `rebrand` (ship-it),
  `redesignate` (starship), `rmv` (short).

## Consequences

- `crates/modules/records`: the slug and the id choice are plain functions with unit tests
  (§12 rule 10); `records.add` with no id and `records.rename` are tested against the in-memory
  `Ctx` (§12 rule 13). ADR 0017's title rendering is shared, not repeated.
- `crates/cli`: optional id on `records add`, `records rename`, one alias per built-in pack.
- `docs/protocol.md`: `id` optional on `records.add`; `records.rename` and
  `records.item.renamed`. `crates/mockd/fixtures/core.json` lists the new op and topic.
- Dev C: the TUI can offer "add" without asking for an id, and should follow
  `records.item.renamed`.

## Not done here

- **Changing how ids are made per collection** (e.g. a `[collection] id_from = "{company}"`).
  The title is the obvious source; add a key only if real collections need something else.
- **Transliterating non-Latin titles** (`"شركة"` → `sharika`). Those fall back to the date id;
  a transliteration table is a large dependency for a small gain.
- **Renaming a collection.** Roadmap item 10.
- **Updating references when a record is renamed.** References don't exist yet (roadmap item 13);
  when they do, they follow `records.item.renamed`.
