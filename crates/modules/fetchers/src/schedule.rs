//! The fetchers heartbeat: a single, fixed-period scheduler trigger (ADR 0028 §5), and the
//! per-source due-check and last-run bookkeeping it drives.
//!
//! `Module::triggers()` has no `Ctx`/config access (`crates/core/src/module.rs:25-27`, called
//! before `Config::load` and before `Module::init`, `crates/daemon/src/lib.rs:95-96,98-102,120`)
//! -- so the trigger's own firing period is a fixed constant here, not a config key.
//! Only which *sources* are due, and how often each one runs, is read from the loaded
//! [`Sources`] when the trigger actually fires and the handler gets a real `Ctx`.
//!
//! (Amends ADR 0028 §5's own example config, which still showed a `tick_interval_s` key --
//! that was never actually readable by `triggers()`, so it is dropped here rather than built.)

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use shimmer_core::{CatchUp, Ctx, Error, Result, Schedule, TriggerSpec};

/// How often the heartbeat fires.
pub const TICK_INTERVAL: Duration = Duration::from_secs(300);
pub const TICK_OP: &str = "fetchers.tick";
/// The real fetch work, queued (ADR 0028 §5) -- internal; `fetchers.run` (inline) checks the
/// source exists, then enqueues this.
pub const RUN_TASK_OP: &str = "fetchers.run_task";

/// Floors below which a source's own cadence may not be set, per `Method` (ADR 0028 §5/§7).
/// An API source hits a known, source-maintained endpoint built for this kind of polling; a
/// scraped page is a much heavier, more easily-noticed load on a host that never agreed to
/// be polled, so it gets a much higher floor.
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

/// Where a source's own file lives (ADR 0028 §4): `data/fetchers/sources/<id>.toml`.
pub const SOURCES_DIR: &str = "sources";
/// Where `fetchers.remove` moves a source's file, until `restore` or `purge` (ADR 0038 §1).
pub const REMOVED_DIR: &str = "removed";

pub(crate) fn path(id: &str) -> String {
    format!("{SOURCES_DIR}/{id}.toml")
}

pub(crate) fn removed_path(id: &str) -> String {
    format!("{REMOVED_DIR}/{id}.toml")
}

/// The ids of removed sources, to restore or purge.
pub(crate) fn removed(ctx: &Ctx) -> Result<Vec<String>> {
    let prefix = format!("{REMOVED_DIR}/");
    Ok(ctx
        .store
        .list(REMOVED_DIR)?
        .iter()
        .filter_map(|f| f.strip_prefix(&prefix).and_then(|f| f.strip_suffix(".toml")))
        .map(str::to_owned)
        .collect())
}

/// Every source whose file loaded, plus every one that didn't (ADR 0028 §5). Loaded once at
/// `Module::init`, re-read on `fetchers.reload` and after any write -- never re-scanned on
/// every op call, the same shape `automations::State` already uses for its rule files.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Sources {
    pub sources: BTreeMap<String, SourceConfig>,
    /// A source file that couldn't be used, by id, with why. Kept out of [`Sources::sources`]
    /// entirely -- never fetched, never scheduled -- but not an error for everything else
    /// (#126's review, B1): one broken file must not take every other source down with it.
    pub problems: BTreeMap<String, String>,
}

impl Sources {
    /// Reads and parses every `data/fetchers/sources/*.toml`. One bad file is recorded in
    /// `problems` and left out of `sources`; every other file still loads. A missing
    /// `sources/` folder at all (a fresh install) is empty, not an error.
    pub fn load(ctx: &Ctx) -> Result<Self> {
        let mut sources = BTreeMap::new();
        let mut problems = BTreeMap::new();
        let prefix = format!("{SOURCES_DIR}/");
        for file in ctx.store.list(SOURCES_DIR)? {
            let Some(id) = file.strip_prefix(&prefix).and_then(|f| f.strip_suffix(".toml")) else { continue };
            // Read raw bytes, not `read_string` -- that errors on non-UTF-8, and the `?` this
            // used to be written with failed `Module::init` for every source over one bad
            // file, against this function's own "one bad file is recorded in problems"
            // contract (#303's review, blocking).
            let bytes = ctx.store.read(&file)?.unwrap_or_default();
            let result = match String::from_utf8(bytes) {
                Ok(text) => SourceConfig::parse(id, &text),
                Err(_) => Err("not UTF-8".to_owned()),
            };
            match result {
                Ok(config) => {
                    sources.insert(id.to_owned(), config);
                }
                Err(e) => {
                    problems.insert(id.to_owned(), e);
                }
            }
        }
        Ok(Self { sources, problems })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceConfig {
    /// Optional, free text, like a workspace's or automation's label.
    pub label: Option<String>,
    /// Every source starts disabled (ADR 0028 §5) -- the user opts in explicitly.
    pub enabled: bool,
    pub interval: IntervalSpec,
    /// Absent: `id` must already be one of the fixed, compiled-in sources
    /// (`crate::source::registry`). Present: built from this source's own `[kind]` table
    /// instead (ADR 0028 §2a/§3), via `crate::kind::kind`.
    pub kind: Option<String>,
    /// A title must contain at least one of these (case-insensitive substring, OR-matched)
    /// to be kept. Empty means no inclusion filter at all -- everything passes this stage.
    pub title_contains: Vec<String>,
    /// A title containing any of these (same matching as `title_contains`) is dropped, even
    /// if `title_contains` would have kept it. Checked against every source's own title
    /// only (ADR 0028 §13) -- a source whose `raw_data` carries a longer description is not
    /// searched.
    pub title_excludes: Vec<String>,
    /// This source's `kind`'s own fields, from the file's `[kind]` table, untouched and
    /// opaque to this struct -- `crate::kind::Kind::parse` is the only thing that knows what
    /// any of these keys mean (ADR 0028 §3).
    pub kind_params: Map<String, Value>,
}

/// The file as written, `data/fetchers/sources/<id>.toml` (ADR 0028 §4): `[source]`, the
/// envelope every kind shares, and `[kind]`, that kind's own fields. Also `Serialize` --
/// [`render`] builds this same shape from `fetchers.new`/`.edit`'s structured params, so a
/// structured command and a hand-edited file go through the exact same [`SourceConfig::parse`].
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceFile {
    source: SourceMeta,
    #[serde(default)]
    kind: toml::Table,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(default)]
    enabled: bool,
    interval: IntervalSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    title_contains: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    title_excludes: Vec<String>,
}

/// Builds a source file's TOML text from `fetchers.new`/`.edit`'s structured params (ADR 0028
/// §9) -- the mechanical inverse of [`SourceConfig::parse`], so a structured command, a
/// template (later), and a hand-edited-then-`fetchers.reload`ed file all produce and accept
/// the identical on-disk shape. Validation stays centralized in `SourceConfig::parse`, called
/// on this function's own output by the caller -- this function only renders text, it never
/// decides whether that text is valid.
pub(crate) fn render(
    label: Option<&str>,
    kind: Option<&str>,
    enabled: bool,
    interval: &IntervalSpec,
    params: &Map<String, Value>,
    title_contains: &[String],
    title_excludes: &[String],
) -> std::result::Result<String, String> {
    let kind_table = match toml::Value::try_from(params) {
        Ok(toml::Value::Table(t)) => t,
        Ok(_) => unreachable!("a JSON object always serializes to a toml table"),
        Err(e) => return Err(format!("params: {e}")),
    };
    let file = SourceFile {
        source: SourceMeta {
            label: label.map(str::to_owned),
            enabled,
            interval: interval.clone(),
            kind: kind.map(str::to_owned),
            title_contains: title_contains.to_vec(),
            title_excludes: title_excludes.to_vec(),
        },
        kind: kind_table,
    };
    toml::to_string(&file).map_err(|e| format!("internal: {e}"))
}

impl SourceConfig {
    /// `id`'s file text, checked: its id's own charset, every `title_contains`/
    /// `title_excludes` term, and -- the bulk of it -- its `kind`'s own fields, dispatched
    /// through `crate::kind::kind` (ADR 0028 §3). One sentence naming what's wrong, the same
    /// convention `Rule::parse` already uses in `automations`, so a broken file looks the
    /// same regardless of which module it's from.
    pub fn parse(id: &str, text: &str) -> std::result::Result<Self, String> {
        // An id becomes a folder name under the module's own store namespace
        // (`dedup::path`), so it is checked here with the same charset rule every other
        // user-named thing uses, rather than only failing at the first fetch when the store
        // rejects the path (#126's review, B14).
        if !shimmer_core::ids::is_valid_id(id) {
            return Err("not a usable source id -- lowercase letters, digits, '-' and '_' only".into());
        }
        let file: SourceFile = toml::from_str(text).map_err(|e| format!("not a source file: {}", e.message()))?;
        for (what, terms) in
            [("title_contains", &file.source.title_contains), ("title_excludes", &file.source.title_excludes)]
        {
            // An empty or whitespace term is a substring of every title, so one stray comma
            // in `title_excludes = ["senior", ""]` excludes everything, and in
            // `title_contains` matches everything (#126's review, B3).
            if terms.iter().any(|t| t.trim().is_empty()) {
                return Err(format!("{what}: an empty term matches every title -- remove it (a trailing comma?)"));
            }
        }
        let kind_params =
            serde_json::to_value(&file.kind).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default();
        // Each `Method` gets its own floor (ADR 0028 §5/§7) -- a scraped page must not be
        // pollable at an API source's much lower cadence (PR #126 review). `Kind::method` is
        // static per kind, so this needs no built `Source` to ask.
        let floor = match file.source.kind.as_deref() {
            None => {
                let Some(found) = crate::source::find(id) else {
                    let registry = crate::source::registry();
                    let known: Vec<&str> = registry.iter().map(|s| s.id()).collect();
                    return Err(format!("unknown source id (known: {})", known.join(", ")));
                };
                // A fixed, compiled-in source has no `kind` and so no `Kind::parse` of its
                // own to reject a stray key -- without this, a `[kind]` table on
                // hn-whoishiring/weworkremotely loaded silently with every key ignored.
                crate::kind::reject_unknown(&kind_params, &[])?;
                match found.method() {
                    crate::source::Method::Api => MIN_INTERVAL_API_S,
                    crate::source::Method::Scrape => MIN_INTERVAL_SCRAPE_S,
                }
            }
            Some(kind_name) => {
                // A `kind` entry builds a brand-new instance rather than adjusting a fixed,
                // compiled-in one -- so an id that already belongs to the fixed registry
                // would either sit alongside it as a silent duplicate (`crate::source::known`)
                // or shadow it outright (`crate::source::resolve` always prefers the `kind`
                // branch). Neither is a real "configured and reconfigured" case to allow.
                if crate::source::find(id).is_some() {
                    return Err(format!(
                        "kind \"{kind_name}\" collides with a fixed, compiled-in source of the same id"
                    ));
                }
                let kind = crate::kind::kind(kind_name)
                    .ok_or_else(|| format!("unknown kind \"{kind_name}\" (known: {})", crate::kind::known_names()))?;
                kind.parse(&kind_params).map_err(|m| format!("kind \"{kind_name}\" {m}"))?;
                match kind.method() {
                    crate::source::Method::Api => MIN_INTERVAL_API_S,
                    crate::source::Method::Scrape => MIN_INTERVAL_SCRAPE_S,
                }
            }
        };
        file.source.interval.resolve(floor).map_err(|e| format!("interval: {e}"))?;
        Ok(Self {
            label: file.source.label,
            enabled: file.source.enabled,
            interval: file.source.interval,
            kind: file.source.kind,
            title_contains: file.source.title_contains,
            title_excludes: file.source.title_excludes,
            kind_params,
        })
    }

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
/// identical rule against lists that never came from a source's own file at all.
pub fn matches_title(title: &str, contains: &[String], excludes: &[String]) -> bool {
    let lower = title.to_lowercase();
    let included = contains.is_empty() || contains.iter().any(|w| lower.contains(&w.to_lowercase()));
    let excluded = excludes.iter().any(|w| lower.contains(&w.to_lowercase()));
    included && !excluded
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum IntervalSpec {
    Preset(String),
    Seconds(u64),
}

impl IntervalSpec {
    /// A named preset, or a raw seconds count, checked against `floor_s`. The right floor
    /// depends on the source's `Method` (`MIN_INTERVAL_API_S` / `MIN_INTERVAL_SCRAPE_S`), so
    /// the caller supplies it -- only [`SourceConfig::parse`] can look a source's `Method` up
    /// (via `crate::source::find`/`crate::kind::kind`) to know which one applies.
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

/// Every enabled, configured source whose interval has elapsed since its last run (or that
/// has never run at all). Deterministic order (`BTreeMap`'s own id order).
pub fn due_sources(
    sources: &BTreeMap<String, SourceConfig>,
    last_run: &BTreeMap<String, DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Vec<String> {
    sources
        .iter()
        .filter(|(_, c)| c.enabled)
        .filter_map(|(id, c)| {
            // Already validated (against the right per-`Method` floor) by
            // `SourceConfig::parse` -- a floor of 0 here only converts units, it never
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
pub(crate) const LAST_RUN_PATH: &str = "last_run.toml";

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

/// `last_run.toml`'s text with `from`'s entry moved to `to`, if `from` had one; `None` if
/// `from` never ran -- a never-run source being renamed needs no `last_run.toml` write at all
/// (ADR 0028 §7). The caller puts the result in the same `ctx.store.transaction` as the
/// source file's own move, so the rename is one atomic unit (CLAUDE.md §7.1).
pub(crate) fn rename_last_run(ctx: &Ctx, from: &str, to: &str) -> Result<Option<String>> {
    let mut all = load_last_run(ctx)?;
    let Some(at) = all.remove(from) else { return Ok(None) };
    all.insert(to.to_owned(), at);
    let raw: BTreeMap<&str, String> = all.iter().map(|(id, at)| (id.as_str(), at.to_rfc3339())).collect();
    Ok(Some(toml::to_string(&raw).unwrap_or_default()))
}

/// Shared across this crate's own tests (`schedule`, `source`, `module`): a source file's text
/// from one flat JSON fixture, the same shape tests wrote before sources moved to files, split
/// into `[source]`/`[kind]` by whether a key is part of the shared envelope.
#[cfg(test)]
pub(crate) mod test_support {
    use shimmer_core::testing::TestEnv;

    use super::*;

    const ENVELOPE_KEYS: &[&str] = &["label", "enabled", "interval", "kind", "title_contains", "title_excludes"];

    pub(crate) fn source_file(fields: &Value) -> String {
        let mut source = toml::Table::new();
        let mut kind = toml::Table::new();
        for (k, v) in fields.as_object().expect("test fixture must be an object") {
            let tv: toml::Value = serde_json::from_value(v.clone()).expect("test fixture value must convert to toml");
            if ENVELOPE_KEYS.contains(&k.as_str()) { &mut source } else { &mut kind }.insert(k.clone(), tv);
        }
        let mut file = toml::Table::new();
        file.insert("source".into(), toml::Value::Table(source));
        file.insert("kind".into(), toml::Value::Table(kind));
        toml::to_string(&file).unwrap()
    }

    pub(crate) fn write_source(env: &TestEnv, id: &str, fields: Value) {
        env.ctx.store.write(&format!("{SOURCES_DIR}/{id}.toml"), source_file(&fields)).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use shimmer_core::testing::TestEnv;

    use super::test_support::{source_file, write_source};
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// A bad source file doesn't fail the whole load (#126's review, B1): it is dropped from
    /// `sources` and its reason recorded in `problems`. Returns that reason, asserting the
    /// source really was kept out of the live set.
    fn problem_for(fields: Value, id: &str) -> String {
        SourceConfig::parse(id, &source_file(&fields)).expect_err("this source must fail to parse")
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
        let why = problem_for(json!({"enabled": true, "interval": 3600}), "weworkremotely");
        assert!(why.contains(&MIN_INTERVAL_SCRAPE_S.to_string()), "{why}");

        // The identical interval is fine for an API source -- the floor is per-`Method`, not
        // one uniform number.
        SourceConfig::parse("hn-whoishiring", &source_file(&json!({"enabled": true, "interval": 3600}))).unwrap();
    }

    #[test]
    fn a_folder_with_no_source_files_at_all_is_empty_not_an_error() {
        let env = TestEnv::new("fetchers");
        let loaded = Sources::load(&env.ctx).unwrap();
        assert!(loaded.sources.is_empty() && loaded.problems.is_empty());
    }

    #[test]
    fn load_reads_every_source_file_and_reports_one_broken_one_without_losing_the_rest() {
        let env = TestEnv::new("fetchers");
        write_source(
            &env,
            "good",
            json!({"enabled": true, "interval": "daily", "kind": "rss", "url": "https://example.test/f.xml"}),
        );
        write_source(&env, "bad", json!({"enabled": true, "interval": "daily", "kind": "rss", "url": "not-a-url"}));
        let loaded = Sources::load(&env.ctx).unwrap();
        assert_eq!(loaded.sources.keys().collect::<Vec<_>>(), vec!["good"]);
        assert_eq!(loaded.problems.len(), 1);
        assert!(loaded.problems.get("bad").is_some_and(|p| p.contains("url")));
    }

    #[test]
    fn an_unreadable_source_file_is_reported_without_a_toml_parser_panic() {
        let env = TestEnv::new("fetchers");
        env.ctx.store.write(&format!("{SOURCES_DIR}/broken.toml"), "this is not [[[ valid toml").unwrap();
        let loaded = Sources::load(&env.ctx).unwrap();
        assert!(loaded.sources.is_empty());
        assert!(loaded.problems.get("broken").is_some_and(|p| p.contains("not a source file")));
    }

    #[test]
    fn a_non_utf8_source_file_is_a_problem_not_a_load_failure() {
        // #303's review, blocking: `Sources::load` used `read_string`, which errors on
        // non-UTF-8 and propagated via `?`, failing `Module::init` -- and with it every
        // other source -- over one bad byte sequence in one file.
        let env = TestEnv::new("fetchers");
        env.ctx.store.write(&format!("{SOURCES_DIR}/latin1.toml"), vec![b's', 0xe9]).unwrap();
        let loaded = Sources::load(&env.ctx).expect("one non-UTF-8 file must not fail the whole load");
        assert!(loaded.sources.is_empty());
        assert!(loaded.problems.get("latin1").is_some_and(|p| p.contains("UTF-8")), "{:?}", loaded.problems);
    }

    #[test]
    fn a_title_filter_round_trips_through_a_file() {
        let cfg = SourceConfig::parse(
            "hn-whoishiring",
            &source_file(&json!({
                "enabled": true, "interval": "daily",
                "title_contains": ["junior", "entry level"],
                "title_excludes": ["senior", "staff"],
            })),
        )
        .unwrap();
        assert_eq!(cfg.title_contains, vec!["junior".to_string(), "entry level".to_string()]);
        assert_eq!(cfg.title_excludes, vec!["senior".to_string(), "staff".to_string()]);
    }

    #[test]
    fn a_typo_d_field_on_a_fixed_source_is_rejected_not_silently_ignored() {
        // #302's review: dropping the struct-level `deny_unknown_fields` (needed for
        // `kind_params`'s `#[serde(flatten)]`) stopped catching a typo on a *fixed*,
        // kind-less source -- `title_exclude` (missing the `s`) used to be refused by
        // master and loaded silently here, with the filter just never applied.
        let why =
            problem_for(json!({"enabled": true, "interval": "daily", "title_exclude": ["intern"]}), "hn-whoishiring");
        assert!(why.contains("title_exclude"), "{why}");
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
        let why = problem_for(json!({"enabled": true, "interval": "fortnightly"}), "hn-whoishiring");
        assert!(why.contains("interval"), "{why}");
    }

    #[test]
    fn an_unknown_field_in_the_source_table_is_rejected() {
        let err = SourceConfig::parse(
            "hn-whoishiring",
            "[source]\nenabled = true\ninterval = \"daily\"\ntick_interval_s = 60\n",
        )
        .unwrap_err();
        assert!(err.contains("tick_interval_s") || err.contains("unknown field"), "{err}");
    }

    #[test]
    fn a_misspelled_source_id_fails_at_load_time_not_on_the_first_tick() {
        let why = problem_for(json!({"enabled": true, "interval": "daily"}), "hn-whoishirign");
        assert!(why.contains("hn-whoishiring"), "the reason should name the real ids: {why}");
    }

    #[test]
    fn a_configured_rss_kind_with_a_url_parses() {
        let cfg = SourceConfig::parse(
            "my-team-blog",
            &source_file(
                &json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/feed.xml"}),
            ),
        )
        .unwrap();
        assert_eq!(cfg.kind.as_deref(), Some("rss"));
    }

    #[test]
    fn an_rss_kind_with_no_url_fails_at_load_time() {
        let why = problem_for(json!({"enabled": true, "interval": "hourly", "kind": "rss"}), "my-team-blog");
        assert!(why.contains("url"), "{why}");
    }

    #[test]
    fn an_rss_kind_with_a_non_http_url_fails_at_load_time() {
        let why = problem_for(
            json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "not-a-url"}),
            "my-team-blog",
        );
        assert!(why.contains("url"), "{why}");
    }

    #[test]
    fn an_rss_kind_with_a_non_http_scheme_url_fails_at_load_time() {
        let why = problem_for(
            json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "ftp://example.test/feed.xml"}),
            "my-team-blog",
        );
        assert!(why.contains("url"), "{why}");
    }

    #[test]
    fn an_rss_kind_with_an_https_url_parses() {
        let cfg = SourceConfig::parse(
            "my-team-blog",
            &source_file(
                &json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/feed.xml"}),
            ),
        )
        .unwrap();
        assert_eq!(cfg.kind_params.get("url").and_then(Value::as_str), Some("https://example.test/feed.xml"));
    }

    #[test]
    fn a_kind_entry_whose_id_collides_with_a_fixed_source_fails_at_load_time() {
        for id in ["hn-whoishiring", "weworkremotely"] {
            let why = problem_for(
                json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/feed.xml"}),
                id,
            );
            assert!(why.contains("collides"), "id {id}: {why}");
        }
    }

    #[test]
    fn an_unknown_kind_is_rejected() {
        let why = problem_for(json!({"enabled": true, "interval": "hourly", "kind": "carrier-pigeon"}), "my-team-blog");
        assert!(why.contains("carrier-pigeon"), "{why}");
    }

    #[test]
    fn a_json_api_kind_with_every_required_field_parses() {
        let cfg = SourceConfig::parse(
            "figma-jobs",
            &source_file(&json!({
                "enabled": true, "interval": "daily", "kind": "json-api",
                "url": "https://boards-api.greenhouse.io/v1/boards/figma/jobs",
                "items_path": "jobs", "id_field": "id", "title_field": "title", "url_field": "absolute_url",
            })),
        )
        .unwrap();
        assert_eq!(cfg.kind.as_deref(), Some("json-api"));
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
            let why = problem_for(fields, "acme-jobs");
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
            fields
        };

        let why = problem_for(raw(json!({"page_size": 0})), "acme-jobs");
        assert!(why.contains("page_size"), "{why}");

        let why = problem_for(raw(json!({"page_size": 50, "url": "https://example.test/jobs"})), "acme-jobs");
        assert!(why.contains("{offset}"), "{why}");

        let good = SourceConfig::parse(
            "acme-jobs",
            &source_file(&raw(json!({"page_size": 50, "url": "https://example.test/jobs?offset={offset}"}))),
        )
        .unwrap();
        assert_eq!(good.kind.as_deref(), Some("json-api"));
    }

    #[test]
    fn a_json_api_kind_with_an_unknown_pagination_is_rejected() {
        let why = problem_for(
            json!({
                "enabled": true, "interval": "daily", "kind": "json-api",
                "url": "https://example.test/jobs", "id_field": "id", "title_field": "title", "url_field": "url",
                "pagination": "cursor",
            }),
            "acme-jobs",
        );
        assert!(why.contains("cursor"), "{why}");
    }

    #[test]
    fn a_json_api_kind_takes_an_optional_active_field() {
        let cfg = SourceConfig::parse(
            "acme-jobs",
            &source_file(&json!({
                "enabled": true, "interval": "daily", "kind": "json-api",
                "url": "https://example.test/jobs", "id_field": "id", "title_field": "title", "url_field": "url",
                "active_field": "active",
            })),
        )
        .unwrap();
        assert_eq!(cfg.kind_params.get("active_field").and_then(Value::as_str), Some("active"));
    }

    #[test]
    fn a_json_api_kind_rejects_a_blank_active_field() {
        let why = problem_for(
            json!({
                "enabled": true, "interval": "daily", "kind": "json-api",
                "url": "https://example.test/jobs", "id_field": "id", "title_field": "title", "url_field": "url",
                "active_field": "  ",
            }),
            "acme-jobs",
        );
        assert!(why.contains("active_field"), "{why}");
    }

    fn source(enabled: bool, interval_s: u64) -> SourceConfig {
        SourceConfig {
            label: None,
            enabled,
            interval: IntervalSpec::Seconds(interval_s),
            kind: None,
            title_contains: Vec::new(),
            title_excludes: Vec::new(),
            kind_params: Map::new(),
        }
    }

    #[test]
    fn a_never_run_enabled_source_is_due() {
        let mut sources = BTreeMap::new();
        sources.insert("hn-whoishiring".into(), source(true, MIN_INTERVAL_API_S));
        let due = due_sources(&sources, &BTreeMap::new(), at("2026-01-01T00:00:00Z"));
        assert_eq!(due, vec!["hn-whoishiring".to_string()]);
    }

    #[test]
    fn a_disabled_source_is_never_due() {
        let mut sources = BTreeMap::new();
        sources.insert("hn-whoishiring".into(), source(false, MIN_INTERVAL_API_S));
        let due = due_sources(&sources, &BTreeMap::new(), at("2026-01-01T00:00:00Z"));
        assert!(due.is_empty());
    }

    #[test]
    fn a_source_run_within_its_interval_is_not_due_yet() {
        let mut sources = BTreeMap::new();
        sources.insert("hn-whoishiring".into(), source(true, 3600));
        let mut last = BTreeMap::new();
        last.insert("hn-whoishiring".to_string(), at("2026-01-01T00:00:00Z"));
        let due = due_sources(&sources, &last, at("2026-01-01T00:30:00Z"));
        assert!(due.is_empty(), "only 30 minutes have passed of a 1-hour interval");
    }

    #[test]
    fn a_source_run_past_its_interval_is_due_again() {
        let mut sources = BTreeMap::new();
        sources.insert("hn-whoishiring".into(), source(true, 3600));
        let mut last = BTreeMap::new();
        last.insert("hn-whoishiring".to_string(), at("2026-01-01T00:00:00Z"));
        let due = due_sources(&sources, &last, at("2026-01-01T01:00:01Z"));
        assert_eq!(due, vec!["hn-whoishiring".to_string()]);
    }

    #[test]
    fn one_bad_source_does_not_stop_the_good_ones() {
        // #126's review, B1: this used to make loading -- and so every `fetchers.tick`,
        // `list` and `status` -- fail outright, so one typo stopped all fetching and (with
        // the built-in scheduled-task-failed alert) popped a high-priority notification every
        // five minutes.
        let env = TestEnv::new("fetchers");
        write_source(
            &env,
            "good",
            json!({"enabled": true, "interval": "daily", "kind": "rss", "url": "https://example.test/f.xml"}),
        );
        write_source(&env, "bad", json!({"enabled": true, "interval": "daily", "kind": "rss", "url": "not-a-url"}));
        write_source(&env, "also-bad", json!({"enabled": true, "interval": "daily", "kind": "carrier-pigeon"}));
        env.ctx
            .store
            .write(&format!("{SOURCES_DIR}/wrong-type.toml"), "[source]\nenabled = true\ninterval = {}\n")
            .unwrap();
        let loaded = Sources::load(&env.ctx).unwrap();
        assert_eq!(loaded.sources.keys().collect::<Vec<_>>(), vec!["good"], "the good source still loads");
        assert_eq!(loaded.problems.len(), 3, "each bad source is reported on its own: {:?}", loaded.problems);
        for id in ["bad", "also-bad", "wrong-type"] {
            assert!(loaded.problems.contains_key(id), "{id} should be reported: {:?}", loaded.problems);
        }

        // And the good one is still schedulable.
        let due = due_sources(&loaded.sources, &BTreeMap::new(), at("2026-01-01T00:00:00Z"));
        assert_eq!(due, vec!["good".to_string()]);
    }

    #[test]
    fn an_empty_filter_term_is_rejected_rather_than_matching_every_title() {
        // #126's review, B3: `--excludes senior,` becomes ["senior", ""], and "" is a substring
        // of every title, so everything got excluded. Same the other way for `title_contains`.
        for field in ["title_excludes", "title_contains"] {
            let why =
                problem_for(json!({"enabled": true, "interval": "daily", field: ["senior", ""]}), "hn-whoishiring");
            assert!(why.contains(field), "{field}: {why}");
        }
        // Whitespace-only counts as empty.
        problem_for(json!({"enabled": true, "interval": "daily", "title_contains": ["  "]}), "hn-whoishiring");
    }

    #[test]
    fn a_source_id_that_cannot_be_a_folder_name_is_rejected_at_load() {
        // #126's review, B14: the store's own path check caught this at the first fetch, but an
        // id should be checked where every other user-named thing is -- at load.
        for id in ["../records", "has space", "UPPER", "has/slash"] {
            let why = problem_for(
                json!({"enabled": true, "interval": "daily", "kind": "rss", "url": "https://e.test/f"}),
                id,
            );
            assert!(why.contains("source id"), "id {id}: {why}");
        }
    }

    #[test]
    fn a_last_run_in_the_future_reads_as_due_rather_than_stalling_the_source() {
        // #126's review, B10: a clock correction, or a $SHIMMER_HOME copied from a machine
        // running ahead, parked a source until that future time actually arrived.
        let mut sources = BTreeMap::new();
        sources.insert("hn-whoishiring".into(), source(true, 3600));
        let mut last = BTreeMap::new();
        last.insert("hn-whoishiring".to_string(), at("2026-02-01T00:00:00Z"));
        let due = due_sources(&sources, &last, at("2026-01-01T00:00:00Z"));
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

    #[test]
    fn render_builds_a_file_that_parses_back_to_the_same_fields() {
        let text = render(
            Some("My Team's Blog"),
            Some("rss"),
            true,
            &IntervalSpec::Preset("hourly".into()),
            &json!({"url": "https://example.test/feed.xml"}).as_object().unwrap().clone(),
            &["junior".to_string()],
            &["staff".to_string()],
        )
        .unwrap();
        let cfg = SourceConfig::parse("my-team-blog", &text).unwrap();
        assert_eq!(cfg.label.as_deref(), Some("My Team's Blog"));
        assert_eq!(cfg.kind.as_deref(), Some("rss"));
        assert!(cfg.enabled);
        assert_eq!(cfg.interval, IntervalSpec::Preset("hourly".into()));
        assert_eq!(cfg.kind_params.get("url").and_then(Value::as_str), Some("https://example.test/feed.xml"));
        assert_eq!(cfg.title_contains, vec!["junior".to_string()]);
        assert_eq!(cfg.title_excludes, vec!["staff".to_string()]);
    }

    #[test]
    fn render_omits_label_and_kind_entirely_when_absent_so_a_built_in_source_still_parses() {
        let text =
            render(None, None, false, &IntervalSpec::Seconds(MIN_INTERVAL_API_S), &Map::new(), &[], &[]).unwrap();
        let cfg = SourceConfig::parse("hn-whoishiring", &text).unwrap();
        assert_eq!(cfg.label, None);
        assert_eq!(cfg.kind, None);
    }
}
