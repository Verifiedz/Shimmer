//! Per-source scheduling, via `ctx.schedules` (ADR 0028 §6), and last-run bookkeeping.
//!
//! There is no fixed heartbeat trigger: each source schedules its own next run directly
//! (`Module::triggers()` stays empty) -- see `module.rs`'s `schedule_run`/`cancel_schedule`/
//! `reconcile_schedule` for when that happens. This file owns only the per-source state that
//! decision needs: the pending schedule itself (`ScheduleState`) and `last_run.toml`.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use shimmer_core::filter::Filter;
use shimmer_core::{Ctx, Error, Result};

/// The real fetch work, queued (ADR 0028 §5) -- internal; `fetchers.run` (inline) checks the
/// source exists, then enqueues this. Also what a source's own schedule fires (ADR 0028 §6).
pub const RUN_TASK_OP: &str = "fetchers.run_task";

/// Floors below which a source's own cadence may not be set, per `Method` (ADR 0028 §5/§7).
/// An API source hits a known, source-maintained endpoint built for this kind of polling; a
/// scraped page is a much heavier, more easily-noticed load on a host that never agreed to
/// be polled, so it gets a much higher floor.
pub const MIN_INTERVAL_API_S: u64 = 1800;
pub const MIN_INTERVAL_SCRAPE_S: u64 = 21_600;

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

/// `data/fetchers/sources/<id>.state.toml` (ADR 0028 §6): `{trigger_id, due_at}` for whatever
/// `ctx.schedules` entry is currently pending for this source. Written by the module, never
/// hand-edited, same spirit as workspaces' own `state.toml`; [`Sources::load`] skips it rather
/// than trying to parse it as a source.
fn schedule_state_path(id: &str) -> String {
    format!("{SOURCES_DIR}/{id}.state.toml")
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScheduleState {
    pub trigger_id: String,
    pub due_at: DateTime<Utc>,
}

/// `None` when this source has no pending schedule (never enabled, or disabled/removed since).
pub(crate) fn load_schedule_state(ctx: &Ctx, id: &str) -> Result<Option<ScheduleState>> {
    let Some(text) = ctx.store.read_string(&schedule_state_path(id))? else { return Ok(None) };
    let raw: BTreeMap<String, String> =
        toml::from_str(&text).map_err(|e| Error::internal(format!("fetchers {id}.state.toml: {e}")))?;
    let bad = || Error::internal(format!("fetchers {id}.state.toml: missing trigger_id or due_at"));
    let trigger_id = raw.get("trigger_id").cloned().ok_or_else(bad)?;
    let due_at = raw
        .get("due_at")
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .ok_or_else(bad)?;
    Ok(Some(ScheduleState { trigger_id, due_at }))
}

pub(crate) fn save_schedule_state(ctx: &Ctx, id: &str, trigger_id: &str, due_at: DateTime<Utc>) -> Result<()> {
    let mut raw = BTreeMap::new();
    raw.insert("trigger_id", trigger_id.to_owned());
    raw.insert("due_at", due_at.to_rfc3339());
    ctx.store.write(&schedule_state_path(id), toml::to_string(&raw).unwrap_or_default())
}

pub(crate) fn delete_schedule_state(ctx: &Ctx, id: &str) -> Result<()> {
    ctx.store.delete(&schedule_state_path(id))
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
            // A source's own schedule state (ADR 0028 §6) is a sibling `<id>.state.toml` in
            // this same folder, not a source -- skipped here, read only through
            // `load_schedule_state`.
            if file.ends_with(".state.toml") {
                continue;
            }
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
    /// Every source names a kind (ADR 0028 §10 -- no fixed, compiled-in registry): built from
    /// this source's own `[kind]` table via `crate::kind::kind` (ADR 0028 §2a/§3).
    pub kind: String,
    /// A `core::filter::Filter` (ADR 0028 §8), checked against `{"title": …}` only -- a source
    /// whose `raw_data` carries a longer description is not searched. `None` keeps everything.
    /// Kept as raw JSON, not a parsed `Filter` (which has no way back to JSON), so
    /// `view()`/`fetchers.show`'s `text` can show it unchanged; already validated once, at
    /// parse time, via [`matches_title`].
    pub filter: Option<Map<String, Value>>,
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
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filter: Option<toml::Table>,
}

/// Builds a source file's TOML text from `fetchers.new`/`.edit`'s structured params (ADR 0028
/// §9) -- the mechanical inverse of [`SourceConfig::parse`], so a structured command, a
/// template (later), and a hand-edited-then-`fetchers.reload`ed file all produce and accept
/// the identical on-disk shape. Validation stays centralized in `SourceConfig::parse`, called
/// on this function's own output by the caller -- this function only renders text, it never
/// decides whether that text is valid.
pub(crate) fn render(
    label: Option<&str>,
    kind: &str,
    enabled: bool,
    interval: &IntervalSpec,
    params: &Map<String, Value>,
    filter: Option<&Map<String, Value>>,
) -> std::result::Result<String, String> {
    let as_table = |m: &Map<String, Value>, what: &str| match toml::Value::try_from(m) {
        Ok(toml::Value::Table(t)) => Ok(t),
        Ok(_) => unreachable!("a JSON object always serializes to a toml table"),
        Err(e) => Err(format!("{what}: {e}")),
    };
    let kind_table = as_table(params, "params")?;
    let filter_table = filter.map(|f| as_table(f, "filter")).transpose()?;
    let file = SourceFile {
        source: SourceMeta {
            label: label.map(str::to_owned),
            enabled,
            interval: interval.clone(),
            kind: kind.to_owned(),
            filter: filter_table,
        },
        kind: kind_table,
    };
    toml::to_string(&file).map_err(|e| format!("internal: {e}"))
}

impl SourceConfig {
    /// `id`'s file text, checked: its id's own charset, its `filter` (a `core::filter::Filter`,
    /// ADR 0028 §8), and -- the bulk of it -- its `kind`'s own fields, dispatched through
    /// `crate::kind::kind` (ADR 0028 §3). One sentence naming what's wrong, the same
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
        let filter = file
            .source
            .filter
            .as_ref()
            .map(|t| serde_json::to_value(t).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default());
        if let Some(f) = &filter {
            only_title_keys(f)?;
            // `Filter::parse`'s own errors already name the key ("filter on 'title': …").
            Filter::parse(f)?;
        }
        let kind_params =
            serde_json::to_value(&file.kind).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default();
        // Every source names a `kind` (ADR 0028 §10: no fixed, compiled-in registry any
        // more -- `hn-whoishiring`/`wwr` are ordinary kinds too). Each `Method` gets its own
        // floor (ADR 0028 §5/§7) -- a scraped page must not be pollable at an API source's
        // much lower cadence (PR #126 review). `Kind::method` is static per kind, so this
        // needs no built `Source` to ask.
        let kind_name = &file.source.kind;
        let kind = crate::kind::kind(kind_name)
            .ok_or_else(|| format!("unknown kind \"{kind_name}\" (known: {})", crate::kind::known_names()))?;
        kind.parse(&kind_params).map_err(|m| format!("kind \"{kind_name}\" {m}"))?;
        let floor = match kind.method() {
            crate::source::Method::Api => MIN_INTERVAL_API_S,
            crate::source::Method::Scrape => MIN_INTERVAL_SCRAPE_S,
        };
        file.source.interval.resolve(floor).map_err(|e| format!("interval: {e}"))?;
        Ok(Self {
            label: file.source.label,
            enabled: file.source.enabled,
            interval: file.source.interval,
            kind: file.source.kind,
            filter,
            kind_params,
        })
    }

    /// Whether an item's title should be kept, given this source's own filter. See
    /// [`matches_title`] for the matching rule itself.
    pub fn matches_title(&self, title: &str, today: NaiveDate, tz: Tz) -> bool {
        matches_title(self.filter.as_ref(), title, today, tz)
    }
}

/// `filter` must check only `title`, plain or inside an `"or"` alternative -- `Filter` has no
/// schema behind it (its own doc comment), so a key like `company = {"contains": "x"}` parses
/// as a perfectly good filter and then silently matches nothing, every time, once it's
/// checked against `{"title": …}` only below (#311's review, blocking; #299's review point
/// 5). Shared by [`SourceConfig::parse`] (a source's own file) and `module::validate_filter`
/// (an ad-hoc `fetchers.run`/`.test`/`.new` override), so neither path can drift from the
/// other's idea of what a filter here is allowed to name.
pub(crate) fn only_title_keys(filter: &Map<String, Value>) -> std::result::Result<(), String> {
    for (key, value) in filter {
        if key == "or" {
            // A malformed "or" (not a list, an empty alternative) is left to `Filter::parse`'s
            // own, better-worded error -- this only needs to recurse into whatever alternatives
            // *are* shaped like filters.
            if let Some(list) = value.as_array() {
                for alt in list {
                    if let Some(obj) = alt.as_object() {
                        only_title_keys(obj)?;
                    }
                }
            }
            continue;
        }
        if key != "title" {
            return Err(format!("filter on '{key}': only 'title' is checked against an item; nothing else matches"));
        }
    }
    Ok(())
}

/// Whether `title` passes `filter` (ADR 0028 §8) -- `None` keeps everything. A free function,
/// not only [`SourceConfig::matches_title`], so an ad-hoc CLI override can run the identical
/// rule against a filter that never came from a source's own file at all. `filter` is already
/// validated (by `SourceConfig::parse`, or by the ad-hoc override's own check before this is
/// ever called) -- a parse failure here would mean that validation has a gap, so it reads as
/// "keep everything," the same safe direction an unset value already takes.
pub fn matches_title(filter: Option<&Map<String, Value>>, title: &str, today: NaiveDate, tz: Tz) -> bool {
    match filter {
        None => true,
        Some(f) => match Filter::parse(f) {
            Ok(parsed) => parsed.matches(&json!({"title": title}), today, tz),
            Err(_) => true,
        },
    }
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

/// `data/fetchers/last_run.toml` -- one small file for every source, read-modified-written
/// together rather than one file per source.
pub(crate) const LAST_RUN_PATH: &str = "last_run.toml";

/// One source's entry in `last_run.toml`: when it last ran, and -- when that run failed --
/// why (ADR 0028 §11). `error`/`reason` are `None` exactly when the most recent run
/// succeeded; the next successful run clears them, so this is always the *most recent*
/// outcome, never a history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct LastRunEntry {
    at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// A corrupt or unparseable `last_run.toml` is treated as empty, never an error (#126's
/// review, B1). The next successful run rewrites the file cleanly.
fn load_raw(ctx: &Ctx) -> BTreeMap<String, LastRunEntry> {
    let Ok(Some(text)) = ctx.store.read_string(LAST_RUN_PATH) else { return BTreeMap::new() };
    toml::from_str(&text).unwrap_or_default()
}

fn write_raw(ctx: &Ctx, all: &BTreeMap<String, LastRunEntry>) -> Result<()> {
    ctx.store.write(LAST_RUN_PATH, toml::to_string(all).unwrap_or_default())
}

/// Individual unparseable *entries* (an unreadable timestamp) are dropped, keeping the good
/// ones -- that source reads as due, which is the safe direction.
pub fn load_last_run(ctx: &Ctx) -> Result<BTreeMap<String, DateTime<Utc>>> {
    Ok(load_raw(ctx)
        .into_iter()
        .filter_map(|(id, e)| DateTime::parse_from_rfc3339(&e.at).ok().map(|at| (id, at.with_timezone(&Utc))))
        .collect())
}

/// Every source whose most recent run failed, with when and why (ADR 0028 §11) --
/// `fetchers.health`'s own reading of `last_run.toml`. Cleared by that source's next
/// successful run; an entry with an unreadable timestamp is dropped, same as
/// [`load_last_run`].
pub fn load_last_failures(ctx: &Ctx) -> BTreeMap<String, (DateTime<Utc>, String, String)> {
    load_raw(ctx)
        .into_iter()
        .filter_map(|(id, e)| {
            let (error, reason) = (e.error?, e.reason?);
            let at = DateTime::parse_from_rfc3339(&e.at).ok()?.with_timezone(&Utc);
            Some((id, (at, error, reason)))
        })
        .collect()
}

/// Why `last_run.toml` could not be read, if it exists and is unreadable -- reported by
/// `fetchers.status` so a corrupt bookkeeping file is visible (#126's review, B1) instead of
/// silently making every source look due forever. Read on the status path only; the fetch path
/// deliberately doesn't care, because "treat everything as due" is already the safe answer.
pub fn last_run_problem(ctx: &Ctx) -> Option<String> {
    let text = ctx.store.read_string(LAST_RUN_PATH).ok()??;
    match toml::from_str::<BTreeMap<String, LastRunEntry>>(&text) {
        Ok(raw) => raw
            .iter()
            .find(|(_, e)| DateTime::parse_from_rfc3339(&e.at).is_err())
            .map(|(id, _)| format!("last_run.toml: '{id}' has an unreadable timestamp; that source reads as due")),
        Err(e) => Some(format!("last_run.toml is unreadable ({e}); every source reads as due")),
    }
}

/// Records that `source` ran, successfully, at `now` -- clearing any previous failure of
/// its (ADR 0028 §11: health reports only the *most recent* outcome). Every other source's
/// record is left untouched.
pub fn record_run(ctx: &Ctx, source: &str, now: DateTime<Utc>) -> Result<()> {
    let mut all = load_raw(ctx);
    all.insert(source.to_owned(), LastRunEntry { at: now.to_rfc3339(), error: None, reason: None });
    write_raw(ctx, &all)
}

/// Records that `source`'s run at `now` failed, with `error` (`Error::message`) and `reason`
/// (`fetchers.fetch.failed`'s own classification, ADR 0028 §9) -- the same payload shape that
/// event already carries, so `fetchers.health` needs no new failure taxonomy of its own.
pub fn record_failure(ctx: &Ctx, source: &str, now: DateTime<Utc>, error: &str, reason: &str) -> Result<()> {
    let mut all = load_raw(ctx);
    all.insert(
        source.to_owned(),
        LastRunEntry { at: now.to_rfc3339(), error: Some(error.to_owned()), reason: Some(reason.to_owned()) },
    );
    write_raw(ctx, &all)
}

/// `last_run.toml`'s text with `from`'s entry moved to `to`, if `from` had one; `None` if
/// `from` never ran -- a never-run source being renamed needs no `last_run.toml` write at all
/// (ADR 0028 §7). The caller puts the result in the same `ctx.store.transaction` as the
/// source file's own move, so the rename is one atomic unit (CLAUDE.md §7.1). Carries a
/// pending failure (if any) over to the new id too -- a rename is not a successful run.
pub(crate) fn rename_last_run(ctx: &Ctx, from: &str, to: &str) -> Result<Option<String>> {
    let mut all = load_raw(ctx);
    let Some(entry) = all.remove(from) else { return Ok(None) };
    all.insert(to.to_owned(), entry);
    Ok(Some(toml::to_string(&all).unwrap_or_default()))
}

/// `last_run.toml`'s text with every id in `ids` dropped, if any of them had an entry;
/// `None` if none did -- a purge of sources that never ran needs no `last_run.toml` write at
/// all. Same "the caller puts this in its own transaction" contract as [`rename_last_run`]
/// (#305's review, should-fix: `purge` left this behind, so a new source reusing a purged
/// id inherited its old last-run time and had its first run delayed).
pub(crate) fn remove_last_run(ctx: &Ctx, ids: &[String]) -> Result<Option<String>> {
    let mut all = load_last_run(ctx)?;
    let mut changed = false;
    for id in ids {
        changed |= all.remove(id).is_some();
    }
    if !changed {
        return Ok(None);
    }
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

    const ENVELOPE_KEYS: &[&str] = &["label", "enabled", "interval", "kind", "filter"];

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
        let why = problem_for(
            json!({"enabled": true, "interval": 3600, "kind": "wwr", "category": "remote-full-stack-programming-jobs"}),
            "my-wwr",
        );
        assert!(why.contains(&MIN_INTERVAL_SCRAPE_S.to_string()), "{why}");

        // The identical interval is fine for an API source -- the floor is per-`Method`, not
        // one uniform number.
        SourceConfig::parse(
            "my-hn",
            &source_file(&json!({"enabled": true, "interval": 3600, "kind": "hn-whoishiring"})),
        )
        .unwrap();
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

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()
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
    fn a_filter_round_trips_through_a_file() {
        let cfg = SourceConfig::parse(
            "my-hn",
            &source_file(&json!({
                "enabled": true, "interval": "daily", "kind": "hn-whoishiring",
                "filter": {"or": [{"title": {"contains": "junior"}}, {"title": {"contains": "entry level"}}],
                           "title": {"not_contains": "senior"}},
            })),
        )
        .unwrap();
        assert!(cfg.matches_title("Junior Engineer", today(), chrono_tz::UTC));
        assert!(!cfg.matches_title("Senior Engineer", today(), chrono_tz::UTC));
    }

    #[test]
    fn a_filter_on_any_field_but_title_is_rejected_at_load_not_silently_matching_nothing() {
        // #311's review, blocking: `Filter` has no schema, so `company = {"contains": "Acme"}`
        // used to load fine and then match nothing, forever -- the filter is checked against
        // `{"title": …}` only. Same for a key inside an "or" alternative.
        let why = problem_for(
            json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring", "filter": {"company": {"contains": "Acme"}}}),
            "hn-whoishiring",
        );
        assert!(why.contains("company"), "{why}");

        let why = problem_for(
            json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring", "filter": {"or": [{"company": {"contains": "Acme"}}]}}),
            "hn-whoishiring",
        );
        assert!(why.contains("company"), "{why}");
    }

    #[test]
    fn matches_title_with_no_filter_configured_keeps_everything() {
        let s = source(true, MIN_INTERVAL_API_S);
        assert!(s.matches_title("Senior Staff Engineer", today(), chrono_tz::UTC));
        assert!(s.matches_title("", today(), chrono_tz::UTC));
    }

    #[test]
    fn matches_title_requires_one_of_contains_case_insensitively() {
        let mut s = source(true, MIN_INTERVAL_API_S);
        s.filter = Some(
            json!({"or": [{"title": {"contains": "junior"}}, {"title": {"contains": "entry level"}}]})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert!(s.matches_title("Junior Software Engineer", today(), chrono_tz::UTC));
        assert!(s.matches_title("JUNIOR SOFTWARE ENGINEER", today(), chrono_tz::UTC), "must be case-insensitive");
        assert!(s.matches_title("Entry Level Developer", today(), chrono_tz::UTC));
        assert!(!s.matches_title("Senior Software Engineer", today(), chrono_tz::UTC), "matches neither term");
    }

    #[test]
    fn matches_title_excludes_always_wins_over_contains() {
        let mut s = source(true, MIN_INTERVAL_API_S);
        s.filter =
            Some(json!({"title": {"contains": "engineer", "not_contains": "senior"}}).as_object().unwrap().clone());
        assert!(s.matches_title("Junior Engineer", today(), chrono_tz::UTC));
        assert!(
            !s.matches_title("Senior Engineer", today(), chrono_tz::UTC),
            "excludes must win even though 'engineer' matches contains"
        );
    }

    #[test]
    fn a_bad_interval_fails_at_load_time_not_on_the_first_tick() {
        let why = problem_for(json!({"enabled": true, "interval": "fortnightly", "kind": "hn-whoishiring"}), "my-hn");
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
    fn a_source_with_no_kind_at_all_fails_at_load_time() {
        // ADR 0028 §10 -- there is no fixed, compiled-in registry to fall back to; every
        // source must name a kind, however it is spelled.
        let why = problem_for(json!({"enabled": true, "interval": "daily"}), "my-hn");
        assert!(why.contains("kind"), "{why}");
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
        assert_eq!(cfg.kind, "rss");
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
        assert_eq!(cfg.kind, "json-api");
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
        assert_eq!(good.kind, "json-api");
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
            kind: "rss".into(),
            filter: None,
            kind_params: Map::new(),
        }
    }

    #[test]
    fn one_bad_source_does_not_stop_the_good_ones() {
        // #126's review, B1: this used to make loading -- and so every `fetchers.list`/
        // `.status` -- fail outright, so one typo stopped all fetching.
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
    }

    #[test]
    fn a_sources_own_state_toml_is_never_mistaken_for_a_source() {
        let env = TestEnv::new("fetchers");
        write_source(
            &env,
            "good",
            json!({"enabled": true, "interval": "daily", "kind": "rss", "url": "https://example.test/f.xml"}),
        );
        save_schedule_state(&env.ctx, "good", "trig-1", at("2026-01-01T00:00:00Z")).unwrap();
        let loaded = Sources::load(&env.ctx).unwrap();
        assert_eq!(loaded.sources.keys().collect::<Vec<_>>(), vec!["good"]);
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
    }

    #[test]
    fn an_empty_filter_operator_value_is_rejected_at_load_time() {
        // #126's review, B3's original concern ("" is a substring of every title, so an empty
        // term matched/excluded everything) is now `core::filter::Op::parse`'s own job --
        // `contains`/`not_contains` already refuse an empty value.
        for op in ["contains", "not_contains"] {
            let mut title = Map::new();
            title.insert(op.to_owned(), json!(""));
            let mut filter = Map::new();
            filter.insert("title".to_owned(), Value::Object(title));
            let mut fields = json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"});
            fields.as_object_mut().unwrap().insert("filter".to_owned(), Value::Object(filter));
            let why = problem_for(fields, "my-hn");
            assert!(why.contains(op), "{op}: {why}");
        }
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
    fn a_corrupt_last_run_file_does_not_stop_every_source() {
        let env = TestEnv::new("fetchers");
        env.ctx.store.write(LAST_RUN_PATH, "this is not: valid toml [[[").unwrap();
        assert!(load_last_run(&env.ctx).unwrap().is_empty(), "unreadable reads as empty, not an error");
        assert!(last_run_problem(&env.ctx).is_some(), "but it is reported, not silent");

        // A single unreadable entry keeps the good ones.
        env.ctx
            .store
            .write(LAST_RUN_PATH, "good = { at = \"2026-01-01T00:00:00Z\" }\nbad = { at = \"not a date\" }\n")
            .unwrap();
        let runs = load_last_run(&env.ctx).unwrap();
        assert_eq!(runs.keys().collect::<Vec<_>>(), vec!["good"]);
        assert!(last_run_problem(&env.ctx).is_some_and(|p| p.contains("bad")));
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
    fn a_failure_is_recorded_and_cleared_by_the_next_success_leaving_other_sources_alone() {
        let env = TestEnv::new("fetchers");
        record_run(&env.ctx, "blog", at("2026-01-01T00:00:00Z")).unwrap();
        record_failure(&env.ctx, "hn-whoishiring", at("2026-01-02T00:00:00Z"), "boom", "network").unwrap();

        let failures = load_last_failures(&env.ctx);
        assert_eq!(failures.len(), 1);
        let (failed_at, error, reason) = failures.get("hn-whoishiring").unwrap();
        assert_eq!(*failed_at, at("2026-01-02T00:00:00Z"));
        assert_eq!(error, "boom");
        assert_eq!(reason, "network");
        // A failed run still updates last_run_at (ADR 0028 §6: a broken source still waits a
        // full interval, not retried immediately).
        assert_eq!(load_last_run(&env.ctx).unwrap().get("hn-whoishiring"), Some(&at("2026-01-02T00:00:00Z")));
        // The other source's own record is untouched.
        assert!(!failures.contains_key("blog"));

        // The next success clears it -- health reports only the most recent outcome.
        record_run(&env.ctx, "hn-whoishiring", at("2026-01-03T00:00:00Z")).unwrap();
        assert!(load_last_failures(&env.ctx).is_empty());
    }

    #[test]
    fn renaming_a_source_carries_its_pending_failure_to_the_new_id() {
        let env = TestEnv::new("fetchers");
        record_failure(&env.ctx, "blog", at("2026-01-01T00:00:00Z"), "boom", "network").unwrap();
        let text = rename_last_run(&env.ctx, "blog", "team-blog").unwrap().expect("blog had a record");
        env.ctx.store.write(LAST_RUN_PATH, text).unwrap();

        let failures = load_last_failures(&env.ctx);
        assert!(!failures.contains_key("blog"));
        let (_, error, reason) = failures.get("team-blog").expect("the failure followed the rename");
        assert_eq!(error, "boom");
        assert_eq!(reason, "network");
    }

    #[test]
    fn render_builds_a_file_that_parses_back_to_the_same_fields() {
        let filter = json!({"title": {"contains": "junior", "not_contains": "staff"}}).as_object().unwrap().clone();
        let text = render(
            Some("My Team's Blog"),
            "rss",
            true,
            &IntervalSpec::Preset("hourly".into()),
            &json!({"url": "https://example.test/feed.xml"}).as_object().unwrap().clone(),
            Some(&filter),
        )
        .unwrap();
        let cfg = SourceConfig::parse("my-team-blog", &text).unwrap();
        assert_eq!(cfg.label.as_deref(), Some("My Team's Blog"));
        assert_eq!(cfg.kind, "rss");
        assert!(cfg.enabled);
        assert_eq!(cfg.interval, IntervalSpec::Preset("hourly".into()));
        assert_eq!(cfg.kind_params.get("url").and_then(Value::as_str), Some("https://example.test/feed.xml"));
        assert_eq!(cfg.filter, Some(filter));
    }

    #[test]
    fn render_omits_label_and_filter_entirely_when_absent_but_kind_is_always_written() {
        // ADR 0028 §10: no fixed, compiled-in registry any more -- `hn-whoishiring` is an
        // ordinary kind, with no fields of its own.
        let text = render(None, "hn-whoishiring", false, &IntervalSpec::Seconds(MIN_INTERVAL_API_S), &Map::new(), None)
            .unwrap();
        let cfg = SourceConfig::parse("my-hn", &text).unwrap();
        assert_eq!(cfg.label, None);
        assert_eq!(cfg.kind, "hn-whoishiring");
        assert_eq!(cfg.filter, None);
    }
}
