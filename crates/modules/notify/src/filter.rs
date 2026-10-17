//! The filters every op that picks messages shares (ADR 0032 §6): `list`, `count`, `mark_read`,
//! `mark_unread`, `remove` and `resend`. Pure: parsed once from params, then matched per message.

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde_json::Value;
use shimmer_core::{Error, Result};

use crate::message::{Message, Priority, State};

/// Where a message came from, as `--from` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum From {
    Send,
    Schedules,
    Fallbacks,
    Alerts,
    Automations,
}

impl From {
    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "send" => Self::Send,
            "schedules" => Self::Schedules,
            "fallbacks" => Self::Fallbacks,
            "alerts" => Self::Alerts,
            "automations" => Self::Automations,
            _ => return None,
        })
    }

    fn matches(self, source: &str) -> bool {
        match self {
            Self::Send => source == "send",
            Self::Schedules => source == "schedule",
            Self::Fallbacks => source == "fallback",
            Self::Alerts => source.starts_with("alert:"),
            Self::Automations => source.starts_with("automation:"),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filter {
    unread: bool,
    read: bool,
    state: Option<State>,
    from: Option<From>,
    priority: Option<Priority>,
    since: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
    /// Lowercased.
    text: Option<String>,
}

const KEYS: [&str; 8] = ["unread", "read", "state", "from", "priority", "since", "before", "text"];

impl Filter {
    /// The filter keys in `params` (other keys are the op's own). `tz` turns a date or a local
    /// time into an instant (ADR 0009).
    pub fn parse(params: &Value, tz: &chrono_tz::Tz) -> Result<Self> {
        let bad = |why: String| Error::invalid_params(why);
        let text = |key: &str| -> Result<Option<String>> {
            match params.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_owned())),
                Some(v) => Err(bad(format!("{key} should be text, not {v}"))),
            }
        };
        let flag = |key: &str| params.get(key) == Some(&Value::Bool(true));
        let f = Filter {
            unread: flag("unread"),
            read: flag("read"),
            state: text("state")?
                .map(|s| {
                    serde_json::from_value::<State>(Value::String(s.clone()))
                        .map_err(|_| bad(format!("state is pending, held, delivered or failed, not '{s}'")))
                })
                .transpose()?,
            from: text("from")?
                .map(|s| {
                    From::parse(&s).ok_or_else(|| {
                        bad(format!("from is send, schedules, fallbacks, alerts or automations, not '{s}'"))
                    })
                })
                .transpose()?,
            priority: text("priority")?
                .map(|s| Priority::parse(&s).ok_or_else(|| bad(format!("priority is low, normal or high, not '{s}'"))))
                .transpose()?,
            since: text("since")?.map(|s| instant(&s, tz)).transpose()?,
            before: text("before")?.map(|s| instant(&s, tz)).transpose()?,
            text: text("text")?.map(|s| s.to_lowercase()),
        };
        if f.unread && f.read {
            return Err(bad("unread and read together match nothing; pick one".into()));
        }
        Ok(f)
    }

    /// Whether `params` names any filter at all.
    pub fn named_in(params: &Value) -> bool {
        KEYS.iter().any(|k| params.get(k).is_some_and(|v| !v.is_null() && *v != Value::Bool(false)))
    }

    pub fn matches(&self, m: &Message) -> bool {
        !(self.unread && m.read)
            && !(self.read && !m.read)
            && self.state.is_none_or(|s| s == m.state)
            && self.from.is_none_or(|f| f.matches(&m.source))
            && self.priority.is_none_or(|p| p == m.priority)
            && self.since.is_none_or(|t| m.at >= t)
            && self.before.is_none_or(|t| m.at < t)
            && self.text.as_ref().is_none_or(|t| {
                m.title.to_lowercase().contains(t.as_str()) || m.body.to_lowercase().contains(t.as_str())
            })
    }
}

/// `2026-10-01` (the start of that local day), `2026-10-01 14:00` (local), or a full RFC 3339 time.
fn instant(text: &str, tz: &chrono_tz::Tz) -> Result<DateTime<Utc>> {
    if let Ok(t) = DateTime::parse_from_rfc3339(text) {
        return Ok(t.with_timezone(&Utc));
    }
    let local = NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map(|d| d.and_hms_opt(0, 0, 0).expect("midnight exists"))
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M"))
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M"))
        .map_err(|_| Error::invalid_params(format!("'{text}' isn't a date: use 2026-10-01 or 2026-10-01 14:00")))?;
    tz.from_local_datetime(&local)
        .earliest()
        .map(|t| t.with_timezone(&Utc))
        .ok_or_else(|| Error::invalid_params(format!("'{text}' doesn't exist in your timezone (a clock change)")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn msg(title: &str, body: &str, source: &str, read: bool, at: &str) -> Message {
        Message {
            id: "01JD2T0000000000000000000A".into(),
            at: at.parse().unwrap(),
            priority: Priority::Normal,
            title: title.into(),
            body: body.into(),
            source: source.into(),
            state: State::Delivered,
            read,
            deliveries: Vec::new(),
        }
    }

    fn f(params: Value) -> Filter {
        Filter::parse(&params, &chrono_tz::UTC).unwrap()
    }

    #[test]
    fn each_filter_matches_its_own() {
        let alert = msg("site couldn't launch", "Step 2/5", "alert:workspace_dirty", false, "2026-10-10T09:00:00Z");
        let sched = msg("Standup", "in 15", "schedule", true, "2026-10-12T09:00:00Z");
        assert!(f(json!({"unread": true})).matches(&alert) && !f(json!({"unread": true})).matches(&sched));
        assert!(f(json!({"read": true})).matches(&sched));
        assert!(f(json!({"from": "alerts"})).matches(&alert) && !f(json!({"from": "alerts"})).matches(&sched));
        assert!(f(json!({"from": "schedules"})).matches(&sched));
        assert!(
            f(json!({"since": "2026-10-11"})).matches(&sched) && !f(json!({"since": "2026-10-11"})).matches(&alert)
        );
        assert!(f(json!({"before": "2026-10-11"})).matches(&alert));
        assert!(f(json!({"text": "STEP 2"})).matches(&alert), "text ignores case, body too");
        assert!(f(json!({"state": "delivered", "priority": "normal"})).matches(&alert));
        assert!(!f(json!({"priority": "high"})).matches(&alert));
        assert!(f(json!({})).matches(&alert));
    }

    #[test]
    fn dates_are_local() {
        let ny: chrono_tz::Tz = "America/New_York".parse().unwrap();
        let since = Filter::parse(&json!({"since": "2026-10-11"}), &ny).unwrap();
        assert!(!since.matches(&msg("x", "", "send", false, "2026-10-11T03:00:00Z")), "still the 10th in New York");
        assert!(since.matches(&msg("x", "", "send", false, "2026-10-11T05:00:00Z")));
    }

    #[test]
    fn mistakes_are_named() {
        let err = |p: Value| Filter::parse(&p, &chrono_tz::UTC).unwrap_err().message;
        assert!(err(json!({"unread": true, "read": true})).contains("pick one"));
        assert!(err(json!({"from": "friends"})).contains("schedules, fallbacks"));
        assert!(err(json!({"state": "lost"})).contains("pending, held"));
        assert!(err(json!({"since": "last week"})).contains("isn't a date"));
        assert!(err(json!({"text": 3})).contains("should be text"));
    }

    #[test]
    fn whether_any_filter_is_named() {
        assert!(!Filter::named_in(&json!({"ids": ["x"], "confirm": true, "unread": false})));
        assert!(Filter::named_in(&json!({"read": true})));
        assert!(Filter::named_in(&json!({"text": "acme"})));
    }
}
