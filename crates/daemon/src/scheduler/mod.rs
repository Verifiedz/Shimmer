//! The scheduler owns *when*, and nothing else (§1.2). When a trigger fires it enqueues a
//! task through the queue's public submit call; it never executes anything and never knows
//! what an op does.
//!
//! * Time comes from the injected `Clock`, never `Utc::now()`, so tests advance a fake clock.
//! * Last-run and next-due are persisted (`data/scheduler/triggers/<id>.json`), not memory.
//! * State is written *before* the task is enqueued: a crash in between loses one firing
//!   rather than repeating it. Duplicate emails are the worse failure (§6.3).
//! * A trigger never overlaps itself: if a task from an earlier firing is still queued or
//!   running, the new firing is dropped and reported as `scheduler.trigger.skipped`.
//! * `tick` is a plain synchronous function; the async loop only sleeps between calls.

mod plan;

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use shimmer_core::{
    CatchUp, Clock, Emitter, EnqueueRequest, Error, Fallback, LaneId, LocalTimezone, ModuleId, NamespacedStore, Origin,
    Priority, Result, Schedule, TaskId, TriggerId, TriggerSpec,
};
use tokio_util::sync::CancellationToken;

use crate::queue::{fallback, QueueInner};
use plan::Parsed;

const DIR: &str = "triggers";

/// Who owns a trigger. Module-declared ones are re-derived from `Module::triggers()` on
/// every start, so users may pause them but not remove them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Source {
    User,
    Module { id: ModuleId },
}

/// What is written to disk.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Stored {
    spec: TriggerSpec,
    source: Source,
    paused: bool,
    last_run: Option<DateTime<Utc>>,
    next_due: Option<DateTime<Utc>>,
    /// A finished `Once`. Kept, so a module re-declaring it does not fire it again.
    done: bool,
    /// The zone a `CronLocal` trigger's `next_due` was computed in (ADR 0031 §2), so a restart
    /// in another zone can move it. Absent for every other schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    zone: Option<String>,
}

struct Entry {
    stored: Stored,
    parsed: Parsed,
    /// Tasks from earlier firings. In memory on purpose: the queue's own state is too.
    pending: Vec<TaskId>,
}

pub struct Scheduler {
    state: Mutex<BTreeMap<String, Entry>>,
    store: NamespacedStore,
    emitter: Emitter,
    clock: Clock,
    queue: Arc<QueueInner>,
    known_ops: HashSet<String>,
    lanes: HashSet<LaneId>,
    /// `[general] local_timezone`, for `CronLocal` schedules (ADR 0031 §2).
    local_tz: LocalTimezone,
}

fn valid_trigger_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && !id.starts_with('.')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn path(id: &str) -> String {
    format!("{DIR}/{id}.json")
}

impl Scheduler {
    pub fn new(
        store: NamespacedStore,
        emitter: Emitter,
        clock: Clock,
        queue: Arc<QueueInner>,
        known_ops: HashSet<String>,
        lanes: HashSet<LaneId>,
        local_tz: LocalTimezone,
    ) -> Self {
        Self { state: Mutex::default(), store, emitter, clock, queue, known_ops, lanes, local_tz }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Entry>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ------------------------------------------------------------------ persistence

    fn write(&self, stored: &Stored, event: Option<(&str, Value)>) -> Result<()> {
        let json = serde_json::to_vec_pretty(stored).map_err(|e| Error::internal(format!("encode trigger: {e}")))?;
        self.store.transaction(|tx| {
            tx.put(&path(stored.spec.id.as_str()), json)?;
            match event {
                Some((topic, payload)) => tx.emit(topic, payload),
                None => Ok(()),
            }
        })
    }

    fn delete(&self, id: &str, event: (&str, Value)) -> Result<()> {
        self.store.transaction(|tx| {
            tx.delete(&path(id))?;
            tx.emit(event.0, event.1)
        })
    }

    /// Read persisted triggers. A corrupt file is reported and skipped, not fatal: one bad
    /// hand-edit must not stop the daemon.
    pub fn load(&self) -> Result<()> {
        let mut st = self.lock();
        for file in self.store.list(DIR)? {
            let Some(text) = self.store.read_string(&file)? else { continue };
            let parsed = serde_json::from_str::<Stored>(&text)
                .map_err(|e| Error::invalid_params(e.to_string()))
                .and_then(|s| fallback::validate(&s.spec.op, s.spec.fallback.as_ref()).map(|()| s))
                .and_then(|s| Parsed::parse(&s.spec.schedule, self.local_tz).map(|p| (s, p)));
            match parsed {
                Ok((mut stored, parsed)) => {
                    // The zone is read at start and may have changed (ADR 0031 §2). Same zone: the
                    // stored next run is kept exactly, so a run missed while the daemon was down
                    // is still caught up. Another zone: the same wall-clock time there.
                    if parsed.is_local() {
                        let was = stored.zone.as_deref().and_then(|z| LocalTimezone::parse(z).ok());
                        stored.zone = Some(self.local_tz.name().to_owned());
                        if let Some(was) = was.filter(|was| *was != self.local_tz) {
                            stored.next_due = stored.next_due.and_then(|due| parsed.rezoned(due, was, stored.last_run));
                            self.write(&stored, None)?;
                        }
                    }
                    st.insert(stored.spec.id.to_string(), Entry { stored, parsed, pending: Vec::new() });
                }
                Err(e) => tracing::error!(file, error = %e, "ignoring unreadable trigger file"),
            }
        }
        Ok(())
    }

    /// Reconcile persisted state with what modules declare now. Keeps last-run and paused for
    /// triggers that still exist, and drops module triggers that are no longer declared.
    pub fn register_module_triggers(&self, declared: Vec<(ModuleId, TriggerSpec)>) -> Result<()> {
        let now = self.clock.now();
        let mut st = self.lock();
        let mut seen = HashSet::new();
        for (module, spec) in declared {
            let id = spec.id.to_string();
            if !valid_trigger_id(&id) {
                return Err(Error::invalid_params(format!("module '{module}': invalid trigger id '{id}'")));
            }
            if !seen.insert(id.clone()) {
                return Err(Error::conflict(format!("trigger id '{id}' declared twice")));
            }
            // A trigger aimed at an op that is not there degrades; it does not stop the daemon.
            if let Err(e) = self.check_target(&spec) {
                tracing::warn!(module = %module, trigger = %id, error = %e, "skipping trigger");
                continue;
            }
            let parsed = match Parsed::parse(&spec.schedule, self.local_tz) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(module = %module, trigger = %id, error = %e, "skipping trigger");
                    continue;
                }
            };
            let source = Source::Module { id: module };
            match st.get_mut(&id) {
                Some(e) if e.stored.source != source => {
                    return Err(Error::conflict(format!("trigger id '{id}' is already taken by another owner")));
                }
                Some(e) => {
                    if e.stored.spec == spec {
                        continue;
                    }
                    let mut next = e.stored.clone();
                    if e.stored.spec.schedule != spec.schedule {
                        next.next_due = parsed.first_due(now);
                        next.done = false;
                        next.zone = parsed.is_local().then(|| self.local_tz.name().to_owned());
                    }
                    next.spec = spec;
                    self.write(&next, None)?;
                    e.stored = next;
                    e.parsed = parsed;
                }
                None => {
                    let stored = Stored {
                        next_due: parsed.first_due(now),
                        spec,
                        source,
                        paused: false,
                        last_run: None,
                        done: false,
                        zone: parsed.is_local().then(|| self.local_tz.name().to_owned()),
                    };
                    self.write(&stored, Some(("scheduler.trigger.added", added_payload(&stored))))?;
                    st.insert(id, Entry { stored, parsed, pending: Vec::new() });
                }
            }
        }
        let stale: Vec<String> = st
            .iter()
            .filter(|(id, e)| matches!(e.stored.source, Source::Module { .. }) && !seen.contains(*id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.delete(&id, ("scheduler.trigger.removed", json!({"trigger_id": id, "reason": "no longer declared"})))?;
            st.remove(&id);
        }
        Ok(())
    }

    fn check_target(&self, spec: &TriggerSpec) -> Result<()> {
        // Shape first: a fallback that breaks ADR 0005 is wrong whichever modules are loaded.
        fallback::validate(&spec.op, spec.fallback.as_ref())?;
        if !self.known_ops.contains(&spec.op) {
            return Err(Error::unknown_op(format!("no module registered op '{}'", spec.op)));
        }
        match &spec.lane {
            Some(l) if !self.lanes.contains(l) => Err(Error::lane_unknown(format!("no lane '{l}'"))),
            _ => Ok(()),
        }
    }

    // ------------------------------------------------------------------ firing

    /// Fire everything that is due at the clock's current time.
    pub fn tick(&self) {
        let now = self.clock.now();
        for entry in self.lock().values_mut() {
            if entry.stored.paused || entry.stored.done {
                continue;
            }
            if entry.stored.next_due.is_some_and(|due| due <= now) {
                self.fire_due(entry, now);
            }
        }
    }

    fn task_pending(&self, id: TaskId) -> bool {
        self.queue.task_view(id).map(|v| matches!(v["status"].as_str(), Some("queued" | "running"))).unwrap_or(false)
    }

    fn fire_due(&self, e: &mut Entry, now: DateTime<Utc>) {
        let Some(due) = e.stored.next_due else { return };
        let decision = plan::decide(&e.parsed, e.stored.spec.catch_up, due, now);
        e.pending.retain(|id| self.task_pending(*id));
        let overlap = !decision.fire.is_empty() && !e.pending.is_empty();

        let mut next = e.stored.clone();
        next.next_due = decision.next_due;
        next.done = decision.next_due.is_none();
        if !decision.fire.is_empty() && !overlap {
            next.last_run = Some(now);
        }
        // Persist first (at-most-once). If that fails nothing has happened and the next tick retries.
        if let Err(err) = self.write(&next, None) {
            tracing::error!(trigger = %e.stored.spec.id, error = %err, "could not persist trigger state; will retry");
            return;
        }
        e.stored = next;

        let spec = &e.stored.spec;
        let id = spec.id.to_string();
        if decision.missed > 0 {
            self.emit(
                "scheduler.trigger.missed",
                json!({"trigger_id": id, "count": decision.missed, "catch_up": spec.catch_up}),
            );
        }
        if overlap {
            self.emit(
                "scheduler.trigger.skipped",
                json!({"trigger_id": id, "reason": "overlap", "dropped": decision.fire.len(), "pending": e.pending}),
            );
            return;
        }
        for scheduled_for in decision.fire {
            let request = EnqueueRequest {
                op: spec.op.clone(),
                params: with_scheduled_for(&spec.params, scheduled_for),
                lane: spec.lane.clone(),
                priority: Priority::Scheduled,
                origin: Origin::Scheduler { trigger_id: TriggerId::new(id.as_str()) },
                fallback: spec.fallback.clone(),
            };
            match self.queue.submit_request(request) {
                Ok(h) => {
                    e.pending.push(h.task_id);
                    self.emit(
                        "scheduler.trigger.fired",
                        json!({"trigger_id": id, "op": spec.op, "task_id": h.task_id, "lane": h.lane,
                               "scheduled_for": scheduled_for, "late_by_s": (now - scheduled_for).num_seconds()}),
                    );
                }
                Err(err) => self.emit(
                    "scheduler.trigger.skipped",
                    json!({"trigger_id": id, "reason": "enqueue_failed", "error": err.message, "code": err.code}),
                ),
            }
        }
    }

    fn emit(&self, topic: &str, payload: Value) {
        if let Err(e) = self.emitter.emit(topic, payload) {
            tracing::error!(topic, error = %e, "could not log scheduler event");
        }
    }

    /// Poll until shutdown. The first tick is immediate, so downtime is caught up on start.
    pub async fn run(self: Arc<Self>, every: Duration, shutdown: CancellationToken) {
        loop {
            self.tick();
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(every) => {}
            }
        }
    }

    // ------------------------------------------------------------------ ops

    pub fn handle(&self, op: &str, params: Value) -> Result<Value> {
        match op {
            "scheduler.list" => Ok(self.list()),
            "scheduler.get" => self.get(&self.resolve(&trigger_id(params)?)?),
            "scheduler.add" => self.add(params),
            "scheduler.remove" => self.remove(&self.resolve(&trigger_id(params)?)?),
            "scheduler.pause" => self.set_paused(&self.resolve(&trigger_id(params)?)?, true),
            "scheduler.resume" => self.set_paused(&self.resolve(&trigger_id(params)?)?, false),
            _ => Err(Error::unknown_op(format!("no such scheduler op '{op}'"))),
        }
    }

    fn list(&self) -> Value {
        let triggers: Vec<Value> = self.lock().values().map(|e| trigger_view(&e.stored)).collect();
        json!({ "triggers": triggers })
    }

    /// One trigger, in exactly the shape a `scheduler.list` entry has (#76) — `queue show`
    /// uses `queue.task` and `workspaces status` uses `workspaces.status`; this was the odd
    /// one out, picking its one trigger out of the full list instead.
    /// A trigger id as a client wrote it: the full id, or a unique ending of one, case insensitive
    /// (ADR 0031 §4). Several matches are `conflict`, none `not_found`.
    fn resolve(&self, written: &str) -> Result<String> {
        let ids: Vec<String> = self.lock().keys().cloned().collect();
        if ids.iter().any(|id| id == written) {
            return Ok(written.to_owned());
        }
        crate::short_id::resolve(written, &ids, "trigger")
    }

    fn get(&self, id: &str) -> Result<Value> {
        let st = self.lock();
        let e = st.get(id).ok_or_else(|| Error::not_found(format!("no trigger '{id}'")))?;
        Ok(trigger_view(&e.stored))
    }

    fn add(&self, params: Value) -> Result<Value> {
        // `catch_up` has no default: guessing wrong sends duplicate emails (§6.3).
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Add {
            schedule: Schedule,
            op: String,
            #[serde(default)]
            params: Value,
            catch_up: CatchUp,
            lane: Option<LaneId>,
            fallback: Option<Fallback>,
        }
        let a: Add = serde_json::from_value(params).map_err(|e| Error::invalid_params(e.to_string()))?;
        let spec = TriggerSpec {
            id: TriggerId::new(format!("usr-{}", ulid::Ulid::new().to_string().to_lowercase())),
            schedule: a.schedule,
            catch_up: a.catch_up,
            op: a.op,
            params: a.params,
            lane: a.lane,
            fallback: a.fallback,
        };
        self.check_target(&spec)?;
        let parsed = Parsed::parse(&spec.schedule, self.local_tz)?;
        let stored = Stored {
            next_due: parsed.first_due(self.clock.now()),
            spec,
            source: Source::User,
            paused: false,
            last_run: None,
            done: false,
            zone: parsed.is_local().then(|| self.local_tz.name().to_owned()),
        };
        self.write(&stored, Some(("scheduler.trigger.added", added_payload(&stored))))?;
        let id = stored.spec.id.clone();
        self.lock().insert(id.to_string(), Entry { stored, parsed, pending: Vec::new() });
        Ok(json!({ "trigger_id": id }))
    }

    fn remove(&self, id: &str) -> Result<Value> {
        let mut st = self.lock();
        let e = st.get(id).ok_or_else(|| Error::not_found(format!("no trigger '{id}'")))?;
        if let Source::Module { id: owner } = &e.stored.source {
            return Err(Error::conflict(format!("trigger '{id}' is declared by module '{owner}'; pause it instead")));
        }
        self.delete(id, ("scheduler.trigger.removed", json!({"trigger_id": id, "reason": "removed by user"})))?;
        st.remove(id);
        Ok(json!({"removed": true}))
    }

    fn set_paused(&self, id: &str, paused: bool) -> Result<Value> {
        let mut st = self.lock();
        let e = st.get_mut(id).ok_or_else(|| Error::not_found(format!("no trigger '{id}'")))?;
        if e.stored.paused == paused {
            return Ok(json!({ "paused": paused }));
        }
        let mut next = e.stored.clone();
        next.paused = paused;
        if !paused && !next.done && !matches!(next.spec.schedule, Schedule::Once(_)) {
            // Time spent paused is not "missed": resume from now instead of replaying it.
            next.next_due = e.parsed.next_after(self.clock.now());
        }
        let topic = if paused { "scheduler.trigger.paused" } else { "scheduler.trigger.resumed" };
        self.write(&next, Some((topic, json!({"trigger_id": id}))))?;
        e.stored = next;
        Ok(json!({ "paused": paused }))
    }
}

/// The wire shape of one trigger, shared by `scheduler.list` (one per entry) and
/// `scheduler.get` (one, by id) so they can never drift apart.
fn trigger_view(s: &Stored) -> Value {
    json!({"id": s.spec.id, "source": s.source, "schedule": s.spec.schedule, "op": s.spec.op,
           "params": s.spec.params, "catch_up": s.spec.catch_up, "lane": s.spec.lane,
           "fallback": s.spec.fallback, "next_due": s.next_due, "last_run": s.last_run,
           "paused": s.paused, "done": s.done})
}

fn trigger_id(params: Value) -> Result<String> {
    #[derive(Deserialize)]
    struct P {
        trigger_id: String,
    }
    serde_json::from_value::<P>(params).map(|p| p.trigger_id).map_err(|e| Error::invalid_params(e.to_string()))
}

fn added_payload(s: &Stored) -> Value {
    json!({"trigger_id": s.spec.id, "op": s.spec.op, "source": s.source, "next_due": s.next_due})
}

/// Stamp the scheduled time into object params so a `Backfill` run knows which period it is
/// for. Non-object params are passed through untouched.
fn with_scheduled_for(params: &Value, at: DateTime<Utc>) -> Value {
    match params {
        Value::Null => json!({ "scheduled_for": at }),
        Value::Object(m) => {
            let mut m = m.clone();
            m.insert("scheduled_for".into(), json!(at));
            Value::Object(m)
        }
        other => other.clone(),
    }
}
