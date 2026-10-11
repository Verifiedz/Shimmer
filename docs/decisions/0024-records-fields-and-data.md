# 0024. Records: datetime, list and reference fields; import and export; query follow-ups

Status: accepted (records roadmap, #101) · Raised by Dev B (records roadmap, items 7, 8, 11, 13 and
the follow-ups left by ADRs 0017–0021) · Signed off: Dev A (new field types and ops in `records`,
docs/protocol.md) · Dev C notified (new wire shapes) · No `core` or `proto` change

## Context

The records roadmap (#82) left four items that only `records` and the CLI need to change:

- **Item 7, `datetime`.** Field types are `string`, `int`, `bool`, `date`, `enum`. "OA due
  23:59 PT" lives in `notes`, where nothing can remind you of it.
- **Item 8, list fields.** A field holds one value, so a tech stack is `"Go, Rust"` and can't be
  filtered properly.
- **Item 13, reference fields.** `interviews.application = "google-swe-ng"` is plain text: a
  typo links to nothing and removing the job leaves rounds pointing at nothing.
- **Item 11, import and export.** Records come in one `records add` at a time.

And the ADRs since 0017 each left small follow-ups: relative dates in filters (0019), titles in
list responses (0017), optional parts in titles (0017), emptying the trash (0020, 0021), and
alternatives across fields (0019).

All of it is decided here so it can ship as one PR, reviewed a commit per section. Items 12
(private collections: writing `.gitignore` is outside records' namespace, Dev A's area) and 15
(kits: needs workspace templates and the calendar) are not part of it.

## Decision

### 1. `datetime`

```toml
[[field]]
name = "oa_due"
type = "datetime"
role = "deadline"
```

- **Stored as UTC**, RFC 3339 to the second (`"2026-10-21T06:59:00Z"`), like every timestamp
  Shimmer writes (ADR 0009: never store local time).
- **A plain date is also a valid value** (`"2026-10-20"`) and stays a plain date: "due that day,
  no time given". So a `date` field can be changed to `datetime` in a collection file and every
  existing value stays valid.
- **Input** is any of: RFC 3339 with an offset or `Z` (`2026-10-20T23:59:00-07:00`), a local time
  without a zone (`2026-10-20 23:59` or `2026-10-20T23:59`), or a plain date. A local time is read
  in the configured `[general] local_timezone`. The daemon converts, using the zone's name from
  `ctx.local_tz` and `chrono-tz` (already a dependency of `core`, same version), so no `core`
  change is needed. A local time that falls in a daylight-saving gap is `invalid_params`; one that
  happens twice takes the earlier.
- **Comparing and sorting** use instants; a plain date counts as the start of that day in the
  local timezone.
- `role = "deadline"` and `stamp_on_complete` may name a `datetime` field. Stamping a `datetime`
  writes the current instant.
- **The CLI shows** a `datetime` in the machine's local time (`2026-10-20 23:59`); `--json` shows
  the stored value.

### 2. List fields

```toml
[[field]]
name = "tech_stack"
type = "list"
of = "string"            # or "enum" (with values = [...]), or "ref" (with collection = "...")
```

- A TOML array on disk, a JSON array on the wire. An empty list counts as unset. `required` means
  at least one item.
- `of` must be `string`, `enum` or `ref`; `unique` and `role` are not allowed on a list.
- **Filtering:** a new operator, `has`: the list contains this item (strings ignoring case).
  A plain value on a list means `has`, and `null` means unset. `set: true/false` works as for any
  field; no other operator applies. Lists can't be sorted.
- **Stored tidy:** string items are trimmed, blank items and repeats (ignoring case) dropped,
  order kept. A list that ends up empty is unset.
- **Updating:** a list field in `records.update` (or `complete`'s `fields`) takes either a whole
  new list, or `{"add": [...], "remove": [...]}` to change items without resending the rest.
- **Search** looks in string and enum list items too; **titles** render a list as its items joined
  with `, `.
- **CLI:** `--tech-stack "go, rust"` sets the list (comma-separated); on `records update`,
  `--add FIELD VALUE` and `--remove FIELD VALUE` change items; `--filter "tech_stack has rust"`
  filters.

### 3. Reference fields

```toml
[[field]]
name = "application"
type = "ref"
collection = "jobs"      # the collection its value must be a record of
```

- The value is the target record's id, a plain string, on disk and on the wire. A `string` field
  changed to `ref` keeps every value.
- **Checked on write:** setting a ref (on add, update, complete, import) to an id that isn't a
  record in the target collection is `invalid_params` naming both. A ref to a collection that
  doesn't exist is an error naming the collection file.
- **Removing a target that others point at is `conflict`**, listing up to five of the records
  that refer to it (all of them in `detail.referred_by`, as `"collection/id"`), unless
  `"force": true` (`--force`). Forced, the references are left dangling
  and `records.check` reports them. Never cascade-deleting user data.
- **Renaming a target** (`records.rename`) rewrites every reference to it in the same transaction,
  and the event says how many (`references`). **Renaming a collection** (`records.rename_collection`) rewrites
  `collection = "…"` in every collection file that refers to it, comments kept.
- `records.check` reports references to records that don't exist.
- Filters and sorting treat a ref like a string; lists of refs (`of = "ref"`) use `has`.
- **Templates keep links as text for now** (`application`, `story`, `event`): a template can't
  know what the user will name the target collection (`jobs` or `job-applications`), and a
  `ref` must name it. Kits (#79), which pick both names together, are where template refs belong.

### 4. Import and export

| Op | Params | Result |
|---|---|---|
| `records.import` | `{"collection","rows":[{…}],"dry_run"?,"skip_invalid"?}` | `{"added":[ids],"skipped":[{"row","reason"}],"invalid":[{"row","error"}],"written":bool}` |

- Each row is an object of field values, checked exactly like `records.add`, with an optional
  `id` (generated as in ADR 0018 when left out) and an optional `status` (`todo`/`done`, so an
  export imports back as it was). `null` and `""` are unset. Rows are numbered from 1. Ids and
  `unique` values are checked against the collection **and against the other rows**.
- A row whose id or `unique` value already exists is **skipped**, never overwritten, and
  reported, so importing the same file twice adds nothing the second time. The same id twice in
  one import is invalid.
- `added` lists the ids that were (or, when `written` is false, would be) added.
- **All or nothing:** every row that will be added is written in **one store transaction**, one
  `records.item.created` per record. If any row is invalid, nothing is written unless
  `skip_invalid` is set. `dry_run` checks everything and writes nothing.
- At most 5000 rows per import.
- **The CLI reads the file:** `shimmer records import COLLECTION FILE` takes CSV (a header row of
  field names; an `id` column is optional; empty cells are unset; list items separated by `;`) or
  JSON (an array of objects), types values from the collection like `--FIELD VALUE` does, shows
  the dry-run summary, then asks before importing at a terminal (`--yes` without one; never waits
  on a prompt nobody sees). `--dry-run` stops after the summary; `--skip-invalid` imports the rest.
- **Export is client-side:** `shimmer records export COLLECTION [--format csv|json] [filters]`
  pages through `records.list` and writes to stdout. CSV lists are joined with `;`, so an export
  imports back unchanged.

### 5. Follow-ups

- **Relative dates** in `filter` values for `date` and `datetime` fields: `"today"`,
  `"today+7"`, `"today-30"` (days), resolved by the daemon in the configured timezone. So
  `--filter "oa_deadline<=today+7"` means "due within a week", and the CLI never needs to know the
  timezone. A plain date compared with a `datetime` compares days, so `<= 2026-10-20` includes
  23:59 that day (sorting still uses instants, §1).
- **Titles in `records.list`:** the response gains `titles: {id: title}` for the listed records.
  `records list` shows them as a TITLE column, unless the title is a single field that already
  has its own column.
- **Optional parts in titles:** `"{title}[ by {author}]"`. A `[…]` part is left out when any field
  inside it is unset, so no dangling "by". A `[…]` part needs a field inside and doesn't nest.
  `[` and `]` outside that use are now an error in a title (no shipped title uses them).
- **`records.purge {collection, id?, confirm?}`:** permanently deletes trashed records (one, or the
  whole trash), after `confirmation_required` with `{"records": N}`, as removing a collection asks
  (ADR 0021). Emits `records.trash.purged`.
- **OR across fields:** `filter` may hold `"or": [ {…}, {…} ]`: a record matches if it matches any
  of the inner filters (each an ordinary, non-empty filter; `or` doesn't nest), and every other
  key still applies. A collection with a field named `or` keeps filtering on that field. CLI:
  `--or "stage=offer" --or "priority=dream"`.

### 6. Templates

The built-in templates switch to the new types where they fit, before anyone has made a
collection from them:

- **Lists:** `projects.stack`, `stories.themes`, `networking-events.companies`,
  `leetcode.topic`, `job-applications`'s new `tech_stack`.
- **Datetimes:** deadlines that have a time of day: `job-applications.oa_deadline`,
  `interviews.scheduled_on` (now `scheduled_at`), `networking-events.event_date`
  (now `starts_at`, replacing `event_date` + `start_time`), `offers.decision_deadline`,
  `certifications.exam_date` (now `exam_at`).

## Consequences

- `crates/modules/records`: new types in `schema.rs`; normalising input (times, list patches)
  before checking; refs checked against the store; `records.import`, `records.purge`; the
  `rename` and `remove` changes; `chrono-tz` added. Each part tested as pure functions and through
  the ops against the in-memory `Ctx` (§12 rules 10, 13).
- `crates/cli`: list flags, `--add`/`--remove`, `--or`, `--force`, local display of datetimes,
  `records import`, `records export`, `records purge`.
- `docs/protocol.md`: the field types, `has`, `or`, relative dates, `titles`, the two ops and the
  topic. Dev C told: list values are arrays and datetimes are RFC 3339 strings on the wire.

## Not done here

- Private collections (item 12) and kits (item 15).
- Lists of `datetime` or `int`; nested lists.
- Transliterating zone abbreviations ("PT"): offsets only.
- Ref targets in templates (see §3).
