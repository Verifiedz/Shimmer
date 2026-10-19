//! The queue: N independent lanes, one pipeline for every unit of work (§1.2, §6).
//!
//! It runs what is in front of it. It never retries (§11.2), never stalls a lane on failure
//! (§11.1), and knows no module names: ops resolve through a table the daemon hands it.
//!
//! Ordering (§6.2, ADR 0003): one arrival-order list per lane. `Scheduled` and `Normal` are
//! provenance, not rank. Only a confirmed `Overridden` runs ahead of anything.
//!
//! Fallback (§11.3, ADR 0005): a task that fails structurally and carries a `notify.*`
//! fallback gets it enqueued in its own lane, after its slot is freed. Rules live in `fallback`.

pub mod fallback;
mod state;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use shimmer_core::{
    Clock, EnqueueRequest, Error, ErrorCode, Event, LaneConfig, LaneId, ModuleId, Priority, ProgressFn, Result, Task,
    TaskHandle, TaskId, TaskSubmitter,
};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::backend::Backend;
use state::LaneState;

/// Finished tasks kept for `queue.show` lookups.
const HISTORY: usize = 1000;
const SOURCE: &str = "queue";

/// Executes one op. Implemented by the daemon core; the queue just calls it.
#[async_trait]
pub trait OpRunner: Send + Sync {
    async fn run_op(
        &self,
        task: TaskId,
        op: &str,
        params: Value,
        cancel: CancellationToken,
        progress: ProgressFn,
    ) -> Result<Value>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    Queued,
    Running,
    Succeeded,
    Failed(String),
    Cancelled,
}

impl Status {
    fn name(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed(_) => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn is_finished(&self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

struct Record {
    task: Task,
    status: Status,
    fraction: f32,
    note: String,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    cancel: Option<CancellationToken>,
}

#[derive(Default)]
struct State {
    lanes: BTreeMap<LaneId, LaneState>,
    records: HashMap<TaskId, Record>,
    finished: VecDeque<TaskId>,
}

pub struct QueueInner {
    state: Mutex<State>,
    /// op -> lane its `CommandSpec` declares (None for inline ops).
    ops: HashMap<String, Option<LaneId>>,
    backend: Arc<Backend>,
    clock: Clock,
    runner: Weak<dyn OpRunner>,
    shutdown: CancellationToken,
    pub tracker: TaskTracker,
}

/// What `Ctx::queue` talks to. Holds a `Weak` so modules' contexts do not keep the queue
/// (and through it, every module) alive.
pub struct QueueFront(Weak<QueueInner>);

impl QueueFront {
    pub fn new(inner: &Arc<QueueInner>) -> Self {
        Self(Arc::downgrade(inner))
    }
}

#[async_trait]
impl TaskSubmitter for QueueFront {
    async fn submit(&self, req: EnqueueRequest) -> Result<TaskHandle> {
        match self.0.upgrade() {
            Some(q) => q.submit_request(req),
            None => Err(Error::unavailable("daemon is shutting down")),
        }
    }
}

impl QueueInner {
    pub fn new(
        lanes: &[LaneConfig],
        ops: HashMap<String, Option<LaneId>>,
        backend: Arc<Backend>,
        clock: Clock,
        runner: Weak<dyn OpRunner>,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        let state = State {
            lanes: lanes.iter().map(|l| (l.id.clone(), LaneState::new(l.clone()))).collect(),
            ..State::default()
        };
        Arc::new(Self { state: Mutex::new(state), ops, backend, clock, runner, shutdown, tracker: TaskTracker::new() })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn event(&self, topic: &str, payload: Value) -> Event {
        Event::new(ModuleId::new(SOURCE), topic, payload, self.clock.now())
    }

    /// Durable + published. A failure to log is reported, never allowed to wedge a lane.
    fn emit(&self, topic: &str, payload: Value) {
        if let Err(e) = shimmer_core::EventSink::emit(&*self.backend, self.event(topic, payload)) {
            tracing::error!(topic, error = %e, "could not log queue event");
        }
    }

    pub fn submit_request(self: &Arc<Self>, req: EnqueueRequest) -> Result<TaskHandle> {
        self.submit(req, None)
    }

    /// Enqueue as `Overridden`, ahead of every non-overridden waiting task (§6.2).
    ///
    /// Never silent: if it would displace anything, the request is refused with
    /// `confirmation_required` unless it echoes the lane's current `queue_version` with
    /// `confirm`. A stale version is refused again with the fresh picture. The queue holds
    /// nothing between the two calls, and a refusal changes nothing.
    pub fn submit_promoted(self: &Arc<Self>, req: EnqueueRequest, promotion: Promotion) -> Result<TaskHandle> {
        self.submit(req, Some(promotion))
    }

    /// The lane `req` would join, or why it can't be enqueued. Changes nothing: `finish` asks it
    /// before reporting a failure, so `queue.task.failed`'s `fallback` is decided before the
    /// fallback is enqueued, and its events come after the failure that caused it (#281's review).
    fn admits(&self, req: &EnqueueRequest) -> Result<LaneId> {
        // Whatever the caller, an invalid fallback never reaches a lane (ADR 0005).
        fallback::validate(&req.op, req.fallback.as_ref())?;
        let declared =
            self.ops.get(&req.op).ok_or_else(|| Error::unknown_op(format!("no module registered op '{}'", req.op)))?;
        let lane = req.lane.clone().or_else(|| declared.clone()).unwrap_or_else(LaneId::default_lane);
        if !self.lock().lanes.contains_key(&lane) {
            return Err(Error::lane_unknown(format!("no lane '{lane}'")));
        }
        Ok(lane)
    }

    fn submit(self: &Arc<Self>, req: EnqueueRequest, promotion: Option<Promotion>) -> Result<TaskHandle> {
        let lane = self.admits(&req)?;
        let now = self.clock.now();
        // Identity is out of `core` (§1.5), so the promoter is recorded as the role, not a person.
        let priority = match promotion {
            Some(_) => Priority::Overridden { by: "user".to_owned(), at: now },
            None => req.priority,
        };
        let task = Task {
            id: TaskId::new(),
            lane: lane.clone(),
            op: req.op,
            params: req.params,
            priority,
            origin: req.origin,
            fallback: req.fallback,
            enqueued_at: now,
        };
        let mut displaced = Vec::new();
        let (id, position) = {
            let mut st = self.lock();
            let ls = st.lanes.get_mut(&lane).ok_or_else(|| Error::lane_unknown(format!("no lane '{lane}'")))?;
            if let Some(p) = &promotion {
                let would = ls.displaced_by_override();
                let confirmed = p.confirm && p.queue_version == Some(ls.version);
                if !would.is_empty() && !confirmed {
                    return Err(confirmation_required(&lane, ls.version, &would));
                }
                displaced = would.iter().map(|t| t.id).collect();
            }
            let (id, position) = (task.id, ls.enqueue(task.clone()));
            st.records.insert(
                id,
                Record {
                    task: task.clone(),
                    status: Status::Queued,
                    fraction: 0.0,
                    note: String::new(),
                    started_at: None,
                    finished_at: None,
                    cancel: None,
                },
            );
            (id, position)
        };
        self.emit(
            "queue.task.enqueued",
            json!({"task_id": id, "lane": lane, "op": task.op, "origin": task.origin, "position": position}),
        );
        if promotion.is_some() {
            // The audit trail for "why did my scheduled fetch run late" (protocol.md).
            self.emit(
                "queue.task.promoted",
                json!({"task_id": id, "lane": lane, "op": task.op, "displaced": displaced}),
            );
        }
        self.pump(&lane);
        Ok(TaskHandle { task_id: id, lane, position })
    }

    /// Start whatever the lane has room for.
    fn pump(self: &Arc<Self>, lane: &LaneId) {
        if self.shutdown.is_cancelled() {
            return;
        }
        let started: Vec<(Task, CancellationToken)> = {
            let mut st = self.lock();
            let Some(ls) = st.lanes.get_mut(lane) else { return };
            let tasks = ls.take_startable();
            let now = self.clock.now();
            tasks
                .into_iter()
                .map(|t| {
                    let token = self.shutdown.child_token();
                    if let Some(r) = st.records.get_mut(&t.id) {
                        r.status = Status::Running;
                        r.started_at = Some(now);
                        r.cancel = Some(token.clone());
                    }
                    (t, token)
                })
                .collect()
        };
        for (task, token) in started {
            self.emit("queue.task.started", json!({"task_id": task.id, "lane": task.lane, "op": task.op}));
            let this = Arc::clone(self);
            self.tracker.spawn(async move { this.execute(task, token).await });
        }
    }

    async fn execute(self: Arc<Self>, task: Task, token: CancellationToken) {
        let result = match self.runner.upgrade() {
            None => Err(Error::unavailable("daemon is shutting down")),
            Some(runner) => {
                let progress = self.progress_fn(task.id, task.lane.clone());
                let (id, op, params, cancel) = (task.id, task.op.clone(), task.params.clone(), token.clone());
                // Own task, so a panicking module fails this task instead of the queue.
                let joined = tokio::spawn(async move { runner.run_op(id, &op, params, cancel, progress).await }).await;
                joined.unwrap_or_else(|e| Err(Error::internal(format!("task panicked: {e}"))))
            }
        };
        self.finish(task, &token, result);
    }

    fn progress_fn(self: &Arc<Self>, id: TaskId, lane: LaneId) -> ProgressFn {
        let this = Arc::downgrade(self);
        Arc::new(move |fraction, note| {
            let Some(q) = this.upgrade() else { return };
            if let Some(r) = q.lock().records.get_mut(&id) {
                r.fraction = fraction;
                r.note = note.to_owned();
            }
            q.backend.publish_ephemeral(q.event(
                "queue.task.progress",
                json!({"task_id": id, "lane": lane, "fraction": fraction, "note": note}),
            ));
        })
    }

    fn finish(self: &Arc<Self>, task: Task, token: &CancellationToken, result: Result<Value>) {
        // Only a task that ran and failed structurally can owe a fallback (§11.3).
        let mut fallback = None;
        let (status, topic, mut payload) = if token.is_cancelled() {
            (Status::Cancelled, "queue.task.cancelled", json!({"task_id": task.id, "lane": task.lane, "op": task.op}))
        } else {
            match result {
                Ok(v) => (
                    Status::Succeeded,
                    "queue.task.finished",
                    json!({"task_id": task.id, "lane": task.lane, "op": task.op, "result": v}),
                ),
                Err(e) => {
                    fallback = fallback::request(&task, &e);
                    (
                        Status::Failed(e.to_string()),
                        "queue.task.failed",
                        json!({"task_id": task.id, "lane": task.lane, "op": task.op, "origin": task.origin,
                               "error": e.message, "code": e.code, "detail": e.detail,
                               "retryable": e.is_retryable(), "attempts": 1}),
                    )
                }
            }
        };
        {
            let mut st = self.lock();
            if let Some(ls) = st.lanes.get_mut(&task.lane) {
                ls.finish(task.id);
            }
            record_finished(&mut st, task.id, status, self.clock.now());
        }
        // `fallback` says whether one is really being sent (ADR 0032 §4): notify's alerts stay
        // quiet only when it is (#276's review). Decided by asking the queue first, and the
        // fallback is enqueued only after the failure is reported, so the log reads cause, then
        // effect (#281's review). Shutting down sends none: nothing would start it.
        let fallback = fallback.filter(|_| !self.shutdown.is_cancelled());
        let admitted = fallback.as_ref().map(|request| self.admits(request));
        if topic == "queue.task.failed" {
            payload["fallback"] = json!(matches!(admitted, Some(Ok(_))));
        }
        self.emit(topic, payload);
        if let (Some(request), Some(admitted)) = (fallback, admitted) {
            self.enqueue_fallback(&task, request, admitted);
        }
        self.pump(&task.lane);
    }

    /// Degrade, don't drop (§11.3). The slot is already free and the failure already reported,
    /// so nothing here can hold the lane: a fallback that cannot be enqueued is one more event,
    /// never a stall and never a retry.
    fn enqueue_fallback(self: &Arc<Self>, failed: &Task, request: EnqueueRequest, admitted: Result<LaneId>) {
        let op = request.op.clone();
        match admitted.and_then(|_| self.submit(request, None)) {
            Ok(h) => self.emit(
                "queue.fallback.enqueued",
                json!({"task_id": failed.id, "fallback_task_id": h.task_id, "op": op, "lane": h.lane}),
            ),
            Err(e) => self.emit(
                "queue.fallback.failed",
                json!({"task_id": failed.id, "op": op, "error": e.message, "code": e.code}),
            ),
        }
    }

    pub fn cancel(self: &Arc<Self>, id: TaskId) -> Result<Value> {
        let mut st = self.lock();
        let status =
            st.records.get(&id).map(|r| r.status.clone()).ok_or_else(|| Error::not_found(format!("no task {id}")))?;
        match status {
            Status::Queued => {
                let lane = st.records[&id].task.lane.clone();
                if let Some(ls) = st.lanes.get_mut(&lane) {
                    ls.remove_queued(id);
                }
                let task = st.records[&id].task.clone();
                record_finished(&mut st, id, Status::Cancelled, self.clock.now());
                drop(st);
                self.emit(
                    "queue.task.cancelled",
                    json!({"task_id": id, "lane": task.lane, "op": task.op, "was": "queued"}),
                );
                Ok(json!({"cancelled": true, "was": "queued", "task_id": id}))
            }
            Status::Running => {
                // Cooperative: the task ends when it next polls `ctx.cancel`.
                if let Some(t) = &st.records[&id].cancel {
                    t.cancel();
                }
                Ok(json!({"cancelled": true, "was": "running", "task_id": id}))
            }
            done => Err(Error::not_cancellable(format!("task {id} already {}", done.name()))),
        }
    }

    /// Move a waiting task within its lane (§6.2, ADR 0006). `queue_version` is the
    /// optimistic-concurrency token the client was last shown; a stale one is `conflict`,
    /// echoing the lane's current version so the client can re-fetch and retry — the same
    /// round trip `queue.list` → act → stale → re-fetch already uses for promotion.
    pub fn reorder(
        self: &Arc<Self>,
        lane: &LaneId,
        id: TaskId,
        before: Option<TaskId>,
        queue_version: u64,
    ) -> Result<Value> {
        let new_version = {
            let mut st = self.lock();
            let ls = st.lanes.get_mut(lane).ok_or_else(|| Error::lane_unknown(format!("no lane '{lane}'")))?;
            if ls.version != queue_version {
                return Err(Error::conflict(format!(
                    "lane '{lane}' is at queue_version {}, not {queue_version}",
                    ls.version
                ))
                .with_detail(json!({"lane": lane, "queue_version": ls.version})));
            }
            ls.reorder(id, before)?;
            ls.version
        };
        // `LaneState::reorder` only bumps `version` when the order actually changed, so this
        // also guards against emitting a "reordered" event for a no-op move (e.g. placing a
        // task before the task already directly after it).
        if new_version != queue_version {
            self.emit(
                "queue.task.reordered",
                json!({"task_id": id, "lane": lane, "before": before, "queue_version": new_version}),
            );
        }
        Ok(json!({"queue_version": new_version, "task_id": id}))
    }

    /// A task id as a client wrote it: the full id, or a unique ending of one, case insensitive
    /// (ADR 0031 §4). A ULID starts with its creation time, so ids made close together share
    /// their start; the end is random. Several matches are `conflict`, none `not_found`.
    pub fn resolve(&self, written: &str) -> Result<TaskId> {
        if let Ok(id) = written.parse::<TaskId>() {
            return Ok(id);
        }
        let st = self.lock();
        let ids: Vec<String> = st.records.keys().map(ToString::to_string).collect();
        drop(st);
        shimmer_core::short_id::resolve(written, &ids, "task")?.parse()
    }

    pub fn task_view(&self, id: TaskId) -> Result<Value> {
        let st = self.lock();
        let r = st.records.get(&id).ok_or_else(|| Error::not_found(format!("no task {id}")))?;
        Ok(view(r))
    }

    pub fn list(&self, only: Option<&LaneId>) -> Result<Value> {
        let st = self.lock();
        if let Some(l) = only {
            if !st.lanes.contains_key(l) {
                return Err(Error::lane_unknown(format!("no lane '{l}'")));
            }
        }
        let lanes: Vec<Value> = st
            .lanes
            .values()
            .filter(|l| only.is_none_or(|o| *o == l.cfg.id))
            .map(|l| {
                let views = |ids: &mut dyn Iterator<Item = &TaskId>| -> Vec<Value> {
                    ids.filter_map(|id| st.records.get(id)).map(view).collect()
                };
                json!({
                    "id": l.cfg.id,
                    "max_concurrent": l.cfg.max_concurrent,
                    "queue_version": l.version,
                    "running": views(&mut l.running.iter()),
                    "queued": views(&mut l.queued.iter().map(|t| &t.id)),
                })
            })
            .collect();
        Ok(json!({ "lanes": lanes }))
    }
}

/// What the client said in the `queue` block of a request that asked for `override`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Promotion {
    pub confirm: bool,
    /// Echoed from the `confirmation_required` the user was shown.
    pub queue_version: Option<u64>,
}

fn confirmation_required(lane: &LaneId, queue_version: u64, would: &[&Task]) -> Error {
    let list: Vec<Value> = would.iter().map(|t| json!({"task_id": t.id, "op": t.op, "priority": t.priority})).collect();
    Error::new(ErrorCode::ConfirmationRequired, format!("would displace {} task(s) in lane '{lane}'", would.len()))
        .with_detail(json!({"lane": lane, "queue_version": queue_version, "would_displace": list}))
}

fn record_finished(st: &mut State, id: TaskId, status: Status, at: DateTime<Utc>) {
    if let Some(r) = st.records.get_mut(&id) {
        r.status = status;
        r.finished_at = Some(at);
        r.cancel = None;
    }
    st.finished.push_back(id);
    while st.finished.len() > HISTORY {
        if let Some(old) = st.finished.pop_front() {
            if st.records.get(&old).is_some_and(|r| r.status.is_finished()) {
                st.records.remove(&old);
            }
        }
    }
}

fn view(r: &Record) -> Value {
    let error = match &r.status {
        Status::Failed(e) => Some(e.clone()),
        _ => None,
    };
    json!({
        "task_id": r.task.id,
        "lane": r.task.lane,
        "op": r.task.op,
        "status": r.status.name(),
        "priority": r.task.priority,
        "origin": r.task.origin,
        "progress": {"fraction": r.fraction, "note": r.note},
        "attempts": if r.started_at.is_some() { 1 } else { 0 },
        "enqueued_at": r.task.enqueued_at,
        "started_at": r.started_at,
        "finished_at": r.finished_at,
        "error": error,
    })
}
