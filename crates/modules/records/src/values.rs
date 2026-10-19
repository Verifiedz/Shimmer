//! Turning what people type into what records store (ADR 0024): local times into UTC, and plain
//! dates kept as dates. Pure: the current moment and the timezone come in as a [`Now`], never from
//! a clock (§12 rule 10).

use chrono::{DateTime, LocalResult, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use chrono_tz::Tz;
use std::collections::BTreeMap;

use serde_json::{Map, Value};
use shimmer_core::{Error, Result};

use crate::schema::{Collection, FieldType};

/// The current moment and the configured timezone (ADR 0009), together, so everything that
/// needs "now" or "today" reads the same one.
#[derive(Clone, Copy, Debug)]
pub struct Now {
    pub at: DateTime<Utc>,
    pub tz: Tz,
}

impl Now {
    /// Today in the configured timezone.
    pub fn today(&self) -> NaiveDate {
        self.at.with_timezone(&self.tz).date_naive()
    }

    /// The current moment as a stored `datetime` value.
    pub fn stamp(&self) -> String {
        utc(self.at)
    }
}

/// A `datetime` value as stored: RFC 3339 in UTC, to the second.
fn utc(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// What someone typed for a `datetime` field, as stored (ADR 0024 §1): a plain date stays a plain
/// date; a time with an offset or `Z` becomes UTC; a time without one is read in `tz`.
pub fn datetime(input: &str, tz: &Tz) -> std::result::Result<String, String> {
    let s = input.trim();
    if NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok() {
        return Ok(s.to_owned());
    }
    if let Ok(at) = DateTime::parse_from_rfc3339(s) {
        return Ok(utc(at.with_timezone(&Utc)));
    }
    for format in ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(local) = NaiveDateTime::parse_from_str(s, format) {
            return match tz.from_local_datetime(&local) {
                LocalResult::Single(at) | LocalResult::Ambiguous(at, _) => Ok(utc(at.with_timezone(&Utc))),
                LocalResult::None => Err(format!("{s} doesn't exist in {tz} (the clocks skip it)")),
            };
        }
    }
    Err("a date and time like \"2026-10-20 23:59\", \"2026-10-20T23:59:00-07:00\", or a date".into())
}

/// A stored `datetime` (or `date`) value as an instant, for comparing and sorting. A plain date
/// is the start of that day in `tz`. `None` for anything else (a hand-edit gone wrong).
pub fn instant(stored: &str, tz: &Tz) -> Option<DateTime<Utc>> {
    if let Ok(at) = DateTime::parse_from_rfc3339(stored) {
        return Some(at.with_timezone(&Utc));
    }
    let day = NaiveDate::parse_from_str(stored, "%Y-%m-%d").ok()?;
    match tz.from_local_datetime(&day.and_hms_opt(0, 0, 0)?) {
        LocalResult::Single(at) | LocalResult::Ambiguous(at, _) => Some(at.with_timezone(&Utc)),
        // A day that starts in a gap (rare): its first moment is an hour later.
        LocalResult::None => {
            tz.from_local_datetime(&day.and_hms_opt(1, 0, 0)?).earliest().map(|at| at.with_timezone(&Utc))
        }
    }
}

/// Rewrite `values` in place so they're stored as records keeps them: `datetime` fields to their
/// stored form, lists tidied (see [`tidy`]; an empty list becomes `null`, unset). Values of other
/// fields, list patches, `null`s and unknown keys are left for [`patch_lists`] and the checks.
pub fn normalize(c: &Collection, values: &mut Map<String, Value>, now: &Now) -> Result<()> {
    for (name, value) in values.iter_mut() {
        let Some(field) = c.field(name) else { continue };
        match field.kind {
            FieldType::Datetime => {
                if let Value::String(s) = value {
                    let stored = datetime(s, &now.tz)
                        .map_err(|want| Error::invalid_params(format!("field '{name}' must be {want}, got \"{s}\"")))?;
                    *value = Value::String(stored);
                }
            }
            FieldType::List => {
                if let Value::Array(items) = value {
                    *value = tidy(std::mem::take(items));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// A list as stored: string items trimmed, blank ones dropped, repeats (ignoring case) dropped,
/// order kept. `null` when nothing is left: an empty list is unset (ADR 0024 §2).
pub fn tidy(items: Vec<Value>) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for item in items {
        let item = match item {
            Value::String(s) if s.trim().is_empty() => continue,
            Value::String(s) => Value::String(s.trim().to_owned()),
            other => other,
        };
        if !out.iter().any(|kept| same(kept, &item)) {
            out.push(item);
        }
    }
    if out.is_empty() {
        Value::Null
    } else {
        Value::Array(out)
    }
}

/// List items are the same if equal, strings ignoring case.
pub use shimmer_core::filter::same;

/// Resolve `{"add": [...], "remove": [...]}` on list fields against the record's current values
/// (ADR 0024 §2), so the changes hold whole lists. Removing an item that isn't there is fine;
/// anything other than `add` and `remove` in a patch is `invalid_params`.
pub fn patch_lists(c: &Collection, changes: &mut Map<String, Value>, current: &BTreeMap<String, Value>) -> Result<()> {
    for (name, value) in changes.iter_mut() {
        let Some(field) = c.field(name).filter(|f| f.kind == FieldType::List) else { continue };
        let Value::Object(patch) = value else { continue };
        if let Some(k) = patch.keys().find(|k| *k != "add" && *k != "remove") {
            return Err(Error::invalid_params(format!(
                "field '{}': a list change takes \"add\" and \"remove\", not \"{k}\"",
                field.name
            )));
        }
        let part = |key: &str| -> Result<Vec<Value>> {
            match patch.get(key) {
                None => Ok(Vec::new()),
                Some(Value::Array(items)) => Ok(items.clone()),
                Some(_) => Err(Error::invalid_params(format!("field '{}': \"{key}\" takes a list", field.name))),
            }
        };
        let (add, remove) = (part("add")?, part("remove")?);
        let mut items = current.get(name).and_then(Value::as_array).cloned().unwrap_or_default();
        items.retain(|i| !remove.iter().any(|r| same(i, r)));
        items.extend(add);
        *value = tidy(items);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

    const TORONTO: Tz = chrono_tz::America::Toronto;

    #[test]
    fn local_times_become_utc_and_dates_stay_dates() {
        // Toronto is UTC-4 in October.
        assert_eq!(datetime("2026-10-20 23:59", &TORONTO).unwrap(), "2026-10-21T03:59:00Z");
        assert_eq!(datetime("2026-10-20T23:59", &TORONTO).unwrap(), "2026-10-21T03:59:00Z");
        assert_eq!(datetime("2026-10-20T23:59:00-07:00", &TORONTO).unwrap(), "2026-10-21T06:59:00Z", "an offset wins");
        assert_eq!(datetime("2026-10-21T06:59:00Z", &TORONTO).unwrap(), "2026-10-21T06:59:00Z");
        assert_eq!(datetime("2026-10-20", &TORONTO).unwrap(), "2026-10-20", "no time given");
        assert!(datetime("next friday", &TORONTO).is_err());
    }

    #[test]
    fn daylight_saving_gaps_and_overlaps() {
        // 2026-03-08 02:30 doesn't happen in Toronto; 2026-11-01 01:30 happens twice.
        assert!(datetime("2026-03-08 02:30", &TORONTO).unwrap_err().contains("doesn't exist"));
        assert_eq!(datetime("2026-11-01 01:30", &TORONTO).unwrap(), "2026-11-01T05:30:00Z", "the earlier one");
    }

    #[test]
    fn a_plain_date_is_the_start_of_the_day() {
        let start = instant("2026-10-20", &TORONTO).unwrap();
        assert_eq!(start, Utc.with_ymd_and_hms(2026, 10, 20, 4, 0, 0).unwrap());
        assert!(instant("2026-10-20T03:00:00Z", &TORONTO).unwrap() < start, "11pm the night before");
        assert!(instant("garbage", &TORONTO).is_none());
    }

    #[test]
    fn lists_are_tidied_and_patched() {
        assert_eq!(tidy(vec![json!(" Go "), json!(""), json!("rust"), json!("go")]), json!(["Go", "rust"]));
        assert_eq!(tidy(vec![json!("  ")]), Value::Null, "empty is unset");

        let c = Collection::parse(
            "p",
            "[collection]\nid = \"p\"\nlabel = \"P\"\n[[field]]\nname = \"stack\"\ntype = \"list\"\nof = \"string\"",
        )
        .unwrap();
        let current = BTreeMap::from([("stack".to_owned(), json!(["Go", "Rust"]))]);
        let mut changes = json!({"stack": {"add": ["SQL", "go"], "remove": ["rust"]}}).as_object().unwrap().clone();
        patch_lists(&c, &mut changes, &current).unwrap();
        assert_eq!(changes["stack"], json!(["Go", "SQL"]));
        let mut changes = json!({"stack": {"remove": ["Go", "Rust"]}}).as_object().unwrap().clone();
        patch_lists(&c, &mut changes, &current).unwrap();
        assert_eq!(changes["stack"], Value::Null, "removing every item unsets it");
        let mut changes = json!({"stack": {"put": ["x"]}}).as_object().unwrap().clone();
        assert!(patch_lists(&c, &mut changes, &current).is_err());
    }

    #[test]
    fn today_is_local() {
        // 02:00 UTC is still the previous evening in Toronto.
        let now = Now { at: Utc.with_ymd_and_hms(2026, 10, 21, 2, 0, 0).unwrap(), tz: TORONTO };
        assert_eq!(now.today().to_string(), "2026-10-20");
        assert_eq!(now.stamp(), "2026-10-21T02:00:00Z");
    }
}
