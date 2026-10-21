# 0028. The fetchers module: sources as files, one command set, no built-ins

Status: proposed · Raised by Dev B · Needs sign-off: Dev A (daemon-side command/scheduling wiring;
one small, additive `core::filter` operator, §8) · Dev C notified (new ops, new topics,
`docs/protocol.md` additions — additive, no version bump) · `ctx.schedules` and `core::filter`
(ADR 0035 §5.3, §5.6) are already on master as of PR #297 — this rework builds directly on them,
nothing to extract or wait for

**Supersedes the amended version of this document** (20 amendments, #116–#280, covering the
original `Source`/dedup/scheduling design and every kind added since). That history is not repeated
here — `git log -p -- docs/decisions/0028-fetchers-module.md` has it — this rewrite describes the
resulting design as one document, per issue #277's own request ("against a rewritten ADR 0028 that
describes the resulting design rather than accumulating another amendment"). Everything below that
isn't called out as new is carried over unchanged from the prior design.

## Context

Issue #277 (raised against the shipped, amended version of this ADR) found that fetchers is the one
host module that doesn't follow the rest of Shimmer's own conventions:

| | records / workspaces / automations | fetchers, before this rewrite |
|---|---|---|
| Where a thing a person makes lives | its own file, through `ctx.store` | a section of global `config.toml` |
| Changing one | immediately, no restart | hand-edit `config.toml`, restart the daemon |
| Command set | new/edit/remove/restore/purge/enable/disable/rename/copy/show/list/templates/health | `run`/`list`/`status`/`test` only |
| What ships on a fresh install | nothing (ADR 0023) | two sources, compiled in, runnable with zero config |
| Adding a new kind | one new file | three: the struct, `schedule.rs`'s validator, `source.rs`'s resolver |

ADR 0038 (remove/restore/purge, naming convention) already did the one fetchers change that needed
no redesign — `fetchers.fetch` became `fetchers.run` — and explicitly deferred the rest here:
"the rest needs sources to be files, which is #277." This ADR is that rework.

## Decision

### 1. Crate and dependency shape — unchanged

Still `crates/modules/fetchers`, `core`-only. Still one registration line in
`crates/app/src/main.rs`. Nothing here changes the dependency graph `.dev/check-deps.py` enforces.

### 2. The `Source` trait — unchanged

```rust
pub enum Method { Api, Scrape }
pub struct Page { pub items: Vec<RawItem>, pub next_cursor: Option<String> }
pub struct RawItem { pub source_id: String, pub title: String, pub url: String, pub raw_data: Value }

#[async_trait]
pub trait Source: Send + Sync {
    fn id(&self) -> &str;
    fn method(&self) -> Method;
    async fn fetch_page(&self, ctx: &Ctx, cursor: Option<&str>) -> Result<Page>;
}
```

The host still loops `fetch_page` up to a fixed max-pages cap. Retry (§6 of the prior design),
scraping/robots.txt politeness (§7), the per-run item cap and first-run flood mitigation (§8) are
all unchanged and not repeated here.

### 3. The `Kind` trait — new, replaces the flat `SourceConfig`

**The problem this fixes, concretely.** Today one struct (`schedule.rs`'s `SourceConfig`) holds
every kind's fields in one flat namespace (`url`, `items_path`, `id_field`, `title_field`,
`url_field`, `url_base`, `pagination`, `page_size`, `active_field`, each commented "`json-api`
only" or similar), and two independent `match kind` blocks both need a new arm for a new kind
(`schedule.rs`'s `validate_source`, `source.rs`'s `resolve_from`). Adding a fifth kind today touches
three shared files, which is exactly the CLAUDE.md §8 violation issue #277 names.

```rust
/// One source type: parses and validates its own config, builds a `Source`. Adding a kind is
/// adding one file that implements this, registered in one place (`sources::all()`) — never an
/// edit to another kind's file.
pub trait Kind: Send + Sync {
    /// The config-file word, e.g. `"rss"`, `"json-api"`, `"hn-whoishiring"`, `"wwr"`.
    fn name(&self) -> &'static str;

    /// Parse and validate this kind's own `[kind]` table from a source file (§4). Checked at
    /// load time — a missing required field or a malformed URL is a load-time error here, never
    /// discovered lazily on first fetch, matching the prior design's own validation posture.
    fn parse(&self, params: &Value) -> Result<Box<dyn KindConfig>>;
}

/// One kind's own, already-validated settings, opaque to everything outside that kind's file.
pub trait KindConfig: Send + Sync {
    /// Build the `Source` this config describes, for one source `id`.
    fn build(&self, id: String) -> Arc<dyn Source>;
    /// Back to JSON, for `fetchers.show`/`list` to include in a source's own view.
    fn to_value(&self) -> Value;
}
```

- **Existing kinds, migrated, logic unchanged:** `hn-whoishiring` and `wwr` keep their exact
  `fetch_page` implementations (the multi-stage HN fetch, the WWR scrape-plus-robots-check) — only
  their config moves from "a few flat fields read out of `SourceConfig`" to "this kind's own
  `KindConfig` impl, parsed from its own `[kind]` table." `rss` and `json-api` migrate the same way;
  `json-api`'s existing fields (`items_path`, `id_field`, `title_field`, `url_field`, `url_base`,
  `pagination`, `page_size`, `active_field`) become that kind's own struct, validated by that kind's
  own `parse`, nobody else's.
- **A new kind is one new file plus one registration line**, the shape CLAUDE.md §8 asks for — no
  edit to `schedule.rs` or `source.rs` (those files, after this rewrite, no longer contain any
  per-kind knowledge at all; see §9).
- **Id collision, unknown kind, missing required field:** all still load-time `invalid_params`
  errors, same posture as before — just raised by the kind's own `parse` now, not a shared `match`.

### 4. Source storage — one file per source

```
data/fetchers/sources/<id>.toml
```

```toml
[source]
label = "My Team's Blog"      # optional, free text, like a workspace's or automation's label
kind = "rss"
enabled = false                # same default-disabled posture as before (§5 of the prior design)
interval = "daily"              # a preset, or an integer seconds count >= the kind's own floor

[kind]
url = "https://example.com/feed.xml"
```

`[kind]`'s shape is entirely up to that kind's `Kind::parse` — `rss` needs `url`; `json-api` needs
`url`/`items_path`/`id_field`/`title_field`/`url_field` and the rest optional; `hn-whoishiring` and
`wwr` need nothing beyond `enabled`/`interval` (and whatever filter fields land per §8). Written and
read through `ctx.store`, same atomicity guarantees as every other module's files (CLAUDE.md §7.1) —
no more `config.toml` section, no more daemon restart to pick up a change.

**Credentials (ADR 0036), wired in now rather than later.** `[kind]` for `json-api`/`rss` gains an
optional `credential = "name"` — the named `[credentials.<name>]` binding ADR 0036 already built.
`JsonApi`/`Rss`'s `fetch_page` calls `ctx.http.get_with_credential(url, credential)` instead of
`ctx.http.get(url)` when present. This is the natural moment: the `Kind` trait rework already touches
every call site that builds a request, and ADR 0036 was written and merged anticipating exactly this
caller ("nothing in `fetchers` needs to change to use this once it does" — it still doesn't *need*
to, this is additive). `hn-whoishiring`/`wwr` don't gain this field; neither reference source is
key-gated, and there is no reason to validate a field a kind never reads.

### 5. In-memory state and reload — the same shape automations already uses

```rust
struct State {
    sources: HashMap<String, Entry>,   // id -> parsed config + its Kind's Source instance
    problems: Vec<(String, String)>,   // file path -> why it doesn't parse, same as automations
}
```

Loaded once in `init()` by reading every `data/fetchers/sources/*.toml`, re-read on `fetchers.reload`
(new op, §9) and after any write (`new`/`edit`/`remove`/`restore`/`rename`/`copy`/`enable`/
`disable`). A file that fails to parse is reported as a problem (surfaced in `fetchers.list` and
`fetchers.health`, §12), not a daemon-start failure — one broken source file must not take every
other source down, the same posture automations' rule files already have.

### 6. Scheduling — per source, via `ctx.schedules`, no heartbeat

`ctx.schedules.add_once(op, params, at, label)` / `.remove(id)` (ADR 0035 §5.6, on master since
PR #297) gives every module a way to schedule a one-time run of its own, kept by the scheduler
across restarts. This replaces the fixed 5-minute `fetchers.tick` heartbeat entirely:

- **A source schedules its own next run** at three moments: when it's created enabled, when it's
  switched from disabled to enabled, and when a run of it finishes — success, failure, *or
  cancellation* (#306's review, blocking: a cancelled run must still reschedule, from the same
  basis as if it had never started, or it never fires again) — at
  `now + interval` for the first two, at `last_run_at + interval` (or `now` if that's already past)
  for the third.
- **A manual run replaces the pending schedule rather than adding to it.** `fetchers run SOURCE`
  finishing books a next run exactly like the source's own schedule firing does, through the same
  reconcile path below — so the two never coexist as two separate pending entries for one source
  (this ADR's own review, point 2: "a manual `fetchers run` finishing also books a next run while
  the scheduled one is still pending").
- **No bootstrap step is needed at daemon start, for the common case.** `ctx.schedules` entries are
  kept by the scheduler across restarts — a source with a pending schedule from before a restart
  still has it after. The only moment a source could lack one is right after it's created or
  re-enabled, and both of those already call `add_once` directly, inside the `handle()` call that
  does the creating/enabling (never inside `init`, where `ctx.schedules` answers `unavailable` until
  the scheduler has started — ADR 0035 §5.6).

  **Known gap, not closed by this rework:** a schedule that fires but whose task is never recorded
  as having run at all — the daemon killed between the trigger firing and `fetchers.run_task`
  reaching this module's `handle`, or `add_once` itself failing (e.g. the scheduler's own 1000-entry
  cap) — still leaves a source with nothing pending, forever, until some unrelated write (an edit,
  enable/disable) happens to touch it. A slow, recurring repair trigger that re-books any enabled
  source with nothing pending would close this properly; it is not built here. The one case this
  rework *does* close is a run that starts and is then cancelled (above), which is the common real
  path (`shimmer shutdown`, a daemon restart, `queue cancel` mid-fetch) — covered because the run
  actually reaches this module's code, unlike the gap above.
- **Each source's file tracks its own current schedule.** A sibling `data/fetchers/sources/<id>.state.toml`
  (written by the module, not hand-edited, same spirit as workspaces' `state.toml`) holds
  `{trigger_id, due_at}` for whatever `ctx.schedules` entry is currently pending for that source.
  **Not a source itself** — `Sources::load`'s scan of `sources/*.toml` skips any file whose name
  ends `.state.toml` before trying to parse it as one, rather than reporting every source's own
  state file as a broken source (this ADR's own review, point 3; resolved by exclusion, not by
  moving the file to its own `data/fetchers/<id>/` folder as first suggested — `seen.toml`/
  `last_run.toml` already live under `sources/`'s *sibling* per-source folders, and `<id>.state.toml`
  staying a flat sibling of `<id>.toml` keeps rename/remove/purge's existing per-id file list
  unchanged).

  This is what makes `edit`/`rename`/`disable` able to reconcile deterministically:
  - **`edit`** changing `interval`: if the new interval would move the next due time, `remove` the
    old `trigger_id` and `add_once` a new one; otherwise leave it alone.
  - **`disable`**: `remove` the pending `trigger_id`, if any, so a disabled source never fires.
  - **`rename`**: `remove` the old pending schedule and `add_once` a new one under the new id's
    params, at the same `due_at` — simpler than automations' `renamed.toml`-follows-the-chain
    mechanism (ADR 0035, PR #287), because a fetchers schedule's payload has no chain/cause history
    to carry forward; re-creating it under the new id is sufficient and exactly reproduces today's
    due time. The op fired is always `fetchers.run_task` with just `{"id": source_id}` — a scheduled
    run always uses the source's own configured filter, never an ad-hoc override, so there is
    nothing else to put in the schedule's own params.
  - **`remove`** (moved to `removed/`, not deleted): same as `disable` — nothing should fire for a
    removed source. **`restore`** re-adds a schedule the same way enabling does.
  - **Visibility, not yet addressed:** since PR #295, `scheduler list` shows every module's
    one-time schedules by default, labelled `fetchers: <id>`. Running `scheduler remove` on one of
    these silently desyncs it from the source's own `<id>.state.toml` — the source's file still
    says it's booked, `fetchers show` still reports a `last_run_at`-derived next-run time, but
    nothing will actually fire. There is no detection or repair for this yet; it is the same class
    of problem as the bootstrap gap above and could share a fix (a periodic reconciliation pass).
- **This removes the double-queue race for real**, not just its symptom. Today's heartbeat has to
  guess "is this due" on a schedule-blind poll; moving scheduling to the thing that's actually due
  (via the scheduler's own single-fire guarantee) means there's no second, independent timer that
  could ever race the first into enqueuing the same source twice. `dedup.rs`'s `RunGuard` stays as
  the defense-in-depth for the one remaining case — a person manually running `fetchers run SOURCE`
  while its own schedule is also about to fire.
- **`fetchers.tick` is deleted**, along with `TICK_OP`/`TICK_INTERVAL` and the `Module::triggers()`
  override — fetchers registers no fixed triggers at all once this lands. `fetchers.fetch.finished`/
  `.failed` keep their names (§9's topic table), only how a run gets *started* changes.

### 7. Dedup — unchanged mechanism, rename made safe

`seen.toml`/`last_run.toml` (prior §4) are unchanged in shape and pruning behavior. The one addition:
**`fetchers rename OLD NEW`** moves `data/fetchers/<OLD>/` to `data/fetchers/<NEW>/` (both `seen.toml`
and `last_run.toml`'s entry) in the same store transaction that moves the source's own config file —
so dedup history and cadence tracking survive a rename instead of silently resetting, closing the gap
#277 flagged.

### 8. Filtering — adopts `core::filter`, one small additive operator needed

Issue item 5 offered two options: a general field-condition filter (automations' `when` shape,
`core::filter`) or leaving filtering to automation rules entirely. `core::filter` is on master now
(ADR 0035 §5.3, via PR #297), so this rework takes the first option — one filter mechanism, shared
with automations, instead of fetchers' own bespoke `title_contains`/`title_excludes` pair.

**The gap found evaluating this:** `core::filter::Op` has `Contains(String)` (case-insensitive
substring) but nothing for "does not contain" — `Ne` is equality-negation, not substring-negation.
`title_contains`'s "any of these terms" maps cleanly onto `Filter`'s existing `or` combinator
(`{"or": [{"title": {"contains": "senior"}}, {"title": {"contains": "staff"}}]}`), but
`title_excludes`'s "none of these terms" has no equivalent without a new operator.

**Decision: add `Op::NotContains`, a tenth operator, alongside the existing nine** (`eq, ne, lt,
lte, gt, gte, in, set, contains, has`). Small and additive — `core::filter::Op::parse`'s `match`
gains one arm (`"not_contains" => Op::NotContains(s.to_lowercase())`), `Op::holds` gains one arm
(the negation of `Contains`'s own check), and records (the other consumer of `core::filter::Op`,
via its own schema-driven `Field`/`query.rs`) is unaffected unless a collection's own schema opts a
field into it — this is purely an added capability, no existing behavior changes. Needs Dev A
sign-off per CLAUDE.md §4 since it touches `crates/core`, but it's the smallest possible version of
that change: one operator, no new combinator, no change to `Filter::parse`'s structure.

**A source's filter**, in its file's `[source]` table:

```toml
[source]
filter = { or = [
  { title = { contains = "senior" } },
  { title = { contains = "staff" } },
], title = { not_contains = "intern" } }
```

(`Filter`'s existing dotted-key/AND-of-conditions shape means a bare `title = {...}` alongside `or`
is already "AND this with whatever the `or` block decided" — no new combinator needed for excludes
to coexist with an inclusion list.) Applied once, in the same place today's `title_contains`/
`title_excludes` check runs (`Fetchers::run_source`/`Fetchers::test`), against `RawItem::title`
only, same as before — `Filter` itself is schema-free, so no per-kind wiring needed.

**`Filter`'s own schema-free-ness is exactly the gap this ADR's own review caught (point 5,
echoed again as #311's review, blocking)**: because it has no
schema behind it, a filter naming any key but `title` (e.g. `company = { contains = "Acme" }`)
parses as a perfectly well-formed `Filter` and then matches nothing, forever, once it is checked
against `{"title": …}` only — with no error, just a source that silently never keeps anything.
Closed by checking the raw filter JSON at load (and on the ad-hoc `--filter` override below) before
`Filter::parse`: every key must be `title`, including inside each `"or"` alternative, or the filter
is refused with a message naming the bad key. This is the one schema `fetchers`' filter has, and it
is enforced here rather than in `core::filter` itself, since `Filter` is deliberately schema-free
for every other caller (automations' `when`, records).

The ad-hoc `--contains`/`--excludes` override on `fetchers test`/`run` (prior §14) becomes a
`--filter` flag taking the same JSON shape inline, replacing — never merging with — the source's own
configured `filter` for that one call, same override semantics as before.

### 9. Commands — the full set, named by CLAUDE.md §15 / ADR 0038's convention

| Op | Execution | Params | Notes |
|---|---|---|---|
| `fetchers.list` | inline | `{"removed"?}` | Every source (or, with `removed`, every removed one): id, label, kind, enabled, interval, last run, running, last error. Merges today's `list` + `status` into one per-source table (issue comment #1). |
| `fetchers.show` | inline | `{"id"}` | One source in full: its config, last result **or error**, next scheduled run. Was `fetchers.status` for a single id; that word now means the feature-wide view below. |
| `fetchers.status` | inline | `{}` | The whole feature at a glance: source count, how many enabled/disabled/broken, which are failing, soonest next runs. Not per-source — matches `automations.status`'s shape. |
| `fetchers.new` | inline | `{"id","label"?,"kind","params"}` or `{"id","template","values","preview"?}` | Validated by that kind's `Kind::parse` before any file is written; a template path follows ADR 0025 §6a (required questions, a preview, never partial). |
| `fetchers.edit` | inline | `{"id","text"}` | **Amended by #305's review, blocking.** Not a structured shape like `fetchers.new`'s: the *whole* replacement file text, same as `automations.edit`. A structured shape meant re-passing every field just to change one, with whatever wasn't re-passed silently reset to its default — most visibly, an edit that didn't repeat `enabled` would switch a running source off. Taking the full text instead means the CLI reads the current file first (`fetchers.show`) and opens `$EDITOR` on a copy of it, so nothing not explicitly retyped changes. Re-validated the same way as `new`; a refused edit changes nothing (old file kept), same posture as automations' `save`. |
| `fetchers.reload` | inline | `{}` | Re-reads every `data/fetchers/sources/*.toml` (§5) — needed after hand-editing a source file, since the module otherwise only reads them at daemon start. Omitted from an earlier draft of this table despite being built in §5; listed here for real. |
| `fetchers.remove` | inline | `{"id"}` | Moves to `removed/`; cancels its pending schedule (§6). |
| `fetchers.restore` | inline | `{"id"}` | Brings it back; re-schedules if enabled. |
| `fetchers.purge` | inline | `{"id"?}` | Confirmed with `{"sources": N}`, same shape as `records.purge`/`automations.purge`. |
| `fetchers.enable` / `.disable` | inline | `{"id"}` | Schedules or cancels (§6). A source shows `enabled` in `list`/`show`. |
| `fetchers.rename` | inline | `{"id","to"}` | Moves the config file, `data/fetchers/<id>/`, and the pending schedule (§6, §7). |
| `fetchers.copy` | inline | `{"id","to","label"?}` | Starts disabled, same reasoning as an automation's copy (one event must not fire both before the copy is reviewed). |
| `fetchers.templates` / `.peek` | inline | `{}` / `{"id"}` | The five starter templates (§10), same shape as every other module's. |
| `fetchers.test` | queued (`fetchers`) | `{"id","filter"?}` | One page, no state touched, no events emitted. `filter` (a `core::filter::Filter`, §8) overrides the source's own for this call only. |
| `fetchers.run` | queued (`fetchers`) | `{"id","filter"?}` | Checks the source exists **before** returning a task id (fixes the "says queued, then fails silently" bug from issue comment #3), the same inline-existence-check pattern `workspaces.activate` already uses. |
| `fetchers.health` | inline | `{}` | New (issue comment #1). Broken source files (parse problems, §5) and sources whose last run failed, each reported once until fixed — not every run — discovered generically by `shimmer doctor` via `core.manifest`, same as records/workspaces already are. |
| `fetchers.items` | inline | `{"id"?,"limit"?}` | New — answers what the CLI's `fetchers items` used to compute itself (issue item 6): matches by source and reuses the same title-filter rule `fetchers.run` already applies, in one place. The CLI command becomes a thin call-and-render, like every other inline op. |

Every op's CLI word is its own verb (ADR 0038's rule), gets an alias in all four built-in packs, Tab
completion, and help text. Topics follow `fetchers.<thing>.<past-tense verb>` for everything new:
`fetchers.source.created` / `.edited` / `.removed` / `.restored` / `.purged` / `.renamed` /
`.copied` / `.enabled` / `.disabled`. `fetchers.item.found` and `fetchers.fetch.finished`/`.failed`
are unchanged.

### 10. No built-in sources — templates instead

`source::registry()`/`find()` (the fixed, compiled-in list) are deleted outright. `hn-whoishiring`
and `wwr` become ordinary `Kind` impls (§3) — real Rust code stays exactly as real Rust code, same
fetch logic, same `Method`; only the "exists with zero config" shortcut goes. A fresh install's
`fetchers list` shows nothing, with a hint pointing at `fetchers templates`, matching ADR 0023's "no
seeding" rule every other module already follows.

Five starter templates (ADR 0025 §6a's rules: required questions, a preview, an end-to-end test
each):

| Template | `kind` it writes | Questions |
|---|---|---|
| Hacker News "Who is hiring?" | `hn-whoishiring` | title filters (optional) |
| We Work Remotely | `wwr` | category, title filters (optional) |
| A Greenhouse job board | `json-api` | the company's board token |
| A Lever job board | `json-api` | the company's id |
| An RSS / Atom feed | `rss` | the feed URL |

A template is data (questions plus a filled-in `[kind]` table), never code — the same "packs are
data" posture CLAUDE.md §15.1 already holds command packs to. This also answers the forward-looking
question of a later template marketplace cleanly: a third-party template can only ever name a `kind`
this binary already ships (so `rss`/`json-api` templates — a new feed, a new board — are exactly
what a marketplace contributor can add with zero Rust); a genuinely new fetch mechanism still needs
a `Kind` impl shipped with Shimmer itself, same as `hn-whoishiring`/`wwr` today. Nothing in this ADR
builds a marketplace; this is just confirming the design doesn't foreclose one.

### 11. Health — `fetchers.health`

Covered in §9's table. Two kinds of problem, both reported once until resolved rather than on every
`shimmer doctor` run: a source file that doesn't parse (§5's `problems`), and a source whose most
recent run failed (tracked alongside `last_run.toml`, carrying the failure's `reason` from
`fetchers.fetch.failed`'s existing payload shape — no new failure taxonomy needed).

## Migration

**No automatic migration from `config.toml`.** Consistent with ADR 0038's own posture for its op
renames ("nothing depends on the old names yet... so they're renamed outright rather than kept as
second names") — no dogfooded installs or outside clients depend on today's `[modules.fetchers.sources.*]`
shape either, **as of this ADR being written**; that stops being true the moment anyone starts
dogfooding against the pre-rework shape, so this whole rework should land before dogfooding does,
not be left to trail behind it. Upgrading means re-creating each source by hand (`fetchers new` or a template);
`config.toml`'s `[modules.fetchers]` section becomes unrecognized and is left alone (unknown
sections are already ignored, not rejected). Worth a line in the next release's notes, not a code
change.

## Not done here

- A TUI surface — out of scope, as always.
- Headless-browser scraping, the `html-list` kind — still undone, unrelated to this rework.
- A template marketplace — §10 only confirms the design doesn't block one later.
- Tier-2/3 credentials (session cookies, automated login) — ADR 0036's own deferral, unchanged here.
- **A fresh install's zero-sources state, filled in by `shimmer setup`, not here.** §10 deletes the
  fixed `hn-whoishiring`/`wwr` registry on purpose (ADR 0023's "no seeding" rule), so a brand-new
  install genuinely has nothing until someone runs `fetchers new --from TEMPLATE`. The planned
  `shimmer setup` onboarding kit is where a first-run prompt (or a default profile) would offer to
  make the HN/WWR sources from their templates, if that is wanted — this ADR only makes that
  possible, it doesn't build the prompt.
- The scheduling repair sweep and the `scheduler list`/`remove` desync (§6) — both flagged there as
  known gaps this rework does not close.

## Sequencing

Landed as separate PRs, in this order, each independently reviewable. Nothing here is blocked on
anything outside this module anymore — `ctx.schedules` and `core::filter` are already on master.

1. **`Op::NotContains`** (§8) — the one small `core` addition, on its own so it gets focused
   sign-off independent of everything fetchers-specific.
2. **The `Kind` trait** (§3) — internal refactor, `SourceConfig`'s fields redistributed into each
   kind's own `KindConfig`, `schedule.rs`/`source.rs` lose all per-kind knowledge. No behavior change,
   no new ops, no storage change yet — config stays in `config.toml` for this step only, so the diff
   is reviewable on its own terms before storage moves too.
3. **File storage** (§4, §5) — sources move to `data/fetchers/sources/<id>.toml`; `init()`/`reload`
   read them; no new CLI commands yet beyond what's needed to exercise this (tests write files
   directly).
4. **The command set** (§9) — `new`/`edit`/`remove`/`restore`/`purge`/`list --removed`/`enable`/
   `disable`/`rename`/`copy`/`show`, plus the existing-bug fix on `run`'s inline existence check.
5. **Scheduling** (§6) — the heartbeat removed, per-source `ctx.schedules` wired in.
6. **Filtering** (§8) — a source's `filter` field replaces `title_contains`/`title_excludes`;
   depends on #1 and #2.
7. **Templates** (§10) — the five starters, the built-in registry deleted.
8. **`fetchers.health` and `fetchers.items`** (§9, §11) — can land any time after #4; independent of
   each other.
9. **Credentials** (§4's addendum) — can land any time after #2 wires `json-api`/`rss` as `Kind`
   impls; small enough to fold into #2 or ship separately.

Steps 2-9 all depend on the Kind trait landing first except #1, which has no dependency at all and
can land immediately.
