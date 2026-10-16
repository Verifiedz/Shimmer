//! What the daemon can run and what can happen in it, read-only (ADR 0035 §5.7): every module op
//! (its `CommandSpec`) and every topic, with its one-line description. Fixed at daemon start, as
//! the registry is. Automations checks a rule's ops and lists what a rule can react to with it;
//! it gives a module no power, only the facts `core.manifest` already tells any client.

use std::collections::BTreeMap;

use crate::manifest::CommandSpec;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Catalog {
    ops: BTreeMap<String, CommandSpec>,
    /// Topic → description (empty when it has none).
    topics: BTreeMap<String, String>,
}

impl Catalog {
    pub fn new(
        ops: impl IntoIterator<Item = CommandSpec>,
        topics: impl IntoIterator<Item = (String, Option<String>)>,
    ) -> Self {
        Self {
            ops: ops.into_iter().map(|c| (c.op.clone(), c)).collect(),
            topics: topics.into_iter().map(|(t, d)| (t, d.unwrap_or_default())).collect(),
        }
    }

    /// The op a module answers, if there is one: what `ctx.queue` can run.
    pub fn op(&self, op: &str) -> Option<&CommandSpec> {
        self.ops.get(op)
    }

    /// Every op, by name.
    pub fn ops(&self) -> impl Iterator<Item = &CommandSpec> {
        self.ops.values()
    }

    /// Every topic, by name, with its description (`""` when it has none).
    pub fn topics(&self) -> impl Iterator<Item = (&str, &str)> {
        self.topics.iter().map(|(t, d)| (t.as_str(), d.as_str()))
    }

    pub fn has_topic(&self, topic: &str) -> bool {
        self.topics.contains_key(topic)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::manifest::Execution;

    #[test]
    fn ops_and_topics_by_name() {
        let spec = CommandSpec {
            op: "records.add".into(),
            summary: "add".into(),
            params_schema: json!({}),
            execution: Execution::Inline,
        };
        let c = Catalog::new(
            [spec],
            [("records.item.created".into(), Some("a record was added".into())), ("x.y.z".into(), None)],
        );
        assert!(c.op("records.add").is_some() && c.op("records.nope").is_none());
        assert_eq!(c.topics().collect::<Vec<_>>(), [("records.item.created", "a record was added"), ("x.y.z", "")]);
        assert!(c.has_topic("x.y.z"));
    }
}
