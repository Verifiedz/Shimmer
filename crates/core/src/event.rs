//! The one event type: bus, IPC stream and JSONL log all carry exactly this.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ulid::Ulid;

use crate::ids::{ModuleId, TaskId};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Sortable, generated on emit.
    pub id: Ulid,
    pub at: DateTime<Utc>,
    pub source: ModuleId,
    /// `<module>.<entity>.<verb>`, verb in past tense.
    pub topic: String,
    pub payload: Value,
    /// The queued task this event was emitted from, if any (ADR 0035 §5.1): what lets automations
    /// follow a chain of rules and stop a loop. Omitted when none, so older lines read unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<TaskId>,
}

impl Event {
    /// The id's timestamp is `at`, so ids sort with the clock.
    pub fn new(source: ModuleId, topic: impl Into<String>, payload: Value, at: DateTime<Utc>) -> Self {
        Self { id: Ulid::from_datetime(at.into()), at, source, topic: topic.into(), payload, cause: None }
    }

    /// The same event, emitted from `cause`.
    pub fn caused_by(self, cause: Option<TaskId>) -> Self {
        Self { cause, ..self }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn cause_is_left_out_when_none_and_old_lines_still_read() {
        let ev = Event::new(ModuleId::new("records"), "records.item.created", json!({}), DateTime::UNIX_EPOCH);
        let line = serde_json::to_string(&ev).unwrap();
        assert!(!line.contains("cause"), "{line}");
        assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), ev);

        let task = TaskId::new();
        let caused = serde_json::to_value(ev.caused_by(Some(task))).unwrap();
        assert_eq!(caused["cause"], json!(task.to_string()));
    }
}
