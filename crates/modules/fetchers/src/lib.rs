//! `shimmer-fetchers`: reaching outward -- scheduling, dedup, backoff (CLAUDE.md §8, ADR
//! 0028). Rate limiting and caching are not this module's job; they live behind `ctx.http`
//! (ADR 0027).
//!
//! [`source`] is the `Source` trait (ADR 0028 §2); [`sources`] holds one file per
//! implementation -- `hn-whoishiring` and `wwr` (`Method::Scrape`, one category per
//! source), alongside `rss` and `json-api`. All four are *kinds* (ADR 0028 §2a/§3/§10/§15):
//! real Rust code, but with no fixed, compiled-in registry -- every one of them is built on
//! demand from a config entry's own `kind` field, by way of [`kind`]'s `Kind`/`KindConfig`
//! traits. Adding one is adding a file implementing those plus a line in [`kind::kinds`],
//! never an edit to `schedule.rs`'s validator or `source.rs`'s resolver. [`source::known`]
//! only reports a source once the user has given it its own config entry. [`dedup`] has the
//! per-source "seen" bookkeeping and [`RunGuard`](dedup::RunGuard), the overlap guard
//! (ADR 0028 §4, §5). [`http`] is the retry wrapper around `ctx.http.get` (ADR 0028 §6);
//! [`html`] and [`robots`] are the scraping and robots.txt helpers `wwr` uses, since
//! `Method::Scrape` is responsible for checking robots.txt itself (ADR 0028 §7).
//! [`module`]'s `Fetchers` wires all of this together: each source schedules its own next
//! run directly, via `ctx.schedules` (ADR 0028 §6, no fixed heartbeat), which fires
//! `fetchers.run_task` to run the matching `Source`, dedupe its items, and emit
//! `fetchers.item.found` for each new one.
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
