# 0019. Asking records real questions: comparisons, search and sorting

Status: accepted (records roadmap, #101) · Raised by Dev B (records roadmap, item 4) · Signed off:
Dev A (`records.list` params, docs/protocol.md) · Dev C notified (the TUI's tables use
`records.list`) · No `core` or `proto` change

## Context

`records.list` (ADR 0008) answers one kind of question: "which records have exactly this value".
`filter` is exact equality on each key (`null` matching unset), and results come back in one
order. That covers "LeetCode problems I haven't solved", and not much else a tracker is for:

- "OA deadlines before Friday", "applied in the last 30 days", "salary over X": no comparisons.
- "stage is `oa` or `interview`", "stage is anything but `rejected`": no alternatives, no "not".
- "jobs with a URL" / "jobs without one": only "without" (`null`) exists.
- "anything mentioning Google": no text search.
- "soonest deadline first", "newest first": no sorting.

And the one order there is isn't quite the id order it claims (`lib.rs`: "Sorted by path, so by
id"). Paths end in `.toml`, and `-` sorts before `.`, so `two-sum-again` lists before `two-sum`.

Everything here is decided on the wire so the M4 index can answer the same params later, faster,
with no client change.

## Decision

### 1. `filter`: a value, or an operator object

Each key of `filter` (a field, `id` or `status`) takes either:

- **a plain value**: exact equality, exactly as today (`null` matches unset). Every existing
  request means what it meant;
- **an object of operators**, all of which must hold:

```json
{"filter": {
  "oa_deadline": {"gte": "2026-10-05", "lte": "2026-10-10"},
  "stage": {"in": ["oa", "interview"]},
  "url": {"set": true},
  "company": {"contains": "goog"}
}}
```

| Operator | Value | Matches |
|---|---|---|
| `eq` | a value | equal: a plain value, except that it can't be `null` (use `set: false` for "unset") |
| `ne` | a value | not equal. **Unset matches**: "stage is not `rejected`" includes records with no stage yet |
| `lt` `lte` `gt` `gte` | a value | less / at most / greater / at least. Unset never matches |
| `in` | a non-empty list | equal to one of them. Unset never matches |
| `set` | `true` / `false` | the field has a value / has none (`""` counts as none, as in ADR 0017) |
| `contains` | text | the value contains it, ignoring case. Unset never matches |

**Which operators fit which field**, so a typo is an error and never a filter that silently
matches nothing:

| Type | Operators |
|---|---|
| `string` | `eq` `ne` `in` `set` `contains` |
| `int`, `date` | `eq` `ne` `lt` `lte` `gt` `gte` `in` `set` |
| `enum` | `eq` `ne` `in` `set`, and `lt` … `gte` by the order the values are listed (`stage < "interview"`) |
| `bool` | `eq` `ne` `set` |
| `id` | `eq` `ne` `in` `contains` |
| `status` | `eq` `ne` `in` |

- Every value is checked against the field's type (a date must be `YYYY-MM-DD`, an enum value
  must be one of its values). An unknown operator, an operator the type doesn't take, an empty
  object, or a bad value is `invalid_params` naming the field.
- Dates compare as dates; ints as numbers; strings and `contains` ignore case.
- A hand-edited value of the wrong type (a number in a date field) doesn't match comparisons; it
  is never an error on read (ADR 0008).

### 2. `search`: one text across a record

`"search": "google"` matches records where the text appears, ignoring case, in the id or any
`string` or `enum` field. It combines with `filter` (both must match). An empty `search` is
`invalid_params`.

### 3. `sort`

`"sort": ["oa_deadline", "-applied_on"]`: a list of keys, each a field, `id` or `status`, with a
leading `-` for descending. Records are compared by the first key, ties by the next, and finally
by id.

- **Unset values always come last**, in either direction: "soonest deadline first" shouldn't
  start with every job that has no deadline.
- ints by number, dates by date, strings ignoring case, bools `false` before `true`, **enums by
  the order their values are listed** (`stage` sorts `oa`, then `interview`, then `offer`, …:
  the order the collection's author wrote them in, which is usually the pipeline order).
- An unknown key, a repeated key, or more than 5 keys is `invalid_params`.
- **Without `sort`, records come back by id**, now truly by id: `two-sum` before `two-sum-again`.

`total` still counts every match before paging, and `limit` / `offset` page the sorted result.

### 4. CLI

```
$ shimmer records list jobs --filter "oa_deadline<=2026-10-10" --sort oa_deadline
$ shimmer records list jobs --filter "stage=oa|interview" --has url
$ shimmer records list jobs --filter "stage!=rejected" --sort -applied_on
$ shimmer records list jobs --search google
$ shimmer records list jobs --missing oa_deadline
```

| Flag | Becomes |
|---|---|
| `--FIELD VALUE` | exact match, as today |
| `--filter "FIELD=VALUE"` | `eq`; `FIELD=a\|b\|c` is `in` |
| `--filter "FIELD!=VALUE"` | `ne` |
| `--filter "FIELD<VALUE"` (`<=`, `>`, `>=`) | `lt` (`lte`, `gt`, `gte`) |
| `--filter "FIELD~TEXT"` | `contains` |
| `--has FIELD` / `--missing FIELD` | `set: true` / `set: false` |
| `--search TEXT` | `search` |
| `--sort FIELD` / `--sort -FIELD` | `sort`, repeatable, in order |

`--filter` is repeatable, and two conditions on one field combine (`--filter "oa_deadline>=2026-10-05"
--filter "oa_deadline<=2026-10-10"` is one range). Values are typed from the collection, as
`--FIELD VALUE` is today. `--last-solved` style names work here too.

## Consequences

- `crates/modules/records`: a new pure `query.rs` (parse and check a filter, match, search,
  compare for sorting) with unit tests per operator and type (§12 rule 10); `records.list` uses
  it; op tests against the in-memory `Ctx` (§12 rule 13). The exact-equality `matches` in
  `item.rs` becomes the plain-value case of the new code.
- `crates/cli`: `--filter`, `--has`, `--missing`, `--search`, `--sort` on `records list`.
- `docs/protocol.md`: the new `filter` forms, `search` and `sort`. Dev C told: the TUI can sort
  and filter its tables through the daemon instead of in the client (§2: clients compute nothing).
- Same params later from the index (M4), with no client change.

## Not done here

- **Relative dates** (`--due-within 7d`, "this week"). They need "today" in the configured
  timezone (ADR 0009), which the CLI doesn't know. A later op param such as `{"lte": "today+7"}`
  resolved by the daemon is the likely shape; left until someone needs it.
- **`or` across fields** ("stage is offer *or* priority is dream"). `in` covers the common case
  within a field.
- **Titles in the list response** (ADR 0017 "Not done here"): still left out.
- **Raising the 500 limit**: with the index (M4).
