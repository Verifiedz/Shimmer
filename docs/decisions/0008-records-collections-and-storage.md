# 0008. Records: where collections live, the file layout, and the ops

Status: accepted (M1, #2) · Raised by Dev B · Signed off: Dev A (changes a path in CLAUDE.md §7) ·
Dev C notified (item shape on the wire)

> **Renamed since:** `swe`, `swe-*` and `SWE_*` in this ADR are now `shimmer`, `shimmer-*` and
> `SHIMMER_*` (ADR 0011). The text below is kept as written.

## Context

CLAUDE.md §7 puts collection definitions at `$SWE_HOME/collections/*.toml`, but §5 gives a module
only `ctx.store`, which cannot read outside `data/<namespace>/` (`validate_path`, ADR 0001). So
`records` could not read its own collections. §8 fixes what a collection is (fields and views as
TOML, "data, not logic") but not how records are stored, what "complete" does, or which ops exist.

## Decision

**Collections live in the module's namespace:** `data/records/collections/<id>.toml`. No change to
`core`: a module owning its own data is exactly what the namespace is for. CLAUDE.md §7 is updated
to match. The file name is the collection id and must equal `[collection] id`.

**Collection format** (§8's example, plus two keys):

* `[[field]]` has `name`, `type` (`string` | `int` | `bool` | `date` | `enum`), `values` (enum
  only), and optional `required`. `id` and `status` are reserved field names.
* `[collection] stamp_on_complete = "<date field>"` names the field `records.complete` sets to
  today. This keeps "what completing means" in data: LeetCode stamps `last_solved`, a job tracker
  could stamp `applied_on`, and one with no stamp just changes status.
* `[[view]]` tables are passed through to clients untouched. No view type exists in Rust (§5, §12
  rule 9).

A malformed collection file fails only the requests that use it (`invalid_params`, naming the file),
never the module's `init`. `records.collections` lists every other collection and names each malformed
file under `"skipped"`, the same way `records.list` reports unreadable record files (added after
issue #29: the op used to fail as a whole, and the CLI calls it before every command).

**Records are one flat TOML file each:** `data/records/items/<collection>/<id>.toml`, holding
`status` (`todo` | `done`) and the field values. The id is the file name, so it is limited to
`[a-z0-9][a-z0-9_-]*`, at most 128 characters. Dates are stored as `"YYYY-MM-DD"` strings; a native
TOML date typed by hand reads the same. An unset field is simply absent.

**Items on the wire are flat**, matching the `records.list` fixture in `crates/mockd/fixtures/core.json`:
`{"id", "status", <every schema field>}`, with unset fields as `null`. Keys a hand-edit added that the
schema does not know are passed through.

**Ops** (all inline; nothing here is slow enough to queue):

| Op | Params | Returns |
|---|---|---|
| `records.collections` | `{}` | `{"collections": [...]}`: each definition as JSON (+ `"skipped"` if any file was malformed) |
| `records.add` | `{"collection", "id", "fields"?}` | the new item; `conflict` if the id exists |
| `records.get` | `{"collection", "id"}` | the item |
| `records.list` | `{"collection", "filter"?, "limit"?, "offset"?}` | `{"items", "total"}` (+ `"skipped"` if any file was unreadable) |
| `records.update` | `{"collection", "id", "fields"}` | the item; a `null` value unsets a field |
| `records.complete` | `{"collection", "id"}` | the item, now `done`, with its stamp field set |
| `records.remove` | `{"collection", "id"}` | `{"removed": true}` |

* `filter` is exact equality on `id`, `status` or any schema field; `null` matches unset. An unknown
  key is `invalid_params`, so a typo never silently matches everything. Results are ordered by id;
  `limit` defaults to 50 (max 500), `total` counts every match before paging.
* Completing a `done` item again re-stamps it and emits another `records.item.completed`: solving a
  problem twice is two solves, which is what a heatmap wants.
* "Today" is the UTC date from `ctx.clock`, the same UTC-only limitation as the scheduler (ADR 0004).

**Events**, each committed in the same store transaction as its file write (§7.1):
`records.item.created` / `.updated` / `.completed` carry `{collection, id, item}` with the full wire
item, so the index can be rebuilt from the log alone (M4); `records.item.removed` carries
`{collection, id}`; `records.collection.created` carries `{collection}`.

**Seeding.** On `init`, if no collection exists, the module writes the built-in LeetCode collection,
so a fresh install has something to track. Deleting every collection brings it back on the next start;
deleting only LeetCode does not.

**Concurrency.** Inline requests run concurrently, so the module serialises its own read-check-write
sequences with a lock. Two `records.add` calls for the same id cannot both succeed.

## Not done here

* `records.reindex` appeared in the mock fixtures despite never being a real op — the index
  belongs to the daemon and a module cannot reach it (§5). Removed from the fixtures and from
  `docs/protocol.md`'s manifest example (mockd-protocol-records-ops branch); still left out of
  `records` itself until M4 says what a module-level reindex means.
* `records.list` reads every file in the collection. Fine at M1 scale; the index should serve it
  once one exists.
* `docs/protocol.md` now has the ops table above under "Records ops" (approved for that edit
  despite §4); kept here too since this ADR is still the source for the design rationale.
