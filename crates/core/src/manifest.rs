use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{LaneId, ModuleId};

/// Identity, version, owned data namespace and declared capabilities.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub id: ModuleId,
    pub version: String,
    /// Directory name under `$SHIMMER_HOME/data/`. The module sees nothing outside it.
    pub namespace: String,
    /// Topics this module emits. Each must start with `<id>.`.
    #[serde(default)]
    pub topics: Vec<String>,
    /// One line on each topic, for people choosing what a rule reacts to (`shimmer automations
    /// events`, ADR 0035 §5.5): "a record was marked done". Every key is one of `topics`; a topic
    /// left out has no description.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub topic_descriptions: BTreeMap<String, String>,
    /// Declared, not yet enforced. Free-form until an ADR fixes the vocabulary.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandSpec {
    /// `<module>.<verb>`.
    pub op: String,
    pub summary: String,
    /// JSON Schema. Advisory before M3.
    pub params_schema: Value,
    pub execution: Execution,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Execution {
    /// Answered synchronously. Fast, non-blocking, no I/O beyond the store.
    Inline,
    /// Enqueued. Returns a task id immediately. May take minutes.
    Queued { lane: LaneId },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneConfig {
    pub id: LaneId,
    /// 1 = strict FIFO.
    pub max_concurrent: usize,
}

impl LaneConfig {
    pub fn new(id: impl Into<LaneId>, max_concurrent: usize) -> Self {
        Self { id: id.into(), max_concurrent }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_matches_wire_shape_in_protocol_md() {
        let inline = serde_json::to_value(Execution::Inline).unwrap();
        assert_eq!(inline, serde_json::json!("inline"));
        let queued = serde_json::to_value(Execution::Queued { lane: "index".into() }).unwrap();
        assert_eq!(queued, serde_json::json!({"queued": {"lane": "index"}}));
    }
}
