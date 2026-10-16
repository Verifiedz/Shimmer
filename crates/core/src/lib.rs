//! `shimmer-core`: the shared contract between daemon, modules and clients.
//!
//! Change-controlled (CLAUDE.md §4). Depends on nothing internal, and never learns what a
//! user is (§1.5): no identity, session or token types belong here.

pub mod bus;
pub mod catalog;
pub mod clock;
pub mod ctx;
pub mod desktop;
pub mod error;
pub mod event;
pub mod filter;
pub mod http;
pub mod ids;
pub mod launcher;
pub mod manifest;
pub mod module;
pub mod params;
pub mod queue;
pub mod retry;
pub mod schedules;
pub mod store;
pub mod sync;
pub mod task;
pub mod text;
pub mod time;
pub mod topic;
pub mod trigger;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use serde_json::Value;

pub use bus::{Emitter, EventSink};
pub use catalog::Catalog;
pub use clock::Clock;
pub use ctx::{Ctx, ModuleConfig, ProgressFn};
pub use desktop::{Desktop, DesktopBackend, DesktopNote, Urgency};
pub use error::{Error, ErrorCode, Result};
pub use event::Event;
pub use http::{HttpBackend, HttpGateway, HttpResponse};
pub use ids::{LaneId, ModuleId, TaskId, TriggerId};
pub use launcher::{LaunchBackend, LaunchStep, Launcher, SpawnMode, Step, StepOutcome};
pub use manifest::{CommandSpec, Execution, LaneConfig, Manifest};
pub use module::Module;
pub use queue::{EnqueueRequest, QueueHandle, TaskHandle, TaskSubmitter};
pub use retry::RetryPolicy;
pub use schedules::{OneTime, ScheduleBackend, Schedules};
pub use store::{NamespacedStore, StoreBackend, Tx, TxPlan, Write};
pub use sync::WriteLock;
pub use task::{Fallback, NotifyPriority, Origin, Priority, Task, TaskOutcome};
pub use time::{local_date, LocalTimezone};
pub use trigger::{CatchUp, Schedule, TriggerSpec};
