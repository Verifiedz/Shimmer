# 0028. The fetchers module: `Source`, scheduling, dedup

Status: proposed · Raised by Dev A (taking over the `fetchers` module from Dev B's territory
per handoff) · Needs sign-off: Dev A · Dev C notified (protocol change: a new op, two payload
shapes — see §9) · No `core` change · Amended (issue #127, source kinds): §2's `Source::id()`
signature and the registry's construction model change — see §2a. Still no `core` change and
no `docs/protocol.md` change — `fetchers.item.found`'s payload shape (§3) is unaffected.
Amended (issue #128, CLI surface): the "Not done here" CLI/TUI deferral below is only ever
true for the TUI now — see §11. Three additive ops, `docs/protocol.md` updated, no `core`
change, no version bump (additive, same as §9's own ops).
Amended (`shimmer fetchers items`): §11's own deferred `items` command, built — see §12.
One new, module-agnostic op (`core.events.list`), `docs/protocol.md` updated, no version
bump, no `crates/proto` *type* change beyond the new op's own response struct (`Event`
itself, already shared and already serializable, is unchanged).
Amended (per-source title filtering): `title_contains`/`title_excludes` on every source's
config entry — see §13. No `core`/`proto` change, no new op; `SourceConfig` and
`fetchers.list`'s existing payload only.
Amended (ad-hoc filter override): `--contains`/`--excludes` on `shimmer fetchers test`/`run`
— see §14. No `core`/`proto` change; two new optional params on two existing ops.
Amended (issue #127's own follow-up): the `json-api` kind, named in §2a and never built
until now — see §15. No `core`/`proto` change, no new op.
Amended (configurable search term): `jobbank-canada`'s query is no longer a compiled-in
constant — see §16. No `core`/`proto` change, no new op; one new `SourceConfig` field.
Amended (ad-hoc search term override): `--search-term` on `shimmer fetchers test`/`run`
— see §17. No `core`/`proto` change; one new optional param on two existing ops.

## Context

`fetchers` is the last unbuilt host module from CLAUDE.md's M6 scope (CLAUDE.md:831). ADR
0027 (`docs/decisions/0027-real-http-gateway.md`, merged via PR #109) already built the part
of this that used to be fetchers' own job — rate limiting, caching, redirects, cooldown —
behind `ctx.http`. What ADR 0027's own "Dev B's side" section left for this module: `Source`,
scheduling and dedup (`docs/decisions/0027-real-http-gateway.md` §"Dev B's side").

Everything here lives inside `crates/modules/fetchers`, `core`-only, because
`.dev/check-deps.py` grants a `module`-role crate a dependency on `core` alone and bans
module-to-module dependencies outright (`.dev/check-deps.py:27-36`) — there is nowhere else
in the dependency graph for a `Source` impl or a scraping helper to live.

## Decision

### 1. Crate and dependency shape

`crates/modules/fetchers`, one new workspace member, `core`-only. One registration line in
`crates/app/src/main.rs:95` (`Arc::new(shimmer_fetchers::Fetchers::default())`), the same
shape the two existing modules already use there.

### 2. The `Source` trait — paginated, in-crate, method-tagged

```rust
pub enum Method { Api, Scrape }

pub struct Page { pub items: Vec<RawItem>, pub next_cursor: Option<String> }

pub struct RawItem {
    pub source_id: String,   // dedup key (§4) -- never a content hash
    pub title: String,
    pub url: String,
    pub raw_data: Value,     // capped, see §3
}

#[async_trait]
pub trait Source: Send + Sync {
    fn id(&self) -> &'static str;
    fn method(&self) -> Method;
    async fn fetch_page(&self, ctx: &Ctx, cursor: Option<&str>) -> Result<Page>;
}
```

The host loops `fetch_page` up to a fixed max-pages cap, following `next_cursor` until it's
`None` or the cap is hit. One trait, no API-then-scrape fallback inside a single `Source` —
a source that genuinely wants both is two `Source` impls, each ~40 lines, matching CLAUDE.md
§8's "one `impl Source` of about forty lines."

### 2a. Amendment (#127): source kinds and a config-driven registry

**Why.** §2's own answer to wanting a variant — "two `Source` impls" — is fine for "API vs.
scrape of the same site," but doesn't scale to "watch five RSS feeds" or "two job-board
searches for two companies." Raised in review of the fetchers PR stack (#116–#126); full
reasoning in issue #127.

**Trait change.** `Source::id` stops returning a `'static` string:

```rust
pub trait Source: Send + Sync {
    fn id(&self) -> &str;    // was: &'static str
    fn method(&self) -> Method;
    async fn fetch_page(&self, ctx: &Ctx, cursor: Option<&str>) -> Result<Page>;
}
```

A runtime-built instance can't hand back a `&'static str` for an id it only learns from
`config.toml`. `hn-whoishiring`/`weworkremotely` need only this mechanical signature change
— each still returns its existing literal, which borrows fine as `&str` — nothing else about
either one moves. No `core` change: `Source` lives entirely in `crates/modules/fetchers`.

**Config change.** `SourceConfig` (§5) gains an optional `kind`:

```toml
[modules.fetchers.sources.hn-whoishiring]
enabled = true
interval = "daily"            # no `kind` -- this id must already exist in the fixed,
                               # compiled-in registry (unchanged, today's behavior)

[modules.fetchers.sources.my-team-blog]
kind = "rss"                  # `kind` present -- the registry builds a brand-new
enabled = true                # instance of this kind, with this id and this entry's
interval = "hourly"           # own settings, instead of looking up a built-in
url = "https://example.com/feed.xml"
```

`kind` absent means today's path exactly: the id must resolve against the fixed, compiled-in
list (`hn-whoishiring`, `weworkremotely`), checked at load time (already shipped: a
misspelled or unknown id with no `kind` is rejected at `FetchersConfig::from_value`, not left
to fail `not_found` on the first tick). `kind` present means a new instance, validated the
same way — a kind's own required fields (e.g. `rss`'s `url`) must be checked at load time
too, not discovered lazily on first fetch.

**Registry becomes two-layered**, not a single flat list:

1. The fixed, compiled-in sources (`hn-whoishiring`, `weworkremotely`) — unchanged, always
   present, no `kind` needed, exactly as §2/§10 describe them today.
2. A config-driven layer: for every `[modules.fetchers.sources.<id>]` entry that has a
   `kind`, dispatch on that string (`"rss"` → build an `Rss` instance; any other value is
   rejected at load time, same as an unknown interval preset) to construct a `Source` with
   this entry's id and params.

**Id collision is rejected, not shadowed.** A config entry whose `kind` is set and whose id
equals one of the fixed, compiled-in ids (e.g. `[modules.fetchers.sources.hn-whoishiring]`
with `kind = "rss"`) is a load-time error, the same `invalid_params` path as an unknown
`kind` or a missing required field — never a silent shadow of the built-in. The two layers
share one id namespace; a `kind`-bearing entry only ever *adds* an id, it never overrides
one the fixed layer already owns. (Validation code lands in the sibling PR, #131, not here.)

This needs no `core` change — the registry is built where `fetch()` already holds a `Ctx`,
not inside `Module::triggers()` (§5's finding about `triggers()` having no config access is
unaffected and unrelated to this).

**The `rss` kind (issue #127, first batch — `json-api`/`html-list` are a later batch, not
decided here):**

- Settings beyond `enabled`/`interval`/`kind`: one required field, `url` — the feed's URL,
  RSS or Atom. Checked at load time per the same "a kind's own required fields are checked
  at load time" rule stated above, and checked as more than "non-empty": `url` must parse
  as a well-formed `http://` or `https://` URL. Empty, malformed, or a non-`http(s)` scheme
  (`ftp://`, `file://`, etc.) is a load-time `invalid_params` error, same as a missing
  `url` entirely. (Validation code lands in #131, not here.)
- Parsing via **`feed-rs` 3.0.0** (MIT, pure Rust — no `-sys` dependency in its tree
  (`chrono`, `mediatype`, `quick-xml`, `regex`, `serde`, `serde_json`, `siphasher`, `url`,
  `uuid`, `ammonia`) — handles RSS 0.x/1.0/2.0 and Atom from one parse call, 2.4M downloads,
  last published 2026-09-27 — verified live against crates.io on 2026-10-07, same bar §7
  already held `scraper`/`texting_robots` to.
- `Method::Api`, not `Scrape` — a feed URL is a known, fetcher-maintained endpoint the same
  way `hn-whoishiring`'s Firebase API is (§10); it is not a crawled site in the robots.txt
  sense, so no robots.txt check applies.
- Dedup key (`RawItem::source_id`): the entry's own `id`/`guid` field when the feed sets one,
  else its `link` URL. Stated now so this isn't invented differently mid-implementation.
  **Caveat found during #131's implementation:** `feed-rs` does not leave an entry's `id`
  empty when the underlying feed omits one — it synthesizes a non-empty id itself (a hash
  of link+title, or, when an entry has neither, a **random UUID that changes on every
  parse**). Taking `feed-rs`'s id at face value would make such an entry look "new" on
  every single run, defeating the entire point of dedup. So the `rss` kind must not use
  `feed-rs`'s default id generation: it overrides it with its own `id_generator`
  (`feed_rs::parser::Builder` supports supplying one) so the fallback is deterministic —
  always the entry's own first `link` when there is no feed-supplied id, never a random
  value — and an entry that still has neither a usable id nor a link is skipped rather than
  ever emitted with an unstable key. (Implemented in #131, not here.)
- Pagination: out of scope for a feed. `fetch_page` returns every entry the feed has in one
  `Page` with `next_cursor: None`, always a single page.

**Orphaned per-source state on removal is fine, left as-is.** When a configured `kind`
entry is later deleted from `config.toml`, its `data/fetchers/<id>/seen.toml` and its entry
in `last_run.toml` are left behind on disk, unreferenced by anything. This is expected and
acceptable, not a leak to fix: CLAUDE.md's "SQLite is disposable" philosophy (§1.4) doesn't
apply to these — they're files, not the derived index — but the underlying principle does:
unreferenced state just sits unused, it does not corrupt anything or affect a source that
is still configured. No automatic cleanup is done, and none is proposed here; a user who
cares can delete the directory/entry by hand, consistent with "files the user could edit by
hand" (CLAUDE.md §1.4).

**Not done in this amendment:**

- The `json-api` and `html-list` kinds — a later batch (issue #127, umbrella PR's own
  done-when). `json-api` is built later, in §15; `html-list` remains undone.
- Migrating `hn-whoishiring`/`weworkremotely` onto the new kind system — they stay hand-written
  `Source` impls exactly as today; only `id()`'s return type changes.
- §7's per-method rate-limit floor (currently a uniform, provisional 900s) — a kind-aware
  registry could eventually expose each instance's `Method` to the scheduling layer and make
  this precise, but that's not decided or built here.
- CLI/TUI surface — out of scope for this amendment specifically; the CLI half later shipped
  separately (issue #128) — see §11. TUI is still untouched.

### 3. Item event

`fetchers.item.found` payload: `{source_name, source_id, title, url, raw_data}`. `raw_data`
capped at 8 KiB — enough for a job posting's text, small enough that one large item can't
turn into the next cache-bloat problem. This is a protocol.md addition (§9).

### 4. Dedup

`data/fetchers/<source>/seen.toml`: a map `id -> last_seen` (RFC3339), through `ctx.store`,
never the index (CLAUDE.md §1.4/§7 — nothing that isn't a file is truth). One
`ctx.store.transaction` per **run**, not per item: the final `seen.toml` is written once and
`tx.emit("fetchers.item.found", ...)` is called once per new item inside that same
transaction — the pattern `records::import` already uses for "many writes, many events, one
transaction" (`crates/modules/records/src/lib.rs:864-872`). This makes "a failed or partial
run never prunes" free: nothing commits until the whole run's result is known.

Pruning: at the end of a successful run, drop `seen.toml` entries whose `last_seen` is older
than 90 days. Per-run cap: at most 50 new items processed (and thus emitted) per run; items
beyond the cap are left unmarked and picked up by a later run — see §7.

**Amendment (#126 review): a kid skipped before fetching is not touched.** §8's fix for HN
getting stuck at comment 200 reads this source's own `SeenSet` *inside* `fetch_page`
(`crate::dedup::load`, read-only) to skip an already-seen kid with zero HTTP calls, before
the orchestration above ever sees it — so that kid's `last_seen` is not refreshed on that
run, unlike every item this orchestration actually examines, which this section's own
"touch everything examined" rule covers. In practice this is harmless here: HN's
"who is hiring" threads are monthly and don't live anywhere near the 90-day pruning window
regardless of how many runs pass without a given old comment being re-examined. Noted so a
future reader isn't surprised that `last_seen` on an old, already-dedup'd item can lag.

### 5. Scheduling

**Finding:** `Module::triggers()` is `fn triggers(&self) -> Vec<TriggerSpec>` — synchronous,
`&self`-only (`crates/core/src/module.rs:25-27`) — called once at registry-build time,
*before* `Config::load` and *before* `Module::init(&ctx)` run
(`crates/daemon/src/lib.rs:95-96,98-102,120`). Module construction is also config-blind
(`crates/app/src/main.rs:95`, plain `::default()`). So a literal "one `TriggerSpec` per
*configured* source" is not implementable: nothing at the point `triggers()` runs has seen
`config.toml` yet. This is reported, not worked around by changing `core` — no `core` edit is
in this ADR.

**Decision:** one fixed `TriggerSpec` — a heartbeat, needing no config to exist:
`op: "fetchers.tick"`, `lane: "fetchers"`, `catch_up: Skip`, default period 300s. Its handler
(which gets a full `Ctx`, including `ctx.config`) reads `[modules.fetchers]` on every firing
and decides which sources are due, tracking each source's own `last_run_at` itself in
`ctx.store` — not the scheduler's per-trigger bookkeeping. This gives correct catch-up
behaviour at the *source* level for free: a "daily" source whose last run was 3 days ago
(daemon asleep) is simply overdue whenever a tick notices it, however many ticks were
skipped getting there. `fetchers.tick` is a protocol.md addition (§9) — discoverable like
any op via `core.manifest`, even though only the scheduler calls it in practice.

Config lives in the already-generic `[modules.fetchers]` section (plain JSON via
`ModuleConfig`, `crates/core/src/ctx.rs:30-51`) — unlike `[http]`
(`crates/daemon/src/config.rs:97-99`), which the daemon parses specially because the real
`HttpBackend` must exist before any `Ctx`, module config needs no daemon change at all:

```toml
[modules.fetchers]
tick_interval_s = 300

[modules.fetchers.sources.hn-whoishiring]
enabled = true
interval = "daily"        # a preset, or an integer seconds count >= the method's floor

[modules.fetchers.sources.weworkremotely]
enabled = false
interval = "daily"
```

Presets: `hourly` = 3600, `daily` = 86400, `weekly` = 604800. Floors (§7): 1800s for `Api`,
21600s for `Scrape`. Sources default **disabled**. Overlap guard: an in-process
`Mutex<HashSet<SourceId>>` on the `Fetchers` struct — the same shape as `records`'
`WriteLock` (`crates/core/src/sync.rs:13-20`), but keyed. `fetchers.fetch`'s handler tries to
insert the source id and fails fast (`module_error`) if another run (tick-triggered or
manual) already holds it; the tick handler additionally skips enqueuing a source it already
sees as running, as an optimization only, not the actual guarantee.

### 6. Retry

`ctx.http.get` returns `Ok(HttpResponse)` for *every* in-band HTTP status, including
4xx/429/503 (`crates/core/src/http.rs:14-26`; `crates/daemon/src/http_backend.rs`'s `finish`
and cooldown paths never return `Err` for a status code) — only a transport-level failure is
`Err(unavailable)`, and only `Unavailable` is retryable
(`crates/core/src/error.rs:98-99`). So the module's retry wrapper branches on `resp.status`
itself:

```rust
let resp = ctx.retry_with_backoff(RetryPolicy::standard(), |_| async {
    let r = ctx.http.get(&url).await?;
    if r.status >= 500 { return Err(Error::unavailable(format!("http {}", r.status))); }
    Ok(r)
}).await?;
```

A `retry_after_secs` on the result means the gateway has this host in cooldown: stop this
source's run now, no further attempt, let the next tick try again. A `4xx` is a terminal
failure for this run (`reason: "http_error"`, no retry). `RetryPolicy::standard()` (3
attempts, 1s/2s, `crates/core/src/retry.rs:15-17`) is already documented as "what a fetcher
wants" — reused as-is.

### 7. Scraping and politeness

A shared parse helper inside the crate, built on **`scraper` 0.27.0** (pure Rust, ISC
license, 31M+ downloads, actively maintained, no system/C dependency — verified live against
crates.io; builds identically on `ubuntu-latest` and `macos-latest`,
`.github/workflows/ci.yml:10`). `select_or_fail(doc, selector, what)` returns
`Err(module_error)` when a selector matches zero elements on an otherwise-200 response — a
distinct `reason: "parse_empty"` in `fetchers.fetch.failed` (§9), so a source's health is
visible instead of silently reporting "0 new items" the same way a genuinely quiet board
would. JS-rendered pages are out of scope; a headless-browser subprocess is a follow-up (its
own ADR — §12 rule 8 bans raw process spawning outside `Launcher`, itself gated to the
`"process"` capability only `workspaces` uses today).

Politeness lives in **fetchers**, not the gateway: robots.txt is a crawling-ethics judgment
tied to "is this traversing a site's pages," which only a `Source` can know, and ADR 0027 is
already merged scoped to rate-limit/cache/redirect only. Applies to `Method::Scrape` sources
only, fetched once through the existing `ctx.http` (so it gets the same cache/rate-limit
treatment as everything else) via **`texting_robots` 0.2.2** (pure Rust, zero dependencies,
dual MIT/Apache-2.0 — verified live, more recently touched than the alternative `robotstxt`
crate). A `Disallow` match is a hard refusal, never a soft warning. User-Agent identifies the
bot, never spoofs a browser.
Scrape sources get the longer config floor from §5 as their "more conservative default
interval."

**Amendment (#126 review): the User-Agent is daemon-wide, not fetchers-specific.** The
original text above promised `Shimmer-Fetchers/<version>`, specific to this module. In
practice the UA is set once on `RealHttpBackend`'s shared `reqwest::Client`
(`crates/daemon/src/http_backend.rs`), which every network-capable module uses — there is no
per-module custom-header plumbing (ADR 0027 deferred it). The actual, current value is
`Shimmer/<version>`, and `robots.rs`'s own `USER_AGENT` constant (used only to *check*
robots.txt rules) is kept identical to it on purpose, so "what we check" and "what we send"
never drift apart again.

### 8. First-run flood

The per-run item cap (§4) is the whole mitigation: a board with hundreds of unseen items on
first contact surfaces only the cap's worth per run; the rest stay unmarked and are picked
up on later ticks, spreading both the event volume (against the 1024-event per-connection
backpressure queue, `docs/protocol.md:175`) and the event-log growth across time instead of
one burst. Not hypothetical: the reference HN source (§9) can have 500+ top-level comments
on one month's thread, each needing its own fetch — at `ctx.http`'s default 2 req/s per
host, an uncapped cold-start run would take minutes before the event-queue angle is even
considered.

### 9. Protocol changes needed (additive, Dev C notified)

- New op `fetchers.tick` (§5) — scheduler-only in practice, discoverable like any op.
- `fetchers.item.found`'s actual payload (§3) — the existing illustrative example
  (`docs/protocol.md:550`, `{"source":"hn-whoishiring","id":"c-41883"}`) is superseded by
  `{source_name, source_id, title, url, raw_data}`.
- `fetchers.fetch.failed`'s payload gains a `reason` enum:
  `"network" | "http_error" | "parse_empty" | "cancelled"`, plus `{source, error, attempt}`.

### 10. Reference sources

Both public, no login, no API key, verified live on 2026-10-06:

- **`hn-whoishiring`** (`Method::Api`). Hacker News's official, public, unauthenticated
  Firebase API (`hacker-news.firebaseio.com`). No robots.txt applies — it's an API host, not
  a crawled site, and the API is explicitly meant for third-party clients. Flow:
  `user/whoishiring.json` for submitted story ids → the latest "Who is hiring?" thread →
  `item/<thread>.json`'s `kids` as the paginated item list → one `item/<id>.json` fetch per
  comment. HN states no hard rate limit beyond "be reasonable"; `ctx.http`'s own per-host
  default (2 req/s) is the actual governor.
- **`weworkremotely`** (`Method::Scrape`). `weworkremotely.com/categories/*`: confirmed
  static, server-rendered HTML (fetched and read listings directly out of the raw page, no
  JS needed). `robots.txt` is `Allow: /`, disallowing only `/admin/`, `/account/`,
  `/job-seekers/.../profile`, `/manage-company/` — the category paths used here are not
  disallowed. No login wall. No `?page=` on a category page (confirmed: 48 jobs, no visible
  pager), so pagination walks the site's own category slugs
  (`remote-programming-jobs` → `remote-devops-sysadmin-jobs` → …) as the trait's
  `next_cursor`, rather than inventing a page number the site doesn't have.
- **`jobbank-canada`** (`Method::Scrape`, added later, same reference-source bar). Job Bank
  Canada (`jobbank.gc.ca`), the federal job board — confirmed live on 2026-10-08:
  `robots.txt` is `User-agent: *` / `Crawl-delay: 5`, no `Disallow` at all; no login wall on
  search results. Unlike `weworkremotely`, this site does paginate (`?page=N` on a search),
  and its results container is present even past the last page, so an empty page of
  listings ends pagination cleanly rather than looking like a structure change. A listing's
  own link carries a session id that changes on every fetch, so the dedup key and canonical
  URL are built from the listing's numeric posting id instead, confirmed to resolve on its
  own with no session id needed. Live testing against this host is what surfaced §6's
  `RealHttpBackend` gap (PR #126 review): a request with no `User-Agent` header at all was
  503'd by this host's bot-defense outright, while the identical request with any
  ordinary-looking `User-Agent` succeeded — not a problem with this source's own code.

### 11. Amendment (issue #128): CLI surface

The CLI deferral in this ADR's original "Not done here" was correct when written but is no
longer the whole story — the CLI half shipped in a later, separate PR. Three new, additive
ops, all `Inline` except `test` (consistent with `fetch` already being queued — a real
network call never answers inline):

- **`fetchers.list`** — every source `fetch`/`status` could ever resolve (the fixed
  registry, §10, plus every configured `kind` instance, §2a), cross-referenced with its
  `config.toml` state: `{id, method, kind, enabled, interval_s}`. `kind`/`interval_s` are
  `null` for a fixed source with no config entry of its own.
- **`fetchers.status`** — the same source universe, but `{id, last_run_at, running}`:
  per-source last-run time and whether it is currently mid-fetch (the §4/§5 overlap guard).
- **`fetchers.test`** — fetches exactly one page from a source and returns its parsed items
  (`{source, items}`), touching none of `seen.toml`, `last_run.toml`, or the overlap guard,
  and emitting no `fetchers.item.found`. A dry run with no state of its own to protect, so
  (unlike `fetch`) it can run alongside a real fetch of the same source without conflict.

`crates/cli/src/fetchers.rs` gives these, plus the pre-existing `fetchers.fetch`, a real
`shimmer fetchers list|status|test|run` subcommand — `list`/`status` render as tables,
`test`/`run` support `--wait` to follow a queued op to completion client-side (the same
subscribe-then-follow pattern `workspaces.rs` already established).

**Not done in this amendment either:** `fetchers.item.found` events themselves still have no
CLI-visible history — "`shimmer fetchers items`" (recent found items, e.g. for "what did the
last run actually find") needs a new, module-agnostic op backed by the index's events table,
which is a `crates/proto` change. Built in §12. The TUI surface remains entirely out of
scope, as originally stated.

### 12. Amendment: `core.events.list`, and `shimmer fetchers items`

§11's deferred `items` command, built. Confirmed live-testing need (ADR 0028's own reference
sources turned up real postings worth re-reading after the fact), and resolved exactly as
§11 flagged: a new, module-agnostic op, not a `fetchers`-owned one — any module's event
history is the same shape, and `fetchers` has no more special claim to "list recent events"
than `records` or `workspaces` do.

- **`core.events.list`** (inline): `{"topic"?, "limit"?}` → `{"events":[Event…]}`, oldest
  first, capped to the `limit` most recent matches (default 100, max 1000 —
  `crates/daemon/src/core.rs`'s `DEFAULT_EVENTS_LIMIT`/`MAX_EVENTS_LIMIT`). `topic` is a
  subscription-style pattern (`a.b.c`, `a.*`, `*`, default `*`; `crates/proto/src/topic.rs`'s
  `is_valid_pattern`, the same grammar a live subscription already uses) —
  `invalid_params` on anything else. Backed by `crates/store/src/index.rs`'s
  `events_matching_recent`, a sibling to the already-existing `events_matching` that answers
  "most recent," not "oldest" — the derived index, not a log scan, so it answers fast
  regardless of how much history has piled up (CLAUDE.md §1.4). `crates/daemon/src/core.rs`'s
  `Core` now holds the shared index handle (`SharedIndex`) the indexer task already kept
  current, opened before `Core::new` instead of only after it.
- **Found along the way, fixed at the `store` layer, not here:** `events_matching`'s own
  `ORDER BY id` assumed a `Ulid`'s sort order reflects insertion order, which is only true
  *between* milliseconds — `Ulid::from_datetime` (`crates/core/src/event.rs`) fills the rest
  of the id with random bits, so two events applied within the same millisecond (a real
  shape: one `fetchers` run's items all commit in a single transaction, §4) can sort in
  either order relative to each other. Caught by this amendment's own test coming back
  shuffled. Fixed by sorting on SQLite's own implicit `rowid` instead, which always reflects
  insertion order — `events_matching`'s existing behavior and tests are unaffected (nothing
  depended on same-millisecond tie order; `events_matching_recent` is new).
- **`shimmer fetchers items [SOURCE]`** (`crates/cli/src/fetchers.rs`): calls
  `core.events.list` with `topic: "fetchers.item.found"` (a literal, not an import —
  `crates/cli` must not depend on `crates/modules/fetchers`, CLAUDE.md §3's dependency
  rules), then filters client-side by `payload.source_name` when `SOURCE` is given. Displays
  newest-first (reversed from the op's own oldest-first convention — a feed/log-tail
  reads naturally newest-on-top; the raw op keeps the event-log's own ordering). No
  `fetchers`-side change at all: every new behavior lives in `core.events.list` or the CLI.
- **Additive, no `docs/protocol.md` version bump** — same reasoning §9's own ops used: `v`
  is bumped only for breaking changes (`docs/protocol.md`'s own header), and an unknown
  client never asked for `core.events.list` before, so nothing it already does changes.

### 13. Amendment: per-source title filtering

Live testing against real job-board data (§10) turned up the obvious next want: most
postings a source returns are not ones worth tracking. Rather than a `jobbank-canada`-only
fix, this generalizes to every current and future source, config-only, no code per source:

- **`title_contains`/`title_excludes`** (`SourceConfig`, `crates/modules/fetchers/src/
  schedule.rs`): two optional string lists under a source's existing
  `[modules.fetchers.sources.<id>]` entry. Matching is case-insensitive substring, OR'd
  within each list: an item's title must contain *at least one* `title_contains` term when
  that list is non-empty (empty means no inclusion filter at all), and must contain *none*
  of `title_excludes` — which always applies, and always wins over `title_contains` when
  both would otherwise keep an item. Checked against `RawItem::title` only, nothing in
  `raw_data` — a source whose parsed title doesn't carry the distinguishing word (e.g. a
  level buried only in a longer description field) needs a source-level change to surface
  it in the title first, not a smarter filter here.
- **Applied once, in `Fetchers::run_source`**, not inside any `Source::fetch_page` — the
  same mechanism covers every source, present and future (an email source, say), with zero
  source-specific code. A title that fails the filter is marked seen in the same statement
  as a kept one (so it is never re-examined on a later run) but never pushed into `found`,
  so it is never counted toward the per-run cap (§4/§8) and never emitted. Distinct from a
  cap-overflow miss, which is deliberately left *unmarked* so a later run retries it (§8) —
  an excluded title is a decision, not a capacity problem, so it is never retried.
- **`Fetchers::test` applies the identical filter** before returning its preview. A dry run
  that shows items a real run would never keep is a misleading preview of what `run` would
  actually do.
- **Surfaced in `fetchers.list`'s existing payload** (`title_contains`/`title_excludes`
  alongside `enabled`/`interval_s`) rather than a new op — it is config state about a
  source, the same category of thing `fetchers.list` already answers.
- **Not done here:** matching against anything beyond the title (a longer description in
  `raw_data`, location, salary); a smarter-than-substring match (synonyms, stemming, an
  actual seniority classifier). Keyword substring matching cannot tell "mentors junior
  engineers" in a senior posting from an actual junior posting — a known, accepted limit,
  not a gap to close here. Both are explicitly deferred, not ruled out, if a source's real
  data later makes title-only filtering insufficient.

### 14. Amendment: an ad-hoc `--contains`/`--excludes` override

§13's filter is config-only by design, which has a real cost: trying a different filter
means editing `config.toml` and restarting the daemon (`CONTRIBUTING.md`: "the daemon keeps
running the binary it was started from"). Exploring a filter interactively needs a lighter
path.

- **`fetchers.fetch`/`fetchers.test` gain optional `title_contains`/`title_excludes`
  params**, alongside the existing `source`. Present (either one, or both): **replaces**
  the source's configured filter outright for this one call, never merges with it — asking
  for "senior" when the config says "junior" must show senior postings, not the
  intersection (empty) or the union (confusing). Absent (neither given): behavior is
  unchanged from §13, the configured filter (if any) applies exactly as before this existed.
- `Fetchers::run_source`/`Fetchers::test` take this override as a parameter rather than
  reading it from `config.toml` a second way — one `passes_title_filter` decision point,
  not two filter code paths to keep in sync. A dropped item is marked seen exactly as in
  §13, override or not: an ad-hoc exclusion is still a decision, not a capacity miss, so it
  is never retried either.
- **`shimmer fetchers test SOURCE --contains W,W --excludes W,W`** and
  **`shimmer fetchers run SOURCE [--wait] --contains W,W --excludes W,W`**
  (`crates/cli/src/fetchers.rs`): comma-separated term lists, parsed client-side, sent only
  when the flag was actually given — an absent flag must send nothing at all, not an empty
  array (an empty `title_contains` array is itself meaningful: "no inclusion filter," which
  is not the same as "use whatever `config.toml` says").
- **No `core`/`proto` change** — two new optional fields on two existing module ops' own
  params, already advisory-schema (`CommandSpec.params_schema`, CLAUDE.md §5: "JSON Schema;
  advisory before M3").

### 15. Amendment (issue #127's own follow-up): the `json-api` kind

§2a named `json-api` as a later batch and did not build it. Built now: a declarative,
field-path-mapped kind for any endpoint that answers with a JSON array of job-shaped
objects — an ATS's public job-board API (Greenhouse, Lever) or a company's own public job
search endpoint (Amazon) — covering each with configuration, not a new `Source` impl, the
same "point at data, not code" `rss` already established for feeds.

**Config** (`SourceConfig`, `crates/modules/fetchers/src/schedule.rs`): reuses the existing
`url` field (already there for `rss`) as the endpoint, plus five new fields —
`items_path`, `id_field`, `title_field`, `url_field`, `url_base`, `pagination`, `page_size`.
`id_field`/`title_field`/`url_field` are required and checked non-empty at load time, same
as `rss`'s `url`.

```toml
[modules.fetchers.sources.figma-jobs]        # Greenhouse: one response, already-absolute urls
kind = "json-api"
enabled = true
interval = "daily"
url = "https://boards-api.greenhouse.io/v1/boards/figma/jobs"
items_path = "jobs"
id_field = "id"
title_field = "title"
url_field = "absolute_url"

[modules.fetchers.sources.amazon-swe]        # Amazon: paginated, relative urls
kind = "json-api"
enabled = true
interval = "daily"
url = "https://www.amazon.jobs/en/search.json?base_query=software+engineer&offset={offset}&result_limit=50"
items_path = "jobs"
id_field = "id"
title_field = "title"
url_field = "job_path"
url_base = "https://www.amazon.jobs"
pagination = "offset"
page_size = 50
```

- **`items_path`** — a dot-separated path of object keys from the response root to the
  items array. Empty (the default) means the response root is already that array (Lever's
  bare `[...]`, unlike Greenhouse's `{"jobs": [...]}`). Nested field mapping stops there
  deliberately: `raw_data` is the whole matched item object, unfiltered, so a field that
  isn't `id`/`title`/`url` never needs its own path — only those three are ever extracted
  individually, and in every real case checked so far (Greenhouse, Lever, Amazon) all three
  are flat, top-level fields, so the path language stays "one key" for them, not a general
  JSONPath.
- **`id_field`/`title_field`/`url_field`** — top-level field names; a missing field on one
  item skips just that item (same "a page is a reported failure only when something
  structural broke, not when one row happens to be thin" posture `jobbank-canada` already
  takes), not the whole page. A JSON number (Greenhouse's own numeric `id`) is coerced to a
  string, the same way a string field already is — anything else (an object, an array,
  `null`) is treated as absent.
- **`url_base`**, when present, is prepended to `url_field`'s value — for an API (Amazon's)
  whose own url field is a site-relative path, not already absolute. Omitted means the
  field is already a full url, used as-is (Greenhouse, Lever).
- **`pagination`**: `"none"` (default — one response has everything, e.g. Greenhouse,
  Lever) or `"offset"` (e.g. Amazon). `"offset"` requires `page_size > 0` and a literal
  `{offset}` placeholder in `url`, both checked at load time — the `Source` substitutes
  `0`, then `page_size`, then `2 * page_size`, … until a response returns fewer than
  `page_size` items, the same "a short page is the last page" signal `jobbank-canada`'s own
  `page=N` scheme uses, just expressed as a running offset instead of a page number.
- **`Method::Api`** always — a url the caller configured deliberately, the same reasoning
  as `rss` (§2a) and `hn-whoishiring` (§10): not a crawled page, so no robots.txt check
  applies.
- **Where this stops being generic, on purpose:** an HTML response (`weworkremotely`,
  `jobbank-canada` stay hand-written `Method::Scrape` sources — `html-list`, §2a's other
  deferred kind, remains undone); an endpoint needing an auth header, a POST body, or
  cursor-token pagination (a response-supplied token to echo back, rather than a
  client-computed offset) — none of those fit the "GET a url, get back a flat JSON array"
  shape this kind covers, and none are built here.
- **No `core`/`proto` change, no new op** — `SourceConfig`, `crate::source::resolve`, and a
  new `crates/modules/fetchers/src/sources/json_api.rs`, the same shape `rss` already
  established.

### 16. Amendment: a configurable `search_term` for `jobbank-canada`

Live use turned up a real gap: `jobbank-canada`'s search query (`"software engineer"`) was
a compiled-in constant, so there was no way to point the same source at a different query
— e.g. `"junior software developer"` — without a code change. Confirmed live that this
matters: Job Bank's own search does more than literal title matching (results for
`"junior software developer"` are mostly titled plainly "software developer," matched on
something beyond the title text — likely a NOC-code or description-level match), so
`title_contains`/`title_excludes` (§13) filtering *after* a fixed `"software engineer"`
query cannot recover what a *different* query already surfaces directly.

- **`search_term`** (`SourceConfig`): a free-text override of a fixed source's own
  built-in query, read by the source itself rather than validated or interpreted by
  `FetchersConfig::from_value` — there is nothing to validate beyond "is it a string," the
  same posture `title_contains`/`title_excludes` already take. Blank after trimming is
  treated identically to absent: both fall back to `jobbank-canada`'s own
  `DEFAULT_SEARCH_TERM` ("software engineer"), so an empty string in `config.toml` can
  never become a literal empty query to the real site.
- **Scoped generically on `SourceConfig`, read only by `jobbank-canada` today** — the same
  shape `title_contains`/`title_excludes` already have (defined once, any source *could*
  read them, only some do). No other fixed source currently reads `search_term`;
  `weworkremotely` has no equivalent free-text query to override (§10: it walks a fixed
  category-slug rotation, not a search string), and a `json-api`-kind source's query lives
  in its own configured `url` already.
- **Built with `url::Url`'s own query encoder, not string interpolation.** The previous
  hardcoded query was a pre-encoded literal (`"software+engineer"`); once the term became
  configurable and free-text, naive `format!` interpolation would silently produce a
  malformed request for any term containing a space, `&`, or other character needing
  percent-encoding. `url::Url::query_pairs_mut().append_pair(...)` encodes correctly
  regardless of what the configured term contains.
- **No `core`/`proto` change, no new op** — one new `SourceConfig` field and a change
  confined to `crates/modules/fetchers/src/sources/jobbank.rs`.

### 17. Amendment: an ad-hoc `--search-term` override

§16 made `search_term` a persistent `config.toml` setting; trying a different phrasing
still meant an edit plus a daemon restart (same cost §14 already solved for
`title_contains`/`title_excludes`). This is that same fix, for `search_term`.

- **`fetchers.fetch`/`fetchers.test` gain an optional `search_term` param**, alongside the
  existing `title_contains`/`title_excludes` (§14). Present: replaces the source's
  configured `search_term` outright for this one call — same "replace, never merge"
  posture §14 already established. Absent: unchanged from §16.
- **No change to the `Source` trait.** `jobbank-canada` already reads `search_term` from
  `ctx.config` itself (§16); the override is applied by handing it a *different* `Ctx` for
  this one call, built by `ctx_with_search_term_override` — clones `ctx`, patches a copy of
  the real `[modules.fetchers]` JSON so `sources.<id>.search_term` is the override, and
  swaps that onto the clone's `config` field. A source that never reads `search_term`
  (every source but `jobbank-canada`, today) is completely unaffected by this — the patch
  changes a value nothing of theirs looks at.
- **`FetchersConfig::from_value`'s validation requires every entry to have an `interval`.**
  The patch supplies a harmless placeholder (`"daily"`) only when the source has no real
  `config.toml` entry at all, since this override never reaches any scheduling code —
  `fetch`/`test` are both triggered by a direct call, never by a trigger reading the
  patched config back out.
- **`shimmer fetchers test SOURCE --search-term TERM`** and the same flag on
  **`run [--wait]`** (`crates/cli/src/fetchers.rs`): a third flag alongside
  `--contains`/`--excludes`, same `Flags::split` call, forwarded only when actually given.
- **No `core`/`proto` change, no new op** — one new optional field on two existing ops' own
  params.

Confirmed live: `shimmer fetchers test jobbank-canada --search-term "entry level software
developer"` reaches the real site with that exact query and returns real results, while a
plain `shimmer fetchers test jobbank-canada` immediately after still uses the configured
`"junior software developer"` — the override touched nothing persistent.

### 18. Amendment: `active_field` for the `json-api` kind

Configuring `kind = "json-api"` against a community-maintained listings archive (e.g.
SimplifyJobs' New-Grad-Positions `listings.json`, confirmed live) surfaced a gap: that feed
keeps already-closed postings in the same array as open ones, flagged only by its own
boolean `active` field, rather than dropping them. With no way to read that field, `fetch`
and `test` reported a long-closed posting as "new" exactly like a live one — a dead link
the first time anyone clicked it.

- **`SourceConfig` gains an optional `active_field`** (`json-api` only): the field within
  each item marking it still open. Validated at load time only for non-emptiness when
  given — the field name is no more special than `id_field`/`title_field`/`url_field`.
- **`JsonApi::fetch_page` drops an item when, and only when, `active_field`'s value there
  is the boolean `false`.** Missing, non-boolean, or `true` all pass through unfiltered —
  deliberately conservative: a feed that omits the field sometimes (or always, for every
  other configured source) must never have items silently dropped because of a guess.
  Checked before dedup, same stage `title_contains`/`title_excludes` already runs at.
- **No change to `Source`, `RawItem`, or any other kind.** `rss` and every fixed source are
  unaffected; `active_field` is meaningless, and left unvalidated, unless `kind =
  "json-api"`.

Confirmed live: `simplify-newgrad` configured with `active_field = "active"` dropped from
4236 title-matching items to 894 once already-closed postings were excluded, and the first
real `fetchers run` against it reported 50 new items, all genuinely open.

### 19. Amendment: an unconfigured fixed source is not `known`

§10's three reference sources are all job boards, and §2a's `registry()` made them "always
present whether or not it has a `[modules.fetchers.sources.*]` entry at all" — so every
Shimmer user saw `hn-whoishiring`, `weworkremotely`, and `jobbank-canada` in `fetchers list`/
`fetchers status` from a fresh install, regardless of whether they have any interest in job
hunting. That's the opposite of CLAUDE.md §1's "general before specific": job hunting is one
`fetchers` use case among several (tech news, docs sites, transcript APIs), not one the
compiled binary should privilege by always showing up unprompted.

The three got hand-written `Source` impls for a reason that has nothing to do with being job
boards: `hn-whoishiring` needs a two-stage fetch (a thread, then each comment) the generic
`json-api` kind's flat-array model can't express, and `weworkremotely`/`jobbank-canada` are
`Method::Scrape` (crawled HTML, needing a `robots.txt` check), which neither generic kind
covers at all (both are `Method::Api`-only). Every other job source added since (§15's
`figma-jobs`, `robinhood-jobs`, `simplify-newgrad`, …) needed zero Rust — plain `kind =
"json-api"`/`"rss"` config, same as a tech-news or weather-alerts source. So "jobs" was never
privileged in the *mechanism* — only in these three ids always being listed regardless of
configuration.

- **`source::known` now requires a `[modules.fetchers.sources.<id>]` entry — even a bare,
  disabled one — before a fixed-registry source is included.** `registry()`, `find()`, and
  `resolve()` are unchanged: `fetchers test hn-whoishiring`/`fetchers run weworkremotely`
  still work with zero config at all, exactly as before. Only what `fetchers.list`/
  `fetchers.status` surface *unprompted* changed.
- **§5's "sources default disabled" is a different axis and is untouched.** An entry with no
  `enabled` key still starts `false`. This amendment is about whether a source is *listed at
  all* with no entry, not what an entry's own default is.
- **No config schema change.** The same bare entry that already worked for adjusting a fixed
  source's `interval`/filters now does double duty as the opt-in.

Not done here: a one-command "job template" that writes `hn-whoishiring`/`weworkremotely`/
`jobbank-canada` entries into `config.toml` in one step, the way `records`/`workspaces`
templates (ADR 0022, ADR 0025) already work for their own modules. Worth doing — it's the
natural way someone who *does* want job sources turns them on without hand-typing three TOML
blocks — but it is its own, separable piece of work, not a prerequisite for this fix.

## Not done here

- Credentials/secret storage for a future authenticated source. **Not** blocked by the
  `Source` trait or the `[modules.fetchers.sources.*]` config shape (both take `ctx`, which
  can carry a key via `ctx.config` later). **Is** blocked by `HttpBackend::get`'s current
  signature (`crates/core/src/http.rs:29-32`), which takes no custom headers — no module can
  set an `Authorization` header through `ctx.http` today. ADR 0027 already named this as its
  own open question ("Not done here": "Request headers on `HttpBackend::get`... deferred
  until a real consumer defines the actual need"). A credentialed source needs that ADR's
  follow-up before it can exist, regardless of anything here.
- `records` inbox/triage (matching a found item to a saved record) — a `records` feature,
  separate from this module.
- TUI surface — out of scope for this ADR. (CLI surface shipped later — see §11.)
- Headless-browser scraping for JS-rendered sites.
- A `core` change that would let `Module::triggers()` read config directly (§5's finding) —
  would affect every module, not just this one; not proposed here.
- A "job template" that opts into the three fixed sources in one command (§19) — separable
  follow-up, not a prerequisite for §19's own fix.
