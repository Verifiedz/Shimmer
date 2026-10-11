# 0017. Collections describe what their data means: title, roles, unique, related, extra

Status: accepted (records roadmap, #101) · Raised by Dev B (records roadmap, item 2) · Signed off:
Dev A (event payloads, docs/protocol.md) · Dev C notified (new collection keys and event fields) ·
No `core` or `proto` change

## Context

Records is the data the rest of Shimmer will act on: the calendar reminds you about deadlines
(CLAUDE.md §1.3), notify names the thing it is reminding you about, fetchers match a found job
posting to one you saved, workspaces open a record's link. None of them may read records' files
or call it (§1.3, §12 rule 2). They only see records' **events**.

Today an event says *what* changed (`{collection, id, item}`), but nothing says what the data
*means*:

- `apply_by` (a deadline) and `applied_on` (something that already happened) are both just
  `date` fields. A calendar can't tell which one to remind about.
- A record has an id (`amazon-sde-intern`) but no readable name, and which fields make a good
  name differs per collection.
- Nothing marks a field as identifying the record, so nothing can refuse saving the same posting
  twice, and a fetcher has nothing to match on.
- `records.item.updated`'s `changed` lists every key the request sent, even ones whose value
  didn't change (`lib.rs`, `update`), so "the stage just became `offer`" can't be told from "the
  stage was sent again".

This has to land **before** collection templates (roadmap item 1): once someone creates a
collection from a template, the file is theirs, and later improvements to the template never
reach it. So the format must already say what other features will need.

The principle: **records describes and announces; everything else listens.** The keys below say
what data *is*, never which feature uses it, so a feature nobody has written yet can use them
without records changing.

## Decision

All keys are **optional**. Every existing collection file stays valid and behaves as before.

### 1. `[collection] description`

One line, display only. Returned by `records.collections` for clients to show (the TUI; the
record collection templates' listing, roadmap item 1).

### 2. `[collection] title`: a readable name for each record

```toml
[collection]
title = "{company}: {position}"
```

- `{name}` is replaced by that field's value. Every `{name}` must be a field of the collection,
  checked when the file is read (`invalid_params` naming the file). No other syntax: no
  fallbacks, no formatting, and a `{` or `}` that isn't part of a `{name}` is an error.
- Values render as written: strings as they are, ints and bools as text, dates as `YYYY-MM-DD`.
  An unset field renders as nothing. Surrounding whitespace is trimmed.
- **When none of the fields the template names is set, or there is no `title`, the title is
  the record id**, so every record always has one, and `"{company}: {position}"` with nothing
  filled in is never shown as a bare `":"`. An empty string counts as unset.
- **The title goes in events (§7), not in items.** Items on the wire are flat
  (`{id, status, <fields>}`, ADR 0008), and a field may itself be called `title` (LeetCode's is),
  so a `title` key in the item would collide.

The built-in `leetcode.toml` gains `title = "{title}"`; existing copies are unaffected and fall
back to the id.

### 3. Field `role`: what a field means

```toml
[[field]]
name = "oa_deadline"
type = "date"
role = "deadline"
```

| Role | Allowed on | Means |
|---|---|---|
| `deadline` | `date` | Something is due on this date. |
| `url` | `string` | The record's link. At most one per collection. |

- A role on the wrong type, an unknown role, or a second `url` field is an error naming the file.
- **The vocabulary grows by ADR**, on purpose: a role is a promise other features rely on, so
  each one is defined once, here.
- `stamp_on_complete` may not name a `deadline` field: a stamp records something that happened.

### 4. Field `unique = true`: no two records share this value

- Allowed on `string`, `int`, `date` and `enum` fields; not `bool` (two values can't be unique
  across more than two records).
- **Checked whenever the field is being set**: on `records.add`, on `records.update` and on
  `records.complete` with `fields`, when the request sets that field to a value. Another record
  in the same collection with the same value is `conflict`, naming it:
  `url 'https://amazon.jobs/123' is already used by 'amazon-sde-intern' in 'jobs'`.
- **Unset and `""` don't count**: many records may have no URL.
- **Not checked on read.** Duplicates a hand-edit introduced stay readable, and updating *other*
  fields of those records still works; only setting the unique field to a taken value fails.
- The check reads every record in the collection, like `records.list` does today (ADR 0008:
  fine at this scale; the index serves it in M4). It runs under the module's write lock, so two
  concurrent adds can't both take the same value.

### 5. `[collection] related`: collections that go together

```toml
[collection]
related = ["interviews"]
```

A list of collection (or, later, template) ids, each a valid id. Display only: the CLI may say
"goes with: interviews". Records never checks that they exist, so related collections can be
created in any order.

### 6. `[extra.<name>]`: settings records doesn't understand

```toml
[extra.calendar]
lead_days = [3, 1]
```

- Top-level `extra` must be a table of tables (`[extra.<name>]`); anything else is an error
  naming the file. Inside each table, anything goes.
- Kept and returned **untouched** in `records.collections` as `extra`, the same way `[[view]]`
  tables pass through (CLAUDE.md §5). Records never reads them.
- This is how a later feature, or a plugin, gets per-collection settings without a change to
  records. Each `<name>` belongs to whoever defines it; this ADR defines none.

### 7. Events say what they mean

`records.item.created`, `.updated`, `.completed` and `.reopened` gain two fields:

| Field | Value |
|---|---|
| `title` | the record's title (§2), always present |
| `deadlines` | `{field: "YYYY-MM-DD"}` for every **set** field with `role = "deadline"`; `{}` when none |

```json
{"collection": "jobs", "id": "amazon-sde-intern",
 "title": "Amazon: SDE Intern",
 "deadlines": {"apply_by": "2026-10-15", "oa_deadline": "2026-10-20"},
 "item": {"id": "amazon-sde-intern", "status": "todo", "company": "Amazon", ...}}
```

A listener (the calendar, a plugin) learns what is due and what to call it from the event alone.
`records.item.removed` is unchanged here; roadmap item 6 gives it the full item.

### 8. `changed` lists only real changes

`records.item.updated`'s `changed` becomes the sorted list of fields whose value **actually
changed** (set, unset, or a different value), comparing the record before and after. Sending
`stage = "oa"` to a record already at `oa` no longer lists `stage`. An update that changes
nothing still writes and emits, with `changed: []`, so a script that sent it sees its request
answered.

### 9. `records.collections` and the CLI

`records.collections` returns every new key: `description`, `title`, `related`, `extra`, and per
field `role` and `unique` (each only when set). `shimmer records collections` shows each field's
role and uniqueness:

```
ID     LABEL  FIELDS
jobs   Jobs   company*, url (url, unique), stage (oa|interview), apply_by (date, deadline), applied_on (date)
```

## Consequences

- `crates/modules/records`: `schema.rs` parses and checks the new keys (pure, §12 rule 10); the
  title, deadlines, unique check and real `changed` are plain functions with unit tests; the ops
  add them to events; op-level tests against the in-memory `Ctx` (§12 rule 13).
- `leetcode.toml` gains `title = "{title}"`.
- `crates/cli`: `records collections` shows roles and `unique`.
- `docs/protocol.md`: the collection format, the two event fields, the new `changed` meaning.
  Dev C told: the TUI can show titles from events, and `changed` now means what it says.
- Templates (roadmap item 1) use all of this; the calendar (M6) reads `deadlines` from events.

## Not done here

- **Titles in `records.list` / `records.get` responses.** Items are flat and a field may be named
  `title` (§2); a response-level `titles` map can come with filters and sorting (roadmap item 4)
  if clients need it.
- **More roles** (`email`, `phone`, `started`, …): each by ADR when a feature needs it.
- **`unique` across several fields together** (company + job id): not needed yet.
- **Reminder timing** (how long before a deadline): the calendar's decision, possibly through
  `[extra.calendar]`.
