//! One message in the inbox (ADR 0032 §3), and what `notify.send` builds it from (§4). Pure.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use shimmer_core::{Error, Result};

/// Every message is one file here, named by its id: a ULID, so names sort oldest first.
pub const INBOX: &str = "inbox";
/// An empty file per held message, so `notify.flush` finds them without reading the inbox.
pub const HELD: &str = "held";
/// How many unread messages the cap has removed: `unread = N`.
pub const REMOVED: &str = "removed.toml";

pub fn path(id: &str) -> String {
    format!("{INBOX}/{id}.toml")
}

pub fn held_path(id: &str) -> String {
    format!("{HELD}/{id}")
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
}

impl Priority {
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "low" => Some(Self::Low),
            "normal" => Some(Self::Normal),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// In the inbox, not yet handed to any sink.
    Pending,
    /// Kept back for quiet hours; sent in the summary when they end.
    Held,
    Delivered,
    /// No sink could take it. Still in the inbox: nothing is ever dropped (§11.3).
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Delivery {
    pub medium: String,
    pub at: DateTime<Utc>,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub id: String,
    pub at: DateTime<Utc>,
    pub priority: Priority,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// `send`, `schedule`, `fallback` or `automation:<rule>` (ADR 0032 §3, ADR 0035 §10).
    pub source: String,
    pub state: State,
    #[serde(default)]
    pub read: bool,
    #[serde(default, rename = "delivery", skip_serializing_if = "Vec::is_empty")]
    pub deliveries: Vec<Delivery>,
}

impl Message {
    pub fn encode(&self) -> String {
        toml::to_string(self).expect("a message always encodes")
    }

    pub fn decode(text: &str, path: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| Error::internal(format!("{path}: {}", e.message())))
    }
}

/// What `notify.send` was asked to say, before it has an id and a time.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub title: String,
    pub body: String,
    pub priority: Priority,
    pub source: String,
}

/// `notify.send`'s params (§4): `{title, body?, priority?, from?}`. `from` is a label the caller
/// sets for `--from`: `schedule` or `automation:<rule>`. A fallback (ADR 0005) carries
/// `failure_code`, `failure_reason` and `failure_detail` instead of a title, and an older one a
/// `message` for the body. Any other key is ignored: a fallback that fails to send would be the
/// silence it exists to prevent, so a fallback's message is always sent, at worst as `high`.
pub fn draft(params: &Value) -> Result<Draft> {
    let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let fallback = params.get("failure_code").is_some() || params.get("failure_reason").is_some();
    let title = match (text("title"), fallback) {
        (Some(title), _) => title.to_owned(),
        (None, true) => {
            let why = text("failure_reason").or(text("failure_code")).unwrap_or("no reason given");
            format!("Scheduled task failed: {why}")
        }
        (None, false) => return Err(Error::invalid_params("a message needs a title")),
    };
    let body = text("body").or(text("message")).unwrap_or_default().to_owned();
    let priority = match params.get("priority") {
        None | Some(Value::Null) => Priority::Normal,
        Some(v) => match v.as_str().and_then(Priority::parse) {
            Some(p) => p,
            None if fallback => Priority::High,
            None => return Err(Error::invalid_params(format!("priority is low, normal or high, not {v}"))),
        },
    };
    let source = match (fallback, text("from")) {
        (true, _) => "fallback".to_owned(),
        (false, None) => "send".to_owned(),
        (false, Some("schedule")) => "schedule".to_owned(),
        (false, Some(from)) => match from.strip_prefix("automation:") {
            Some(rule) if !rule.is_empty() && rule.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) => {
                from.to_owned()
            }
            _ => return Err(Error::invalid_params(format!("from is 'schedule' or 'automation:<rule>', not '{from}'"))),
        },
    };
    Ok(Draft { title, body, priority, source })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_message_round_trips_as_toml() {
        let m = Message {
            id: "01JD2T0000000000000000000A".into(),
            at: "2026-10-14T13:00:00Z".parse().unwrap(),
            priority: Priority::High,
            title: "site couldn't launch".into(),
            body: "Step 2/5 failed".into(),
            source: "automation:workspace-dirty".into(),
            state: State::Delivered,
            read: false,
            deliveries: vec![Delivery {
                medium: "desktop".into(),
                at: "2026-10-14T13:00:01Z".parse().unwrap(),
                ok: true,
                error: None,
            }],
        };
        let text = m.encode();
        assert!(text.contains("priority = \"high\"") && text.contains("[[delivery]]"), "{text}");
        assert_eq!(Message::decode(&text, "x").unwrap(), m);
    }

    #[test]
    fn a_send_needs_a_title_and_a_known_priority() {
        let d = draft(&json!({"title": " Standup ", "body": "in 15", "priority": "low"})).unwrap();
        assert_eq!(
            d,
            Draft { title: "Standup".into(), body: "in 15".into(), priority: Priority::Low, source: "send".into() }
        );
        assert_eq!(draft(&json!({"title": "x"})).unwrap().priority, Priority::Normal);
        assert!(draft(&json!({"body": "x"})).unwrap_err().message.contains("needs a title"));
        assert!(draft(&json!({"title": "x", "priority": "urgent"}))
            .unwrap_err()
            .message
            .contains("low, normal or high"));
        assert_eq!(draft(&json!({"title": "x", "colour": "red"})).unwrap().title, "x", "unknown keys ignored");
        assert_eq!(draft(&json!({"title": "x", "from": "schedule"})).unwrap().source, "schedule");
        assert_eq!(
            draft(&json!({"title": "x", "from": "automation:leet-solved"})).unwrap().source,
            "automation:leet-solved"
        );
        assert!(draft(&json!({"title": "x", "from": "alert:fake"})).is_err(), "only schedule or automation:<rule>");
    }

    #[test]
    fn a_fallback_always_makes_a_message() {
        // ADR 0005's shape: the queue stamps `priority` and the failure; `message` is the old body key.
        let d = draft(&json!({"message": "your 2pm session", "priority": "high", "failure_code": "workspace_dirty",
                              "failure_reason": "site is dirty", "failure_detail": null}))
        .unwrap();
        assert_eq!(d.title, "Scheduled task failed: site is dirty");
        assert_eq!((d.body.as_str(), d.priority, d.source.as_str()), ("your 2pm session", Priority::High, "fallback"));
        let odd = draft(&json!({"failure_code": "unavailable", "priority": "loud"})).unwrap();
        assert_eq!((odd.title.as_str(), odd.priority), ("Scheduled task failed: unavailable", Priority::High));
    }
}
