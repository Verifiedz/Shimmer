//! Emit-only handle onto the event bus. Modules cannot subscribe through `Ctx`; reactions
//! arrive via `Module::on_event`.

use std::sync::Arc;

use serde_json::Value;

use crate::clock::Clock;
use crate::error::{Error, Result};
use crate::event::Event;
use crate::ids::{ModuleId, TaskId};

/// The daemon's side: appends to the event log, then publishes.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: Event) -> Result<()>;
}

/// A topic is `<source>.<entity>.<verb>`: at least three non-empty segments, and a module
/// may only emit under its own id, so a topic always says who it came from.
pub fn validate_topic(source: &ModuleId, topic: &str) -> Result<()> {
    let segments: Vec<&str> = topic.split('.').collect();
    let ok = segments.len() >= 3
        && segments.iter().all(|s| !s.is_empty() && !s.contains(['*', ' ']))
        && segments[0] == source.as_str();
    if ok {
        Ok(())
    } else {
        Err(Error::invalid_params(format!("topic '{topic}' must look like '{source}.<entity>.<verb>'")))
    }
}

#[derive(Clone)]
pub struct Emitter {
    source: ModuleId,
    sink: Arc<dyn EventSink>,
    clock: Clock,
    /// Stamped on every event: the task this handle runs in (ADR 0035 §5.1).
    cause: Option<TaskId>,
}

impl Emitter {
    pub fn new(source: ModuleId, sink: Arc<dyn EventSink>, clock: Clock) -> Self {
        Self { source, sink, clock, cause: None }
    }

    /// A copy whose events say they came from `task`.
    pub fn for_task(&self, task: TaskId) -> Self {
        Self { cause: Some(task), ..self.clone() }
    }

    pub fn emit(&self, topic: &str, payload: Value) -> Result<()> {
        validate_topic(&self.source, topic)?;
        self.sink.emit(Event::new(self.source.clone(), topic, payload, self.clock.now()).caused_by(self.cause))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_rules() {
        let m = ModuleId::new("records");
        assert!(validate_topic(&m, "records.item.created").is_ok());
        assert!(validate_topic(&m, "records.item").is_err());
        assert!(validate_topic(&m, "fetchers.item.found").is_err());
        assert!(validate_topic(&m, "records..created").is_err());
        assert!(validate_topic(&m, "records.item.*").is_err());
    }
}
