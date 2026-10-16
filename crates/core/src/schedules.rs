//! One-time schedules a module adds while running (ADR 0035 §5.6): "run this op at that time",
//! kept by the scheduler across restarts. What automations' delayed actions (`after = "3d"`) and a
//! future reminders feature use. `Module::triggers()` stays the way to declare fixed schedules.
//!
//! The same power as `ctx.queue`, later: every Shimmer module gets it. A module can only touch
//! the schedules it added; ids carry its name.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::ids::{ModuleId, TriggerId};

/// One run of `op` with `params`, due `at`. `label` is what `scheduler list` shows for it, e.g.
/// "automations: follow-up for acme (rule nudge-after-applying)".
#[derive(Clone, Debug, PartialEq)]
pub struct OneTime {
    pub op: String,
    pub params: Value,
    pub at: DateTime<Utc>,
    pub label: String,
}

/// The daemon's side: the scheduler.
pub trait ScheduleBackend: Send + Sync {
    /// Checks the op exists, persists the schedule, and returns its id.
    fn add_once(&self, module: &ModuleId, once: OneTime) -> Result<TriggerId>;
    /// `false` when there's no such schedule of `module`'s, e.g. it already ran.
    fn remove(&self, module: &ModuleId, id: &TriggerId) -> Result<bool>;
}

#[derive(Clone)]
pub struct Schedules {
    module: ModuleId,
    backend: Arc<dyn ScheduleBackend>,
}

impl Schedules {
    pub fn new(module: ModuleId, backend: Arc<dyn ScheduleBackend>) -> Self {
        Self { module, backend }
    }

    /// Fails `unavailable`: until the daemon wires the scheduler in (during `Module::init` too).
    pub fn unavailable(module: ModuleId) -> Self {
        Self::new(module, Arc::new(NoScheduler))
    }

    /// Run `op` with `params` once, at `at` (or as soon as the daemon is up again, if it was down
    /// then). It runs in the op's own lane, as the scheduler's, so its events start a new chain
    /// (ADR 0035 §3).
    pub fn add_once(&self, op: &str, params: Value, at: DateTime<Utc>, label: &str) -> Result<TriggerId> {
        self.backend.add_once(&self.module, OneTime { op: op.to_owned(), params, at, label: label.to_owned() })
    }

    /// Cancel one this module added. `false` if it's gone already.
    pub fn remove(&self, id: &TriggerId) -> Result<bool> {
        self.backend.remove(&self.module, id)
    }
}

struct NoScheduler;

impl ScheduleBackend for NoScheduler {
    fn add_once(&self, _: &ModuleId, _: OneTime) -> Result<TriggerId> {
        Err(Error::unavailable("the scheduler isn't running yet"))
    }

    fn remove(&self, _: &ModuleId, _: &TriggerId) -> Result<bool> {
        Err(Error::unavailable("the scheduler isn't running yet"))
    }
}
