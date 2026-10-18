//! Per-source "have I surfaced this id before" bookkeeping, and the guard that stops a
//! scheduled and a manual run of the same source overlapping (ADR 0028 §4, §5).
//!
//! `SeenSet` is pure data in, pure data out -- it only tracks ids and decides what to keep.
//! The orchestration that will drive it (fetch a page, check `is_new`, stop once enough new
//! ids have been found, `touch` everything examined, write it back in the same
//! `ctx.store.transaction` as the `fetchers.item.found` events) lands with the `Source` trait
//! and the commands that use it -- nothing here calls `ctx.store.transaction` or emits an
//! event.

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use shimmer_core::{Ctx, Error, Result};

/// How long an id is remembered after the last run that returned it. Past this, it is
/// pruned, and would be (correctly) treated as new again if the source ever returned it.
pub const RETENTION_DAYS: i64 = 90;

/// At most this many ids not already in the set are accepted as new in one run (ADR 0028
/// §8) -- the rest are left unmarked and picked up on a later run, which is also how a first
/// contact with a large, already-stuffed board gets spread across several runs instead of
/// flooding one. Enforced by the orchestration that drives a `Source`, not by `SeenSet`
/// itself, which only ever sees the ids it's told to.
pub const MAX_NEW_PER_RUN: usize = 50;

/// One source's dedup state: every id it has returned, and when it was last seen.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SeenSet(BTreeMap<String, DateTime<Utc>>);

impl SeenSet {
    pub fn parse(text: &str) -> Result<Self> {
        let raw: BTreeMap<String, String> =
            toml::from_str(text).map_err(|e| Error::internal(format!("fetchers seen.toml: {e}")))?;
        let mut out = BTreeMap::new();
        for (id, at) in raw {
            let parsed = DateTime::parse_from_rfc3339(&at)
                .map_err(|e| Error::internal(format!("fetchers seen.toml: '{id}': {e}")))?;
            out.insert(id, parsed.with_timezone(&Utc));
        }
        Ok(Self(out))
    }

    pub fn to_toml(&self) -> String {
        let raw: BTreeMap<&str, String> = self.0.iter().map(|(id, at)| (id.as_str(), at.to_rfc3339())).collect();
        toml::to_string(&raw).unwrap_or_default()
    }

    /// Not yet in the set -- the caller has never surfaced this id before.
    pub fn is_new(&self, id: &str) -> bool {
        !self.0.contains_key(id)
    }

    /// Marks `id` seen as of `now`, whether it was already known or brand new. A
    /// permanently-recurring id must be touched on every run it still appears in, or it would
    /// eventually age past [`RETENTION_DAYS`] and be wrongly treated as new again.
    pub fn touch(&mut self, id: impl Into<String>, now: DateTime<Utc>) {
        self.0.insert(id.into(), now);
    }

    /// Drops ids last touched more than [`RETENTION_DAYS`] before `now`. Returns how many.
    pub fn prune(&mut self, now: DateTime<Utc>) -> usize {
        let before = self.0.len();
        let cutoff = now - chrono::Duration::days(RETENTION_DAYS);
        self.0.retain(|_, at| *at >= cutoff);
        before - self.0.len()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// `data/fetchers/<source>/seen.toml`, relative to the module's own `ctx.store` namespace.
pub fn path(source: &str) -> String {
    format!("{source}/seen.toml")
}

/// A missing file is an empty set -- a source's first run, not an error.
pub fn load(ctx: &Ctx, source: &str) -> Result<SeenSet> {
    match ctx.store.read_string(&path(source))? {
        Some(text) => SeenSet::parse(&text),
        None => Ok(SeenSet::default()),
    }
}

/// Prevents a scheduled tick and a manual `fetchers.fetch` for the same source running at
/// once (ADR 0028 §5). In-process only -- correct because there is exactly one daemon
/// process per `$SHIMMER_HOME` (CLAUDE.md §1.6, "one writer").
#[derive(Default)]
pub struct RunGuard(Mutex<HashSet<String>>);

impl RunGuard {
    /// `None` if `source` is already running. The returned permit releases it automatically
    /// on drop -- including on an early return or a panic -- so a caller can never forget to.
    pub fn try_acquire(&self, source: &str) -> Option<RunPermit<'_>> {
        let mut running = self.0.lock().unwrap_or_else(|e| e.into_inner());
        running.insert(source.to_owned()).then(|| RunPermit { guard: self, source: source.to_owned() })
    }

    /// A non-mutating peek, for a caller (the tick handler) that only wants to skip enqueuing
    /// a source it can already see is running -- an optimization, never the actual guarantee,
    /// which is [`RunGuard::try_acquire`] at the point a run actually starts.
    pub fn is_running(&self, source: &str) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).contains(source)
    }
}

pub struct RunPermit<'a> {
    guard: &'a RunGuard,
    source: String,
}

impl Drop for RunPermit<'_> {
    fn drop(&mut self) {
        self.guard.0.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.source);
    }
}

#[cfg(test)]
mod tests {
    use shimmer_core::testing::TestEnv;

    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn unknown_ids_are_new_until_touched() {
        let mut set = SeenSet::default();
        assert!(set.is_new("c-1"));
        set.touch("c-1", at("2026-01-01T00:00:00Z"));
        assert!(!set.is_new("c-1"));
        assert!(set.is_new("c-2"), "touching one id must not affect another");
    }

    #[test]
    fn touch_refreshes_an_already_known_id_rather_than_leaving_its_old_timestamp() {
        let mut set = SeenSet::default();
        set.touch("c-1", at("2026-01-01T00:00:00Z"));
        set.touch("c-1", at("2026-06-01T00:00:00Z"));
        assert_eq!(set.len(), 1);
        // A re-touch within retention of the new timestamp must survive a prune the old
        // timestamp alone would not have.
        let pruned = set.prune(at("2026-06-02T00:00:00Z"));
        assert_eq!(pruned, 0);
        assert!(!set.is_new("c-1"));
    }

    #[test]
    fn prune_drops_only_ids_older_than_the_retention_window() {
        let mut set = SeenSet::default();
        set.touch("old", at("2026-01-01T00:00:00Z"));
        set.touch("recent", at("2026-03-01T00:00:00Z"));
        // 2026-04-01 is 90 days after 2026-01-01, comfortably more than 90 days after
        // 2026-01-01 and well under 90 days after 2026-03-01.
        let now = at("2026-04-15T00:00:00Z");
        let pruned = set.prune(now);
        assert_eq!(pruned, 1);
        assert!(set.is_new("old"), "a pruned id must read back as new");
        assert!(!set.is_new("recent"));
    }

    #[test]
    fn round_trips_through_toml() {
        let mut set = SeenSet::default();
        set.touch("c-1", at("2026-01-01T00:00:00Z"));
        set.touch("c-2", at("2026-02-15T12:30:00Z"));
        let text = set.to_toml();
        let back = SeenSet::parse(&text).unwrap();
        assert_eq!(set, back);
    }

    #[test]
    fn a_malformed_seen_file_is_a_reported_error_not_a_silent_empty_set() {
        let err = SeenSet::parse("c-1 = \"not a date\"\n").unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::Internal);
    }

    #[test]
    fn load_with_no_file_yet_is_an_empty_set_not_an_error() {
        let env = TestEnv::new("fetchers");
        let set = load(&env.ctx, "hn-whoishiring").unwrap();
        assert!(set.is_empty());
    }

    #[test]
    fn load_round_trips_what_was_written_through_ctx_store() {
        let env = TestEnv::new("fetchers");
        let mut set = SeenSet::default();
        set.touch("c-1", at("2026-01-01T00:00:00Z"));
        env.ctx.store.write(&path("hn-whoishiring"), set.to_toml()).unwrap();

        let loaded = load(&env.ctx, "hn-whoishiring").unwrap();
        assert_eq!(loaded, set);
        // A different source must never see this one's file.
        assert!(load(&env.ctx, "weworkremotely").unwrap().is_empty());
    }

    #[test]
    fn a_second_acquire_for_a_running_source_is_refused() {
        let guard = RunGuard::default();
        let permit = guard.try_acquire("hn-whoishiring").expect("first acquire must succeed");
        assert!(guard.try_acquire("hn-whoishiring").is_none(), "the same source must not run twice at once");
        drop(permit);
        assert!(guard.try_acquire("hn-whoishiring").is_some(), "releasing the permit must free it up again");
    }

    #[test]
    fn different_sources_never_block_each_other() {
        let guard = RunGuard::default();
        let _a = guard.try_acquire("hn-whoishiring").unwrap();
        assert!(guard.try_acquire("weworkremotely").is_some());
    }

    #[test]
    fn is_running_peeks_without_acquiring() {
        let guard = RunGuard::default();
        assert!(!guard.is_running("hn-whoishiring"));
        let permit = guard.try_acquire("hn-whoishiring").unwrap();
        assert!(guard.is_running("hn-whoishiring"));
        // A peek must not itself hold anything -- a real acquire attempt still sees it as
        // taken, not freed by the act of checking.
        assert!(guard.is_running("hn-whoishiring"));
        assert!(guard.try_acquire("hn-whoishiring").is_none());
        drop(permit);
        assert!(!guard.is_running("hn-whoishiring"));
    }

    #[test]
    fn dropping_a_permit_on_an_early_return_still_releases_it() {
        let guard = RunGuard::default();
        fn runs_and_bails(guard: &RunGuard) -> Option<()> {
            let _permit = guard.try_acquire("hn-whoishiring")?;
            None // simulates an early-return failure path, permit dropped here regardless
        }
        assert_eq!(runs_and_bails(&guard), None);
        assert!(guard.try_acquire("hn-whoishiring").is_some(), "the permit must have been released on drop");
    }
}
