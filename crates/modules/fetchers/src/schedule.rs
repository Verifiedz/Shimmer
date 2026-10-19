//! The fetchers heartbeat: a single, fixed-period scheduler trigger (ADR 0028 §5), and the
//! per-source due-check and last-run bookkeeping it drives.
//!
//! `Module::triggers()` has no `Ctx`/config access (`crates/core/src/module.rs:25-27`, called
//! before `Config::load` and before `Module::init`, `crates/daemon/src/lib.rs:95-96,98-102,120`)
//! -- so the trigger's own firing period is a fixed constant here, not a `config.toml` key.
//! Only which *sources* are due, and how often each one runs, is read from
//! `[modules.fetchers]` when the trigger actually fires and the handler gets a real `Ctx`.
//!
//! (Amends ADR 0028 §5's own example config, which still showed a `tick_interval_s` key --
//! that was never actually readable by `triggers()`, so it is dropped here rather than built.)

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use shimmer_core::{CatchUp, Ctx, Error, Result, Schedule, TriggerSpec};

/// How often the heartbeat fires.
pub const TICK_INTERVAL: Duration = Duration::from_secs(300);
pub const TICK_OP: &str = "fetchers.tick";
pub const FETCH_OP: &str = "fetchers.run";

/// Floors below which a source's own cadence may not be set, per `Method` (ADR 0028 §5/§7).
/// An API source hits a known, source-maintained endpoint built for this kind of polling; a
/// scraped page is a much heavier, more easily-noticed load on a host that never agreed to
/// be polled, so it gets a much higher floor. `crate::source::find`'s registry (which exists
/// now, unlike when a single uniform floor was first written here) is what lets
/// `FetchersConfig::from_value` look a source's `Method` up and apply the right one
/// (PR #126 review -- a single `MIN_INTERVAL_S` let `weworkremotely`, a `Method::Scrape`
/// source, be polled as often as every 15 minutes).
pub const MIN_INTERVAL_API_S: u64 = 1800;
pub const MIN_INTERVAL_SCRAPE_S: u64 = 21_600;

/// The one `TriggerSpec` this module registers (ADR 0028 §5). `lane: None` defers to
/// `fetchers.tick`'s own `CommandSpec` (`Execution::Queued { lane: "fetchers" }`) rather than
/// repeating the lane name here.
pub fn trigger() -> TriggerSpec {
    TriggerSpec {
        id: TICK_OP.into(),
        schedule: Schedule::Every(TICK_INTERVAL),
        catch_up: CatchUp::Skip,
        op: TICK_OP.into(),
        params: Value::Null,
        lane: None,
        fallback: None,
    }
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct FetchersConfig {
    pub sources: BTreeMap<String, SourceConfig>,
    /// Sources whose own entry could not be read, by id, with why. They are kept out of
    /// [`FetchersConfig::sources`] entirely -- never fetched, never scheduled -- but they are
    /// *not* an error for everything else (#126's review, B1): one typo in one source used to
    /// fail every `fetchers.tick`, `fetchers.list` and `fetchers.status`, which with the
    /// built-in `scheduled-task-failed` alert meant a high-priority popup every five minutes.
    /// `fetchers.list`/`status` report these so a broken source is visible rather than silent.
    #[serde(skip)]
    pub problems: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    /// Every source starts disabled (ADR 0028 §5) -- the user opts in explicitly.
    #[serde(default)]
    pub enabled: bool,
    pub interval: IntervalSpec,
    /// Absent: `id` must already be one of the fixed, compiled-in sources
    /// (`crate::source::registry`) -- today's behavior, unchanged. Present: build a new
    /// instance of this *kind* instead (ADR 0028 §2a) -- currently only `"rss"`.
    #[serde(default)]
    pub kind: Option<String>,
    /// The `rss` kind's one required setting (ADR 0028 §2a), and the `json-api` kind's own
    /// endpoint (ADR 0028 §15, which also reuses this field rather than inventing a second
    /// one). Meaningless, and left unvalidated, when `kind` is absent or neither of those.
    #[serde(default)]
    pub url: Option<String>,
    /// `json-api` only (ADR 0028 §15): dot-separated path from the response root to the
    /// items array. Omitted (or empty) means the response root is already that array.
    #[serde(default)]
    pub items_path: Option<String>,
    /// `json-api` only: the field within each item holding its dedup key.
    #[serde(default)]
    pub id_field: Option<String>,
    /// `json-api` only: the field within each item holding its title.
    #[serde(default)]
    pub title_field: Option<String>,
    /// `json-api` only: the field within each item holding its url.
    #[serde(default)]
    pub url_field: Option<String>,
    /// `json-api` only: prepended to `url_field`'s value when that field is a site-relative
    /// path rather than already absolute (e.g. Amazon's own job search endpoint).
    #[serde(default)]
    pub url_base: Option<String>,
    /// `json-api` only: `"none"` (default, everything in one response) or `"offset"` (`url`
    /// must contain a literal `{offset}` placeholder, driven by `page_size`).
    #[serde(default)]
    pub pagination: Option<String>,
    /// `json-api` only: required, and must be greater than 0, when `pagination = "offset"`.
    #[serde(default)]
    pub page_size: Option<u32>,
    /// `json-api` only: the field within each item that marks it still open. When set, an
    /// item whose value there is the boolean `false` is dropped before dedup ever sees it --
    /// a feed that keeps already-closed postings around (e.g. a community-maintained
    /// listings archive) would otherwise surface dead links as "new". Missing, non-boolean,
    /// or `true` all pass through unfiltered; omitted entirely means no such field exists
    /// and nothing is dropped on this basis.
    #[serde(default)]
    pub active_field: Option<String>,
    /// A title must contain at least one of these (case-insensitive substring, OR-matched)
    /// to be kept. Empty means no inclusion filter at all -- everything passes this stage.
    #[serde(default)]
    pub title_contains: Vec<String>,
    /// A title containing any of these (same matching as `title_contains`) is dropped, even
    /// if `title_contains` would have kept it. Checked against every source's own title
    /// only (ADR 0028 §13) -- a source whose `raw_data` carries a longer description is not
    /// searched.
    #[serde(default)]
    pub title_excludes: Vec<String>,
}

impl SourceConfig {
    /// Whether an item's title should be kept, given this source's own filter lists. See
    /// [`matches_title`] for the matching rule itself.
    pub fn matches_title(&self, title: &str) -> bool {
        matches_title(title, &self.title_contains, &self.title_excludes)
    }
}

/// A title matching nothing in `contains` is dropped only when that list is non-empty; a
/// title matching anything in `excludes` is always dropped, regardless of `contains`. Both
/// are case-insensitive substrings, OR'd within each list. A free function, not only
/// [`SourceConfig::matches_title`], so an ad-hoc CLI override (ADR 0028 §14) can run the
/// identical rule against lists that never came from `config.toml` at all.
pub fn matches_title(title: &str, contains: &[String], excludes: &[String]) -> bool {
    let lower = title.to_lowercase();
    let included = contains.is_empty() || contains.iter().any(|w| lower.contains(&w.to_lowercase()));
    let excluded = excludes.iter().any(|w| lower.contains(&w.to_lowercase()));
    included && !excluded
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum IntervalSpec {
    Preset(String),
    Seconds(u64),
}

impl IntervalSpec {
    /// A named preset, or a raw seconds count, checked against `floor_s`. The right floor
    /// depends on the source's `Method` (`MIN_INTERVAL_API_S` / `MIN_INTERVAL_SCRAPE_S`), so
    /// the caller supplies it -- only `FetchersConfig::from_value` can look a source's
    /// `Method` up (via `crate::source::find`) to know which one applies.
    pub fn resolve(&self, floor_s: u64) -> Result<u64> {
        let secs = match self {
            Self::Seconds(n) => *n,
            Self::Preset(name) => match name.as_str() {
                "hourly" => 3600,
                "daily" => 86_400,
                "weekly" => 604_800,
                other => {
                    return Err(Error::invalid_params(format!(
                    "unknown interval preset '{other}'; use \"hourly\", \"daily\", \"weekly\", or a number of seconds"
                )))
                }
            },
        };
        if secs < floor_s {
            return Err(Error::invalid_params(format!("interval must be at least {floor_s}s, got {secs}s")));
        }
        Ok(secs)
    }
}

impl FetchersConfig {
    /// Pure parse, no `Ctx` -- [`load`] is the thin wrapper that supplies `ctx.config.raw()`.
    ///
    /// Each source is read and validated **on its own** (#126's review, B1). One entry with a
    /// bad `url`, an unknown `kind`, a misspelled id or the wrong type for a field is recorded
    /// in `problems` and left out of `sources`; every other source still loads and still runs.
    /// Only a `[modules.fetchers]` section that isn't a table at all, or an unknown key at the
    /// top level, is fatal -- at that point there is no per-source structure to salvage.
    pub fn from_value(raw: &Value) -> Result<Self> {
        // `ModuleConfig::default()` (no `[modules.fetchers]` section attached at all, e.g.
        // `shimmer_core::testing::TestEnv`) wraps `Value::Null`, not `{}` -- both mean "no
        // config given", so both resolve to every default.
        if raw.is_null() {
            return Ok(Self::default());
        }
        // Deserialized in two steps on purpose: `sources` as raw JSON first, so one entry whose
        // *type* is wrong (`interval = {}`) fails only itself, where deserializing the whole
        // `FetchersConfig` in one go would fail the lot.
        #[derive(Default, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        struct Outer {
            sources: BTreeMap<String, Value>,
        }
        let outer: Outer = serde_json::from_value(raw.clone())
            .map_err(|e| Error::invalid_params(format!("[modules.fetchers]: {e}")))?;

        let mut cfg = Self::default();
        for (id, entry) in outer.sources {
            match serde_json::from_value::<SourceConfig>(entry)
                .map_err(|e| Error::invalid_params(format!("sources.{id}: {e}")))
                .and_then(|source| validate_source(&id, &source).map(|()| source))
            {
                Ok(source) => {
                    cfg.sources.insert(id, source);
                }
                Err(e) => {
                    cfg.problems.insert(id, e.message);
                }
            }
        }
        Ok(cfg)
    }
    pub fn load(ctx: &Ctx) -> Result<Self> {
        Self::from_value(ctx.config.raw())
    }
}

/// Everything that must be true of one source's own entry, checked at load rather than on the
/// first tick that happens to look at it. Returns the reason as an error for the caller to
/// record against that one source (#126's review, B1) -- it never decides the fate of any
/// other source.
fn validate_source(id: &str, source: &SourceConfig) -> Result<()> {
    // An id becomes a folder name under the module's own store namespace
    // (`dedup::path`), so it is checked here with the same charset rule every other
    // user-named thing uses, rather than only failing at the first fetch when the store
    // rejects the path (#126's review, B14).
    if !shimmer_core::ids::is_valid_id(id) {
        return Err(Error::invalid_params(format!(
            "sources.{id}: not a usable source id -- lowercase letters, digits, '-' and '_' only"
        )));
    }
    for (what, terms) in [("title_contains", &source.title_contains), ("title_excludes", &source.title_excludes)] {
        // An empty or whitespace term is a substring of every title, so one stray comma in
        // `title_excludes = ["senior", ""]` excludes everything, and in `title_contains`
        // matches everything (#126's review, B3). Neither can be what anyone meant.
        if terms.iter().any(|t| t.trim().is_empty()) {
            return Err(Error::invalid_params(format!(
                "sources.{id}.{what}: an empty term matches every title -- remove it (a trailing comma?)"
            )));
        }
    }
    // Each `Method` gets its own floor (ADR 0028 §5/§7) -- a scraped page must not be
    // pollable at an API source's much lower cadence (PR #126 review). For a `kind`
    // entry there is no fixed `Source` to ask yet (it is only ever built by
    // `crate::source::resolve`, lazily, per `Ctx`) -- but `sources::rss::Rss::method`
    // always answers `Method::Api` (a feed url is a known endpoint, not a crawled
    // page), so the `"rss"` floor can be hardcoded here rather than instantiating one
    // just to ask.
    let floor = match source.kind.as_deref() {
        None => {
            let Some(found) = crate::source::find(id) else {
                let known_sources = crate::source::registry();
                let known: Vec<&str> = known_sources.iter().map(|s| s.id()).collect();
                return Err(Error::invalid_params(format!(
                    "sources.{id}: unknown source id (known: {})",
                    known.join(", ")
                )));
            };
            match found.method() {
                crate::source::Method::Api => MIN_INTERVAL_API_S,
                crate::source::Method::Scrape => MIN_INTERVAL_SCRAPE_S,
            }
        }
        Some(kind) => {
            // A `kind` entry builds a brand-new instance (`crate::source::resolve`)
            // rather than adjusting a fixed, compiled-in one -- so an id that
            // already belongs to the fixed registry would either sit alongside it
            // as a silent duplicate (`crate::source::known`) or shadow it outright
            // (`crate::source::resolve` always prefers the `kind` branch). Neither
            // is a real "this source is both configured and reconfigured" case we
            // want to allow; reject it at load time rather than let one silently
            // win.
            if crate::source::find(id).is_some() {
                return Err(Error::invalid_params(format!(
                    "sources.{id}: kind \"{kind}\" collides with a fixed, compiled-in source of the same id"
                )));
            }
            match kind {
                "rss" => {
                    let url = source.url.as_deref().unwrap_or_default().trim();
                    if url.is_empty() {
                        return Err(Error::invalid_params(format!("sources.{id}: kind \"rss\" needs a non-empty url")));
                    }
                    if !is_http_url(url) {
                        return Err(Error::invalid_params(format!(
                            "sources.{id}: kind \"rss\" url must be a well-formed http:// or https:// url, got '{url}'"
                        )));
                    }
                    MIN_INTERVAL_API_S
                }
                "json-api" => {
                    let url = source.url.as_deref().unwrap_or_default().trim();
                    if url.is_empty() {
                        return Err(Error::invalid_params(format!(
                            "sources.{id}: kind \"json-api\" needs a non-empty url"
                        )));
                    }
                    if !is_http_url(url) {
                        return Err(Error::invalid_params(format!(
                                "sources.{id}: kind \"json-api\" url must be a well-formed http:// or https:// url, got '{url}'"
                            )));
                    }
                    for (field_name, value) in [
                        ("id_field", &source.id_field),
                        ("title_field", &source.title_field),
                        ("url_field", &source.url_field),
                    ] {
                        if value.as_deref().unwrap_or_default().trim().is_empty() {
                            return Err(Error::invalid_params(format!(
                                "sources.{id}: kind \"json-api\" needs a non-empty {field_name}"
                            )));
                        }
                    }
                    if let Some(field) = &source.active_field {
                        if field.trim().is_empty() {
                            return Err(Error::invalid_params(format!(
                                "sources.{id}: kind \"json-api\" active_field must be non-empty when given"
                            )));
                        }
                    }
                    match source.pagination.as_deref().unwrap_or("none") {
                        "none" => {}
                        "offset" => {
                            if source.page_size.unwrap_or(0) == 0 {
                                return Err(Error::invalid_params(format!(
                                    "sources.{id}: kind \"json-api\" pagination \"offset\" needs page_size > 0"
                                )));
                            }
                            if !url.contains("{offset}") {
                                return Err(Error::invalid_params(format!(
                                        "sources.{id}: kind \"json-api\" pagination \"offset\" needs a literal '{{offset}}' placeholder in url"
                                    )));
                            }
                        }
                        other => {
                            return Err(Error::invalid_params(format!(
                                    "sources.{id}: kind \"json-api\" has unknown pagination \"{other}\" (known: none, offset)"
                                )));
                        }
                    }
                    MIN_INTERVAL_API_S
                }
                other => {
                    return Err(Error::invalid_params(format!(
                        "sources.{id}: unknown kind \"{other}\" (known: rss, json-api)"
                    )));
                }
            }
        }
    };
    source.interval.resolve(floor).map_err(|e| Error::invalid_params(format!("sources.{id}.interval: {e}")))?;
    Ok(())
}

/// `url` is deliberately not a free-text field: an `rss` source feeds straight into
/// `http::get` (ADR 0028 §2a), so a malformed value would otherwise surface as a cryptic
/// connection error deep in the first tick rather than a clear one at load time.
fn is_http_url(candidate: &str) -> bool {
    matches!(url::Url::parse(candidate), Ok(u) if u.scheme() == "http" || u.scheme() == "https")
}

/// Every enabled, configured source whose interval has elapsed since its last run (or that
/// has never run at all). Deterministic order (`BTreeMap`'s own id order).
pub fn due_sources(
    config: &FetchersConfig,
    last_run: &BTreeMap<String, DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Vec<String> {
    config
        .sources
        .iter()
        .filter(|(_, c)| c.enabled)
        .filter_map(|(id, c)| {
            // Already validated (against the right per-`Method` floor) by
            // `FetchersConfig::load` -- a floor of 0 here only converts units, it never
            // re-checks anything.
            let interval = c.interval.resolve(0).ok()?;
            let due = match last_run.get(id) {
                None => true,
                // A `last_run` in the future (a clock correction, or a `$SHIMMER_HOME` copied
                // from a machine whose clock was ahead) used to park a source until that time
                // actually arrived -- up to a silent month-long stall (#126's review, B10).
                // Ahead of `now` means the record can't be trusted, so the source is due.
                Some(last) if *last > now => true,
                Some(last) => now.signed_duration_since(*last).num_seconds() >= interval as i64,
            };
            due.then(|| id.clone())
        })
        .collect()
}

/// `data/fetchers/last_run.toml` -- one small file for every source, read-modified-written
/// together rather than one file per source, since a tick always reads and updates it as a
/// whole.
const LAST_RUN_PATH: &str = "last_run.toml";

/// A corrupt or unparseable `last_run.toml` is logged and treated as empty, never an error
/// (#126's review, B1): failing here failed every `fetchers.tick`, so a single bad line in a
/// bookkeeping file stopped all fetching. Empty means every source reads as due, so the worst
/// case is one extra round of fetches -- which `seen.toml` then dedupes to zero new items --
/// rather than no fetching at all. The next successful run rewrites the file cleanly.
/// Individual unparseable *entries* are dropped the same way, keeping the good ones.
pub fn load_last_run(ctx: &Ctx) -> Result<BTreeMap<String, DateTime<Utc>>> {
    let Some(text) = ctx.store.read_string(LAST_RUN_PATH)? else { return Ok(BTreeMap::new()) };
    let Ok(raw) = toml::from_str::<BTreeMap<String, String>>(&text) else { return Ok(BTreeMap::new()) };
    // An entry whose timestamp doesn't parse is dropped, keeping the good ones -- that source
    // reads as due, which is the safe direction.
    Ok(raw
        .into_iter()
        .filter_map(|(id, at)| DateTime::parse_from_rfc3339(&at).ok().map(|at| (id, at.with_timezone(&Utc))))
        .collect())
}

/// Why `last_run.toml` could not be read, if it exists and is unreadable -- reported by
/// `fetchers.status` so a corrupt bookkeeping file is visible (#126's review, B1) instead of
/// silently making every source look due forever. Read on the status path only; the fetch path
/// deliberately doesn't care, because "treat everything as due" is already the safe answer.
pub fn last_run_problem(ctx: &Ctx) -> Option<String> {
    let text = ctx.store.read_string(LAST_RUN_PATH).ok()??;
    match toml::from_str::<BTreeMap<String, String>>(&text) {
        Ok(raw) => raw
            .iter()
            .find(|(_, at)| DateTime::parse_from_rfc3339(at).is_err())
            .map(|(id, _)| format!("last_run.toml: '{id}' has an unreadable timestamp; that source reads as due")),
        Err(e) => Some(format!("last_run.toml is unreadable ({e}); every source reads as due")),
    }
}

/// Records that `source` ran at `now`; every other source's record is left untouched.
pub fn record_run(ctx: &Ctx, source: &str, now: DateTime<Utc>) -> Result<()> {
    let mut all = load_last_run(ctx)?;
    all.insert(source.to_owned(), now);
    let raw: BTreeMap<&str, String> = all.iter().map(|(id, at)| (id.as_str(), at.to_rfc3339())).collect();
    ctx.store.write(LAST_RUN_PATH, toml::to_string(&raw).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use shimmer_core::testing::TestEnv;

    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// A bad source entry no longer fails the whole load (#126's review, B1): it is dropped
    /// from `sources` and its reason recorded in `problems`. Returns that reason, asserting
    /// the source really was kept out of the live set.
    fn problem_for(raw: Value, id: &str) -> String {
        let cfg = FetchersConfig::from_value(&raw).expect("one bad source must not fail the load");
        assert!(!cfg.sources.contains_key(id), "a source that failed validation must not be live");
        cfg.problems.get(id).cloned().unwrap_or_else(|| panic!("no problem recorded for {id}: {:?}", cfg.problems))
    }

    #[test]
    fn presets_resolve_to_the_expected_seconds() {
        assert_eq!(IntervalSpec::Preset("hourly".into()).resolve(0).unwrap(), 3600);
        assert_eq!(IntervalSpec::Preset("daily".into()).resolve(0).unwrap(), 86_400);
        assert_eq!(IntervalSpec::Preset("weekly".into()).resolve(0).unwrap(), 604_800);
    }

    #[test]
    fn an_unknown_preset_is_rejected() {
        let err = IntervalSpec::Preset("fortnightly".into()).resolve(0).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::InvalidParams);
    }

    #[test]
    fn a_raw_interval_below_the_given_floor_is_rejected() {
        let err = IntervalSpec::Seconds(MIN_INTERVAL_API_S - 1).resolve(MIN_INTERVAL_API_S).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::InvalidParams);
        IntervalSpec::Seconds(MIN_INTERVAL_API_S).resolve(MIN_INTERVAL_API_S).unwrap();
    }

    #[test]
    fn a_scrape_source_below_its_own_higher_floor_is_rejected_even_at_an_interval_an_api_source_would_accept() {
        let why =
            problem_for(json!({"sources": {"weworkremotely": {"enabled": true, "interval": 3600}}}), "weworkremotely");
        assert!(why.contains(&MIN_INTERVAL_SCRAPE_S.to_string()), "{why}");

        // The identical interval is fine for an API source -- the floor is per-`Method`, not
        // one uniform number.
        FetchersConfig::from_value(&json!({
            "sources": {"hn-whoishiring": {"enabled": true, "interval": 3600}}
        }))
        .unwrap();
    }

    #[test]
    fn an_absent_modules_section_is_no_sources_not_an_error() {
        let cfg = FetchersConfig::from_value(&json!({})).unwrap();
        assert!(cfg.sources.is_empty());
    }

    #[test]
    fn a_null_config_is_also_no_sources_not_an_error() {
        // `ModuleConfig::default()` wraps `Value::Null`, not `{}` -- both must mean the same
        // thing, not a type error.
        let cfg = FetchersConfig::from_value(&Value::Null).unwrap();
        assert!(cfg.sources.is_empty());
    }

    #[test]
    fn a_configured_source_parses_with_its_interval_validated_up_front() {
        let cfg = FetchersConfig::from_value(&json!({
            "sources": {
                "hn-whoishiring": {"enabled": true, "interval": "daily"},
                "weworkremotely": {"enabled": false, "interval": MIN_INTERVAL_SCRAPE_S},
            }
        }))
        .unwrap();
        assert!(cfg.sources["hn-whoishiring"].enabled);
        assert!(!cfg.sources["weworkremotely"].enabled);
    }

    #[test]
    fn a_title_filter_round_trips_through_config() {
        let cfg = FetchersConfig::from_value(&json!({
            "sources": {
                "hn-whoishiring": {
                    "enabled": true, "interval": "daily",
                    "title_contains": ["junior", "entry level"],
                    "title_excludes": ["senior", "staff"],
                },
            }
        }))
        .unwrap();
        let hn = &cfg.sources["hn-whoishiring"];
        assert_eq!(hn.title_contains, vec!["junior".to_string(), "entry level".to_string()]);
        assert_eq!(hn.title_excludes, vec!["senior".to_string(), "staff".to_string()]);
    }

    #[test]
    fn matches_title_with_no_filter_configured_keeps_everything() {
        let s = source(true, MIN_INTERVAL_API_S);
        assert!(s.matches_title("Senior Staff Engineer"));
        assert!(s.matches_title(""));
    }

    #[test]
    fn matches_title_requires_one_of_contains_case_insensitively() {
        let mut s = source(true, MIN_INTERVAL_API_S);
        s.title_contains = vec!["junior".into(), "entry level".into()];
        assert!(s.matches_title("Junior Software Engineer"));
        assert!(s.matches_title("JUNIOR SOFTWARE ENGINEER"), "must be case-insensitive");
        assert!(s.matches_title("Entry Level Developer"));
        assert!(!s.matches_title("Senior Software Engineer"), "matches neither term");
    }

    #[test]
    fn matches_title_excludes_always_wins_over_contains() {
        let mut s = source(true, MIN_INTERVAL_API_S);
        s.title_contains = vec!["engineer".into()];
        s.title_excludes = vec!["senior".into()];
        assert!(s.matches_title("Junior Engineer"));
        assert!(!s.matches_title("Senior Engineer"), "excludes must win even though 'engineer' is in title_contains");
    }

    #[test]
    fn a_bad_interval_fails_at_load_time_not_on_the_first_tick() {
        let why = problem_for(
            json!({"sources": {"hn-whoishiring": {"enabled": true, "interval": "fortnightly"}}}),
            "hn-whoishiring",
        );
        assert!(why.contains("interval"), "{why}");
    }

    #[test]
    fn an_unknown_top_level_key_is_rejected() {
        let err = FetchersConfig::from_value(&json!({"tick_interval_s": 60})).unwrap_err();
        assert_eq!(err.code, shimmer_core::ErrorCode::InvalidParams);
    }

    #[test]
    fn a_misspelled_source_id_fails_at_load_time_not_on_the_first_tick() {
        let why = problem_for(
            json!({"sources": {"hn-whoishirign": {"enabled": true, "interval": "daily"}}}),
            "hn-whoishirign",
        );
        assert!(why.contains("hn-whoishiring"), "the reason should name the real ids: {why}");
    }

    #[test]
    fn a_configured_rss_kind_with_a_url_parses() {
        let cfg = FetchersConfig::from_value(&json!({
            "sources": {
                "my-team-blog": {"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/feed.xml"},
            }
        }))
        .unwrap();
        assert_eq!(cfg.sources["my-team-blog"].kind.as_deref(), Some("rss"));
    }

    #[test]
    fn an_rss_kind_with_no_url_fails_at_load_time() {
        let why = problem_for(
            json!({"sources": {"my-team-blog": {"enabled": true, "interval": "hourly", "kind": "rss"}}}),
            "my-team-blog",
        );
        assert!(why.contains("url"), "{why}");
    }

    #[test]
    fn an_rss_kind_with_a_non_http_url_fails_at_load_time() {
        let why = problem_for(
            json!({"sources": {"my-team-blog": {"enabled": true, "interval": "hourly", "kind": "rss", "url": "not-a-url"}}}),
            "my-team-blog",
        );
        assert!(why.contains("url"), "{why}");
    }

    #[test]
    fn an_rss_kind_with_a_non_http_scheme_url_fails_at_load_time() {
        let why = problem_for(
            json!({"sources": {"my-team-blog": {"enabled": true, "interval": "hourly", "kind": "rss", "url": "ftp://example.test/feed.xml"}}}),
            "my-team-blog",
        );
        assert!(why.contains("url"), "{why}");
    }

    #[test]
    fn an_rss_kind_with_an_https_url_parses() {
        let cfg = FetchersConfig::from_value(&json!({
            "sources": {"my-team-blog": {"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/feed.xml"}}
        }))
        .unwrap();
        assert_eq!(cfg.sources["my-team-blog"].url.as_deref(), Some("https://example.test/feed.xml"));
    }

    #[test]
    fn a_kind_entry_whose_id_collides_with_a_fixed_source_fails_at_load_time() {
        for id in ["hn-whoishiring", "weworkremotely"] {
            let why = problem_for(
                json!({"sources": {id: {"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/feed.xml"}}}),
                id,
            );
            assert!(why.contains("collides"), "id {id}: {why}");
        }
    }

    #[test]
    fn an_unknown_kind_is_rejected() {
        let why = problem_for(
            json!({"sources": {"my-team-blog": {"enabled": true, "interval": "hourly", "kind": "carrier-pigeon"}}}),
            "my-team-blog",
        );
        assert!(why.contains("carrier-pigeon"), "{why}");
    }

    #[test]
    fn a_json_api_kind_with_every_required_field_parses() {
        let cfg = FetchersConfig::from_value(&json!({
            "sources": {
                "figma-jobs": {
                    "enabled": true, "interval": "daily", "kind": "json-api",
                    "url": "https://boards-api.greenhouse.io/v1/boards/figma/jobs",
                    "items_path": "jobs", "id_field": "id", "title_field": "title", "url_field": "absolute_url",
                },
            }
        }))
        .unwrap();
        assert_eq!(cfg.sources["figma-jobs"].kind.as_deref(), Some("json-api"));
    }

    #[test]
    fn a_json_api_kind_missing_a_required_field_is_rejected() {
        for missing in ["id_field", "title_field", "url_field"] {
            let mut fields = json!({
                "enabled": true, "interval": "daily", "kind": "json-api",
                "url": "https://example.test/jobs",
                "items_path": "jobs", "id_field": "id", "title_field": "title", "url_field": "url",
            });
            fields.as_object_mut().unwrap().remove(missing);
            let why = problem_for(json!({"sources": {"acme-jobs": fields}}), "acme-jobs");
            assert!(why.contains(missing), "missing {missing}: {why}");
        }
    }

    #[test]
    fn a_json_api_kind_with_offset_pagination_needs_page_size_and_a_placeholder() {
        let raw = |extra: Value| {
            let mut fields = json!({
                "enabled": true, "interval": "daily", "kind": "json-api",
                "url": "https://example.test/jobs", "id_field": "id", "title_field": "title", "url_field": "url",
                "pagination": "offset",
            });
            for (k, v) in extra.as_object().unwrap() {
                fields.as_object_mut().unwrap().insert(k.clone(), v.clone());
            }
            json!({"sources": {"acme-jobs": fields}})
        };

        let why = problem_for(raw(json!({"page_size": 0})), "acme-jobs");
        assert!(why.contains("page_size"), "{why}");

        let why = problem_for(raw(json!({"page_size": 50, "url": "https://example.test/jobs"})), "acme-jobs");
        assert!(why.contains("{offset}"), "{why}");

        let good = FetchersConfig::from_value(&raw(
            json!({"page_size": 50, "url": "https://example.test/jobs?offset={offset}"}),
        ))
        .unwrap();
        assert!(good.sources.contains_key("acme-jobs") && good.problems.is_empty());
    }

    #[test]
    fn a_json_api_kind_with_an_unknown_pagination_is_rejected() {
        let why = problem_for(
            json!({"sources": {"acme-jobs": {
                "enabled": true, "interval": "daily", "kind": "json-api",
                "url": "https://example.test/jobs", "id_field": "id", "title_field": "title", "url_field": "url",
                "pagination": "cursor",
            }}}),
            "acme-jobs",
        );
        assert!(why.contains("cursor"), "{why}");
    }

    #[test]
    fn a_json_api_kind_takes_an_optional_active_field() {
        let cfg = FetchersConfig::from_value(&json!({
            "sources": {"acme-jobs": {
                "enabled": true, "interval": "daily", "kind": "json-api",
                "url": "https://example.test/jobs", "id_field": "id", "title_field": "title", "url_field": "url",
                "active_field": "active",
            }}
        }))
        .unwrap();
        assert_eq!(cfg.sources["acme-jobs"].active_field.as_deref(), Some("active"));
    }

    #[test]
    fn a_json_api_kind_rejects_a_blank_active_field() {
        let why = problem_for(
            json!({"sources": {"acme-jobs": {
                "enabled": true, "interval": "daily", "kind": "json-api",
                "url": "https://example.test/jobs", "id_field": "id", "title_field": "title", "url_field": "url",
                "active_field": "  ",
            }}}),
            "acme-jobs",
        );
        assert!(why.contains("active_field"), "{why}");
    }

    fn source(enabled: bool, interval_s: u64) -> SourceConfig {
        SourceConfig {
            enabled,
            interval: IntervalSpec::Seconds(interval_s),
            kind: None,
            url: None,
            items_path: None,
            id_field: None,
            title_field: None,
            url_field: None,
            url_base: None,
            pagination: None,
            page_size: None,
            active_field: None,
            title_contains: Vec::new(),
            title_excludes: Vec::new(),
        }
    }

    #[test]
    fn a_never_run_enabled_source_is_due() {
        let mut cfg = FetchersConfig::default();
        cfg.sources.insert("hn-whoishiring".into(), source(true, MIN_INTERVAL_API_S));
        let due = due_sources(&cfg, &BTreeMap::new(), at("2026-01-01T00:00:00Z"));
        assert_eq!(due, vec!["hn-whoishiring".to_string()]);
    }

    #[test]
    fn a_disabled_source_is_never_due() {
        let mut cfg = FetchersConfig::default();
        cfg.sources.insert("hn-whoishiring".into(), source(false, MIN_INTERVAL_API_S));
        let due = due_sources(&cfg, &BTreeMap::new(), at("2026-01-01T00:00:00Z"));
        assert!(due.is_empty());
    }

    #[test]
    fn a_source_run_within_its_interval_is_not_due_yet() {
        let mut cfg = FetchersConfig::default();
        cfg.sources.insert("hn-whoishiring".into(), source(true, 3600));
        let mut last = BTreeMap::new();
        last.insert("hn-whoishiring".to_string(), at("2026-01-01T00:00:00Z"));
        let due = due_sources(&cfg, &last, at("2026-01-01T00:30:00Z"));
        assert!(due.is_empty(), "only 30 minutes have passed of a 1-hour interval");
    }

    #[test]
    fn a_source_run_past_its_interval_is_due_again() {
        let mut cfg = FetchersConfig::default();
        cfg.sources.insert("hn-whoishiring".into(), source(true, 3600));
        let mut last = BTreeMap::new();
        last.insert("hn-whoishiring".to_string(), at("2026-01-01T00:00:00Z"));
        let due = due_sources(&cfg, &last, at("2026-01-01T01:00:01Z"));
        assert_eq!(due, vec!["hn-whoishiring".to_string()]);
    }

    #[test]
    fn one_bad_source_does_not_stop_the_good_ones() {
        // #126's review, B1: this used to make `from_value` -- and so every `fetchers.tick`,
        // `list` and `status` -- fail outright, so one typo stopped all fetching and (with the
        // built-in scheduled-task-failed alert) popped a high-priority notification every five
        // minutes.
        let cfg = FetchersConfig::from_value(&json!({
            "sources": {
                "good": {"enabled": true, "interval": "daily", "kind": "rss", "url": "https://example.test/f.xml"},
                "bad": {"enabled": true, "interval": "daily", "kind": "rss", "url": "not-a-url"},
                "also-bad": {"enabled": true, "interval": "daily", "kind": "carrier-pigeon"},
                "wrong-type": {"enabled": true, "interval": {}},
            }
        }))
        .unwrap();
        assert_eq!(cfg.sources.keys().collect::<Vec<_>>(), vec!["good"], "the good source still loads");
        assert_eq!(cfg.problems.len(), 3, "each bad source is reported on its own: {:?}", cfg.problems);
        for id in ["bad", "also-bad", "wrong-type"] {
            assert!(cfg.problems.contains_key(id), "{id} should be reported: {:?}", cfg.problems);
        }

        // And the good one is still schedulable.
        let due = due_sources(&cfg, &BTreeMap::new(), at("2026-01-01T00:00:00Z"));
        assert_eq!(due, vec!["good".to_string()]);
    }

    #[test]
    fn an_empty_filter_term_is_rejected_rather_than_matching_every_title() {
        // #126's review, B3: `--excludes senior,` becomes ["senior", ""], and "" is a substring
        // of every title, so everything got excluded. Same the other way for `title_contains`.
        for field in ["title_excludes", "title_contains"] {
            let why = problem_for(
                json!({"sources": {"hn-whoishiring": {"enabled": true, "interval": "daily", field: ["senior", ""]}}}),
                "hn-whoishiring",
            );
            assert!(why.contains(field), "{field}: {why}");
        }
        // Whitespace-only counts as empty.
        problem_for(
            json!({"sources": {"hn-whoishiring": {"enabled": true, "interval": "daily", "title_contains": ["  "]}}}),
            "hn-whoishiring",
        );
    }

    #[test]
    fn a_source_id_that_cannot_be_a_folder_name_is_rejected_at_load() {
        // #126's review, B14: the store's own path check caught this at the first fetch, but an
        // id should be checked where every other user-named thing is -- at load.
        for id in ["../records", "has space", "UPPER", "has/slash"] {
            let why = problem_for(
                json!({"sources": {id: {"enabled": true, "interval": "daily", "kind": "rss", "url": "https://e.test/f"}}}),
                id,
            );
            assert!(why.contains("source id"), "id {id}: {why}");
        }
    }

    #[test]
    fn a_last_run_in_the_future_reads_as_due_rather_than_stalling_the_source() {
        // #126's review, B10: a clock correction, or a $SHIMMER_HOME copied from a machine
        // running ahead, parked a source until that future time actually arrived.
        let mut cfg = FetchersConfig::default();
        cfg.sources.insert("hn-whoishiring".into(), source(true, 3600));
        let mut last = BTreeMap::new();
        last.insert("hn-whoishiring".to_string(), at("2026-02-01T00:00:00Z"));
        let due = due_sources(&cfg, &last, at("2026-01-01T00:00:00Z"));
        assert_eq!(due, vec!["hn-whoishiring".to_string()], "a future last_run must not stall the source");
    }

    #[test]
    fn a_corrupt_last_run_file_does_not_stop_every_source() {
        // #126's review, B1's other half: this returned `Err`, failing the whole tick.
        let env = TestEnv::new("fetchers");
        env.ctx.store.write(LAST_RUN_PATH, "this is not: valid toml [[[").unwrap();
        assert!(load_last_run(&env.ctx).unwrap().is_empty(), "unreadable means every source is due, not an error");
        assert!(last_run_problem(&env.ctx).is_some(), "but it is reported, not silent");

        // A single unreadable entry keeps the good ones.
        env.ctx.store.write(LAST_RUN_PATH, "good = \"2026-01-01T00:00:00Z\"\nbad = \"not a date\"\n").unwrap();
        let runs = load_last_run(&env.ctx).unwrap();
        assert_eq!(runs.keys().collect::<Vec<_>>(), vec!["good"]);
        assert!(last_run_problem(&env.ctx).is_some_and(|p| p.contains("bad")));
    }

    #[test]
    fn trigger_shape_matches_adr_0028() {
        let t = trigger();
        assert_eq!(t.op, TICK_OP);
        assert_eq!(t.schedule, Schedule::Every(TICK_INTERVAL));
        assert_eq!(t.catch_up, CatchUp::Skip);
        assert!(t.lane.is_none(), "defers to fetchers.tick's own CommandSpec lane");
    }

    #[test]
    fn last_run_round_trips_and_leaves_other_sources_alone() {
        let env = TestEnv::new("fetchers");
        record_run(&env.ctx, "hn-whoishiring", at("2026-01-01T00:00:00Z")).unwrap();
        record_run(&env.ctx, "weworkremotely", at("2026-01-02T00:00:00Z")).unwrap();

        let all = load_last_run(&env.ctx).unwrap();
        assert_eq!(all.get("hn-whoishiring"), Some(&at("2026-01-01T00:00:00Z")));
        assert_eq!(all.get("weworkremotely"), Some(&at("2026-01-02T00:00:00Z")));

        // Re-recording one source must not disturb the other's timestamp.
        record_run(&env.ctx, "hn-whoishiring", at("2026-01-03T00:00:00Z")).unwrap();
        let all = load_last_run(&env.ctx).unwrap();
        assert_eq!(all.get("hn-whoishiring"), Some(&at("2026-01-03T00:00:00Z")));
        assert_eq!(all.get("weworkremotely"), Some(&at("2026-01-02T00:00:00Z")));
    }

    #[test]
    fn load_last_run_with_nothing_recorded_yet_is_empty_not_an_error() {
        let env = TestEnv::new("fetchers");
        assert!(load_last_run(&env.ctx).unwrap().is_empty());
    }
}
