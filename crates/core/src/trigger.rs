use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{LaneId, TriggerId};
use crate::task::Fallback;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerSpec {
    pub id: TriggerId,
    pub schedule: Schedule,
    /// Required. There is deliberately no default (§6.3).
    pub catch_up: CatchUp,
    /// Enqueued when the trigger fires.
    pub op: String,
    pub params: Value,
    pub lane: Option<LaneId>,
    pub fallback: Option<Fallback>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Schedule {
    /// Five-field cron, in UTC.
    Cron(String),
    /// Five-field cron in the configured `local_timezone` (ADR 0009), so 9am stays 9am across a
    /// clock change (ADR 0031 §2). `{"cron_local": "0 9 * * 1-5"}` on the wire.
    CronLocal(String),
    /// Whole seconds on the wire.
    Every(#[serde(with = "duration_secs")] Duration),
    Once(DateTime<Utc>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatchUp {
    /// Missed firings are dropped. Polls, health checks.
    Skip,
    /// Fire once on wake regardless of how many were missed.
    RunOnce,
    /// Fire once per missed period, with the correct historical timestamp.
    Backfill,
}

mod duration_secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_wire_shapes() {
        let v = serde_json::to_value(Schedule::Every(Duration::from_secs(172_800))).unwrap();
        assert_eq!(v, serde_json::json!({"every": 172800}));
        let back: Schedule = serde_json::from_value(v).unwrap();
        assert_eq!(back, Schedule::Every(Duration::from_secs(172_800)));
    }

    #[test]
    fn catch_up_has_no_default() {
        // Missing `catch_up` must fail to deserialize: guessing sends duplicate emails.
        let json = serde_json::json!({
            "id": "t", "schedule": {"cron": "0 9 * * 1"}, "op": "x.y", "params": null,
            "lane": null, "fallback": null
        });
        assert!(serde_json::from_value::<TriggerSpec>(json).is_err());
    }
}
