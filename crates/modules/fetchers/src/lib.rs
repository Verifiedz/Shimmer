//! `shimmer-fetchers`: reaching outward -- scheduling, dedup, backoff (CLAUDE.md §8, ADR
//! 0028). Rate limiting and caching are not this module's job; they live behind `ctx.http`
//! (ADR 0027).
//!
//! [`source`] is the `Source` trait (ADR 0028 §2); [`sources`] holds one file per
//! implementation -- `hn-whoishiring` (`Method::Api`) and `weworkremotely` (`Method::Scrape`),
//! the fixed reference sources from ADR 0028 §10 that [`source::registry`] always compiles
//! in, plus `rss` and `json-api`, generic *kinds* (ADR 0028 §2a/§3/§15) [`source::resolve`]
//! builds on demand from a config entry, by way of [`kind`]'s `Kind`/`KindConfig` traits --
//! adding one is adding a file implementing those, never an edit to `schedule.rs`'s validator
//! or `source.rs`'s resolver. [`source::known`] only reports a fixed source once the user has
//! given it its own config entry (ADR 0028 §19). [`dedup`] has the
//! per-source "seen" bookkeeping and [`RunGuard`](dedup::RunGuard), the overlap guard
//! (ADR 0028 §4, §5). [`http`] is the retry wrapper around `ctx.http.get` (ADR 0028 §6);
//! [`html`] and [`robots`] are the scraping and robots.txt helpers `weworkremotely` uses,
//! since `Method::Scrape` is responsible for checking robots.txt itself (ADR 0028 §7).
//! [`module`]'s `Fetchers` wires all of this together: `fetchers.tick` finds due sources
//! and enqueues `fetchers.run` for each, which runs the matching `Source`, dedupes its
//! items, and emits `fetchers.item.found` for each new one.
//!
//! Depends on `core` only.

pub mod dedup;
pub mod html;
pub mod http;
pub mod kind;
mod module;
pub mod robots;
mod schedule;
pub mod source;
mod sources;

pub use module::Fetchers;
