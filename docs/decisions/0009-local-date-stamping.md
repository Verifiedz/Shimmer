# 0009. Deriving a local calendar date without storing local time

Status: accepted (M1, #5; landed on master in #6) · Raised by Dev A (CLAUDE.md §4, changes `core`)

Implemented in #5 and merged to master in #6: `crates/core/src/time.rs`, `Ctx.local_tz`,
`Config::load`'s detect-and-persist step, and `crates/modules/records/src/lib.rs`'s `complete`
now stamping via `local_date`.

> **Renamed since:** `swe`, `swe-*` and `SWE_*` in this ADR are now `shimmer`, `shimmer-*` and
> `SHIMMER_*` (ADR 0011). The text below is kept as written.

## Context

`records.complete` stamps `stamp_on_complete` fields with `ctx.clock.now().date_naive()`
(`crates/modules/records/src/lib.rs:243`) — the UTC calendar date. ADR 0004 already flagged
this as "a real limitation for a laptop app": a solve at 11pm US/Eastern lands on tomorrow's
date. PR 2 and PR 3 (records module and its CLI) carried the same limitation forward and
asked for a real fix rather than another deferral.

This is not only a `records` problem. The scheduler's future daily jobs and the calendar
module (§1.3, §6.3) will hit the identical question: "what day is it, for the human," derived
from a UTC instant. One helper should answer it everywhere, which is why this needs a `core`
change rather than a fix local to `records`.

**Explicitly out of scope:** ADR 0004's larger deferral — evaluating *cron schedules* in
local time (DST gaps/overlaps in "fire at 09:00 local") — stays deferred. This ADR only
covers turning an already-fired UTC instant into a local calendar date, a strictly smaller
and always-well-defined problem (an instant maps to exactly one local date; the ambiguity in
cron scheduling comes from going the other direction, local wall-clock to instant, which we
are not doing).

## Decision

**A `local_timezone` setting in `config.toml`:**

```toml
[general]
local_timezone = "America/New_York"   # any IANA name
```

Resolved once at daemon startup, not re-read per call. On first run, with no `config.toml`
or no `local_timezone` key, the daemon best-effort detects the system timezone (`TZ` env var,
else the `/etc/localtime` symlink target on Linux/macOS) and **writes it into `config.toml`**
so it is inspectable and hand-editable afterward — it is not silently re-detected on every
start, so a user who travels edits the file rather than fighting a heuristic. If detection
fails, it falls back to `"UTC"` and logs a warning once at startup.

**A new `swe_core::time` module, pure functions only:**

```rust
/// Wraps a resolved IANA timezone. Parsing/validating the name happens once, at load.
pub struct LocalTimezone(/* ... */);

impl LocalTimezone {
    pub fn parse(iana_name: &str) -> Result<Self>;
    pub const UTC: LocalTimezone = /* ... */;
}

/// The calendar date `at` falls on in `tz`. Pure — no I/O, no `Utc::now()`, takes an instant.
pub fn local_date(at: DateTime<Utc>, tz: &LocalTimezone) -> NaiveDate;
```

`records.complete` becomes `local_date(ctx.clock.now(), &ctx.local_tz)`; the scheduler's
daily-boundary logic and the calendar module call the same function later.

**Event and file timestamps stay UTC, unchanged.** `Event.at`, `enqueued_at`, and every
stored `DateTime<Utc>` keep meaning exactly what they mean today — portable across machines
and unambiguous in the event log. Only the *derived* calendar date used for stamping or
display changes. We never store a local-time timestamp as if it were absolute.

**New dependency:** resolving an IANA name to an offset needs a timezone database —
decided: `chrono-tz` (compiled-in database, no filesystem or network lookup at runtime, so
this stays local-first). This is a new dependency on `core` and needs sign-off alongside the
rest of this ADR (§4).

## Where `local_date` reads the timezone from — decided

Two shapes were considered for getting the resolved `LocalTimezone` to a module. This was
flagged explicitly for Dev A/B to pick before any code landed, because it decides how much
of `core`'s existing test surface moves:

**Option A — a method on `Clock`:** `ctx.clock.today_local(&tz) -> NaiveDate`. Terser at the
call site, and additive (existing `Clock::fake` construction and every test using it is
unaffected — no existing signature changes). The cost: `Clock`'s stated job (§5, `clock.rs`
doc comment) is narrowly "injectable clock... testable by advancing a fake clock." Teaching
it timezone lookups pulls a config-shaped concern (which IANA zone) into a type whose whole
point is being a trivial, dependency-free time source, and it becomes the type that pulls in
`chrono-tz`.

**Option B — a free function taking the clock (recommended):**
`swe_core::time::local_date(clock.now(), &local_tz)`. `Clock` does not change at all: no new
method, no new dependency inside `clock.rs`, zero risk to its existing tests or the dozen
call sites across `daemon`/`scheduler` that already construct `Clock::fake(...)`. The
timezone lookup and its dependency live in one new, independently testable module. The cost
is one extra argument at call sites, and callers must have both `ctx.clock` and the resolved
timezone in scope — which they already will, once `Ctx` carries the second field below.

**Decided: Option B.** `Clock` is used pervasively enough (§5's table lists it as
load-bearing) that growing its contract for a feature that is really "look up a config value
and do timezone math" is the wrong place to put that math, versus one new pure function next
to it. `Clock` and every existing `Clock::fake` call site are unchanged by this ADR.

**`Ctx` gains a second field to carry the resolved zone to modules:**
`pub local_tz: swe_core::time::LocalTimezone`, populated by the daemon from `config.toml` at
startup, alongside the existing `clock`. Per §5, adding a `Ctx` field is called out as a
security decision, not a convenience — flagged here for the same sign-off rather than added
quietly. It is plain resolved config data, not a capability: a module can read what zone
it's in but cannot change the system clock through it — but the rule is "adding a field", not
"adding a capability", so it goes through the same door. CLAUDE.md §5's `Ctx` listing is
updated to match.

## Not done here

* Cron/`Every` schedule evaluation in local time (ADR 0004's deferral) — unrelated axis,
  stays deferred.
* Retroactively correcting dates already stamped in UTC before this lands — no migration;
  old stamps keep their (possibly off-by-one) value.
* A per-record or per-collection timezone override. One process-wide zone from config, same
  as every other global setting in `config.toml`.
