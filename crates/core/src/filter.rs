//! Records' filter operators (ADR 0019, ADR 0024 §5) over plain JSON values (ADR 0035 §5.3).
//!
//! [`Op`] is one operator, its argument checked. Records decides which operators a field takes and
//! compares by the field's type (enum order, datetimes in the timezone), passing that comparison
//! in. [`Filter`] is a whole filter with no schema behind it, for automations' `when`: numbers
//! compare as numbers, text ignoring case, dates as dates, and enums as text (so use `in`).
//! Pure: today and the timezone come in, never from a clock (§12 rule 10).

use std::cmp::Ordering;

use chrono::{DateTime, NaiveDate};
use chrono_tz::Tz;
use serde_json::{Map, Value};

/// One operator and its argument.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Eq(Value),
    Ne(Value),
    Lt(Value),
    Lte(Value),
    Gt(Value),
    Gte(Value),
    In(Vec<Value>),
    Set(bool),
    /// Text containing this, ignoring case; kept lowercase.
    Contains(String),
    /// A list holding this item (text ignoring case).
    Has(Value),
}

impl Op {
    /// `name` with `arg`, its shape checked: `in` takes a non-empty list, `set` true or false,
    /// `contains` some text.
    pub fn parse(name: &str, arg: &Value) -> Result<Self, String> {
        let v = arg.clone();
        Ok(match name {
            "eq" => Op::Eq(v),
            "ne" => Op::Ne(v),
            "lt" => Op::Lt(v),
            "lte" => Op::Lte(v),
            "gt" => Op::Gt(v),
            "gte" => Op::Gte(v),
            "has" => Op::Has(v),
            "in" => match arg.as_array() {
                Some(list) if !list.is_empty() => Op::In(list.clone()),
                _ => return Err("'in' takes a non-empty list".into()),
            },
            "set" => Op::Set(arg.as_bool().ok_or("'set' takes true or false")?),
            "contains" => match arg.as_str() {
                Some(s) if !s.is_empty() => Op::Contains(s.to_lowercase()),
                _ => return Err("'contains' takes some text".into()),
            },
            other => return Err(format!("unknown operator '{other}'")),
        })
    }

    /// The same operator with each value it compares against passed through `f` (records turns
    /// what someone typed into what's stored). `set` and `contains` are left as they are.
    pub fn try_map<E>(self, mut f: impl FnMut(&Value) -> Result<Value, E>) -> Result<Self, E> {
        Ok(match self {
            Op::Eq(v) => Op::Eq(f(&v)?),
            Op::Ne(v) => Op::Ne(f(&v)?),
            Op::Lt(v) => Op::Lt(f(&v)?),
            Op::Lte(v) => Op::Lte(f(&v)?),
            Op::Gt(v) => Op::Gt(f(&v)?),
            Op::Gte(v) => Op::Gte(f(&v)?),
            Op::Has(v) => Op::Has(f(&v)?),
            Op::In(list) => Op::In(list.iter().map(f).collect::<Result<_, _>>()?),
            other @ (Op::Set(_) | Op::Contains(_)) => other,
        })
    }

    /// Does `got` pass? `cmp(want)` orders `got` against `want`, `None` when they don't compare
    /// (unset, or values of different kinds): then no comparison passes.
    pub fn holds(&self, got: &Value, cmp: impl Fn(&Value) -> Option<Ordering>) -> bool {
        match self {
            Op::Eq(want) => got == want,
            // Unset passes: "stage is not rejected" includes records with no stage yet.
            Op::Ne(want) => got != want,
            Op::Lt(want) => cmp(want) == Some(Ordering::Less),
            Op::Lte(want) => matches!(cmp(want), Some(Ordering::Less | Ordering::Equal)),
            Op::Gt(want) => cmp(want) == Some(Ordering::Greater),
            Op::Gte(want) => matches!(cmp(want), Some(Ordering::Greater | Ordering::Equal)),
            Op::In(list) => is_set(got) && list.contains(got),
            Op::Set(want) => is_set(got) == *want,
            Op::Contains(needle) => got.as_str().is_some_and(|s| s.to_lowercase().contains(needle)),
            Op::Has(want) => got.as_array().is_some_and(|items| items.iter().any(|i| same(i, want))),
        }
    }
}

/// Set means a value other than `null`, `""` or `[]` (ADR 0017, ADR 0024 §2).
pub fn is_set(v: &Value) -> bool {
    !(v.is_null() || v.as_str() == Some("") || v.as_array().is_some_and(Vec::is_empty))
}

/// Equal, text ignoring case.
pub fn same(a: &Value, b: &Value) -> bool {
    match (a.as_str(), b.as_str()) {
        (Some(a), Some(b)) => a.to_lowercase() == b.to_lowercase(),
        _ => a == b,
    }
}

/// `today`, `today+N` or `today-N` (days) as a date; `None` for anything else (ADR 0024 §5).
pub fn relative_date(s: &str, today: NaiveDate) -> Result<Option<NaiveDate>, String> {
    let Some(rest) = s.trim().strip_prefix("today") else { return Ok(None) };
    let rest = rest.trim();
    if rest.is_empty() {
        return Ok(Some(today));
    }
    let (sign, n) = match (rest.strip_prefix('+'), rest.strip_prefix('-')) {
        (Some(n), _) => (1, n),
        (_, Some(n)) => (-1, n),
        _ => return Err(format!("'{s}': write today, today+N or today-N (days)")),
    };
    let days: i64 = n.trim().parse().map_err(|_| format!("'{s}': write today, today+N or today-N (days)"))?;
    today
        .checked_add_signed(chrono::Duration::days(sign * days))
        .map(Some)
        .ok_or_else(|| format!("'{s}' is too far away"))
}

/// A plain `YYYY-MM-DD` date.
pub fn plain_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// A filter over plain JSON: every key's conditions must pass, and `"or": [{…}, …]` passes when
/// one of its filters does (at the top only, as in records). A key is a payload field or a dotted
/// path into one (`item.stage`). A plain value is equality, or `has` against a list; `null` is
/// "unset".
#[derive(Clone, Debug, PartialEq)]
pub struct Filter {
    conditions: Vec<Cond>,
}

#[derive(Clone, Debug, PartialEq)]
enum Cond {
    One(String, Op),
    /// A plain value: equality, or `has` when the value found is a list.
    Plain(String, Value),
    Any(Vec<Vec<Cond>>),
}

impl Filter {
    /// Every problem names its key.
    pub fn parse(filter: &Map<String, Value>) -> Result<Self, String> {
        Ok(Self { conditions: conditions(filter, true)? })
    }

    /// Does `data` pass? `today` and `tz` resolve `today+N` and read datetimes as local days.
    pub fn matches(&self, data: &Value, today: NaiveDate, tz: Tz) -> bool {
        meets(&self.conditions, data, today, tz)
    }
}

fn bad(key: &str, msg: &str) -> String {
    format!("filter on '{key}': {msg}")
}

fn conditions(filter: &Map<String, Value>, top: bool) -> Result<Vec<Cond>, String> {
    let mut out = Vec::new();
    for (key, value) in filter {
        if key == "or" {
            if !top {
                return Err("filter: \"or\" can't be nested inside \"or\"".into());
            }
            let alternatives = match value.as_array() {
                Some(list) if !list.is_empty() => list,
                _ => return Err("filter: \"or\" takes a non-empty list of filters".into()),
            };
            let mut any = Vec::new();
            for alternative in alternatives {
                let Some(inner) = alternative.as_object().filter(|o| !o.is_empty()) else {
                    return Err("filter: each \"or\" alternative is a non-empty filter object".into());
                };
                any.push(conditions(inner, false)?);
            }
            out.push(Cond::Any(any));
            continue;
        }
        if key.split('.').any(str::is_empty) {
            return Err(format!("filter: '{key}' is not a field or a dotted path"));
        }
        match value {
            Value::Object(ops) => {
                if ops.is_empty() {
                    return Err(bad(key, "an operator object needs at least one operator"));
                }
                for (name, arg) in ops {
                    let op = Op::parse(name, arg).map_err(|m| bad(key, &m))?;
                    // Checked now so a broken `today+x` is refused when the filter is written. Against
                    // an ordinary date, not `NaiveDate::MIN`, which any `today-N` would run past
                    // (#251's review).
                    op.clone().try_map(|v| resolve(v, check_day())).map_err(|m| bad(key, &m))?;
                    out.push(Cond::One(key.clone(), op));
                }
            }
            Value::Null => out.push(Cond::One(key.clone(), Op::Set(false))),
            plain => out.push(Cond::Plain(key.clone(), plain.clone())),
        }
    }
    Ok(out)
}

fn meets(conditions: &[Cond], data: &Value, today: NaiveDate, tz: Tz) -> bool {
    conditions.iter().all(|cond| match cond {
        Cond::One(key, op) => {
            let got = crate::text::lookup(data, key).unwrap_or(&Value::Null);
            let Ok(op) = op.clone().try_map(|v| resolve(v, today)) else { return false };
            op.holds(got, |want| plain_cmp(got, want, tz))
        }
        Cond::Plain(key, want) => {
            let got = crate::text::lookup(data, key).unwrap_or(&Value::Null);
            match (got, resolve(want, today)) {
                (Value::Array(items), Ok(want)) if !want.is_array() => items.iter().any(|i| same(i, &want)),
                (got, Ok(want)) => got == &want,
                (_, Err(_)) => false,
            }
        }
        Cond::Any(alternatives) => alternatives.iter().any(|all| meets(all, data, today, tz)),
    })
}

/// `today+N` as a date; any other value as it is.
/// The "today" a filter's relative dates are checked against when it's written: any ordinary
/// date works, since only the form is being checked.
fn check_day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2000, 1, 1).expect("a valid date")
}

fn resolve(v: &Value, today: NaiveDate) -> Result<Value, String> {
    match v.as_str().map(|s| relative_date(s, today)).transpose()?.flatten() {
        Some(day) => Ok(Value::String(day.format("%Y-%m-%d").to_string())),
        None => Ok(v.clone()),
    }
}

/// Plain ordering: numbers as numbers; a plain date against a datetime by its day in `tz` (so
/// "<= 2026-10-20" includes 23:59 that day); datetimes as moments; other text ignoring case.
fn plain_cmp(got: &Value, want: &Value, tz: Tz) -> Option<Ordering> {
    if !is_set(got) {
        return None;
    }
    match (got, want) {
        (Value::Number(a), Value::Number(b)) => a.as_f64()?.partial_cmp(&b.as_f64()?),
        (Value::String(a), Value::String(b)) => {
            let moment = |s: &str| DateTime::parse_from_rfc3339(s).ok();
            let day = |s: &str| plain_date(s).or_else(|| Some(moment(s)?.with_timezone(&tz).date_naive()));
            if let (Some(a), Some(b)) = (moment(a), moment(b)) {
                return Some(a.cmp(&b));
            }
            if plain_date(a).is_some() || plain_date(b).is_some() {
                if let (Some(a), Some(b)) = (day(a), day(b)) {
                    return Some(a.cmp(&b));
                }
            }
            Some(a.to_lowercase().cmp(&b.to_lowercase()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn filter(f: Value) -> Result<Filter, String> {
        Filter::parse(f.as_object().unwrap())
    }

    fn passes(f: Value, data: Value) -> bool {
        let today = NaiveDate::from_ymd_opt(2026, 10, 16).unwrap();
        filter(f).unwrap().matches(&data, today, chrono_tz::Asia::Karachi)
    }

    #[test]
    fn plain_values_are_equality_and_has_on_lists() {
        let ev = json!({"collection": "jobs", "item": {"stage": "applied", "tags": ["Remote"], "pay": 90}});
        assert!(passes(json!({"collection": "jobs"}), ev.clone()));
        assert!(!passes(json!({"collection": "leetcode"}), ev.clone()));
        assert!(passes(json!({"item.tags": "remote"}), ev.clone()), "a plain value on a list is has");
        assert!(passes(json!({"item.notes": null}), ev.clone()), "null is unset");
        assert!(!passes(json!({"item.stage": null}), ev));
    }

    #[test]
    fn operators_over_plain_values() {
        let ev =
            json!({"pay": 90, "stage": "Applied", "tags": ["a"], "due": "2026-10-18", "at": "2026-10-20T20:00:00Z"});
        assert!(passes(json!({"pay": {"gte": 90, "lt": 100.5}}), ev.clone()), "numbers as numbers");
        assert!(passes(json!({"stage": {"in": ["Applied", "offer"]}}), ev.clone()));
        assert!(passes(json!({"stage": {"contains": "PPL"}}), ev.clone()));
        assert!(passes(json!({"tags": {"has": "A"}, "nope": {"set": false}}), ev.clone()));
        assert!(passes(json!({"stage": {"ne": "rejected"}}), ev.clone()));
        assert!(passes(json!({"due": {"lte": "today+3", "gt": "today"}}), ev.clone()), "relative dates");
        assert!(!passes(json!({"due": {"lt": "today+2"}}), ev.clone()));
        // 20:00 UTC on the 20th is the 21st in Karachi (UTC+5).
        assert!(passes(json!({"at": {"gt": "2026-10-20"}}), ev.clone()), "a datetime's day is local");
        assert!(!passes(json!({"pay": {"gt": "80"}}), ev), "different kinds don't compare");
    }

    /// `today-N` used to be refused when the filter was written: it was checked against
    /// `NaiveDate::MIN`, which any subtraction runs past (#251's review).
    #[test]
    fn today_minus_n_is_accepted_and_looks_back() {
        assert!(filter(json!({"applied_on": {"lte": "today-7"}})).is_ok());
        assert!(filter(json!({"applied_on": "today-1"})).is_ok());
        let ev = json!({"applied_on": "2026-10-05"});
        assert!(passes(json!({"applied_on": {"lte": "today-7"}}), ev.clone()), "11 days ago is at least a week ago");
        assert!(!passes(json!({"applied_on": {"gte": "today-7"}}), ev));
    }

    #[test]
    fn or_takes_any_alternative() {
        let f = json!({"collection": "jobs", "or": [{"stage": "offer"}, {"pay": {"gt": 100}}]});
        assert!(passes(f.clone(), json!({"collection": "jobs", "stage": "offer"})));
        assert!(passes(f.clone(), json!({"collection": "jobs", "pay": 120})));
        assert!(!passes(f.clone(), json!({"collection": "jobs", "stage": "applied", "pay": 50})));
        assert!(!passes(f, json!({"collection": "docs", "stage": "offer"})));
    }

    #[test]
    fn mistakes_name_the_key() {
        for (f, want) in [
            (json!({"pay": {"bigger": 3}}), "filter on 'pay': unknown operator 'bigger'"),
            (json!({"pay": {}}), "needs at least one operator"),
            (json!({"stage": {"in": []}}), "'in' takes a non-empty list"),
            (json!({"x": {"set": "yes"}}), "'set' takes true or false"),
            (json!({"x": {"contains": ""}}), "'contains' takes some text"),
            (json!({"due": {"lt": "today+x"}}), "write today, today+N or today-N"),
            (json!({"or": [{"or": [{"a": 1}]}]}), "can't be nested"),
            (json!({"or": []}), "non-empty list of filters"),
            (json!({"item..x": 1}), "not a field or a dotted path"),
        ] {
            let err = filter(f.clone()).unwrap_err();
            assert!(err.contains(want), "{f}: {err}");
        }
    }
}
