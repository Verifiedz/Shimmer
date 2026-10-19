//! The scheduler owns *when*, and nothing else (§1.2). When a trigger fires it enqueues a
//! task through the queue's public submit call; it never executes anything and never knows
//! what an op does.
//!
//! * Time comes from the injected `Clock`, never `Utc::now()`, so tests advance a fake clock.
//! * Last-run and next-due are persisted (`data/scheduler/triggers/<id>.json`), not memory.
//! * State is written *before* the task is enqueued: a crash in between never repeats a
//!   firing. Duplicate emails are the worse failure (§6.3).
//! * A firing is remembered on disk (`unfinished`) until its task ends, so a stop while the task
//!   still waits in its lane doesn't lose it: on the next start, a task the event log says never
//!   started is queued again, and one that started but never ended is reported as interrupted,
//!   never run twice (#263 finding 1, ADR 0004 amendment).
//! * A schedule file that can't be read is skipped but listed as broken, never overwritten
//!   (#263 finding 5).
//! * A trigger never overlaps itself: if a task from an earlier firing is still queued or
//!   running, the new firing is dropped and reported as `scheduler.trigger.skipped`.
//! * `tick` is a plain synchronous function; the async loop only sleeps between calls.
//! * A person's removed schedule waits in `removed/<id>.json` until restored or purged
//!   (ADR 0038). It is never loaded, so it never fires.

mod plan;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use shimmer_core::{
    CatchUp, Clock, Emitter, EnqueueRequest, Error, Event, Fallback, LaneId, LocalTimezone, ModuleId, NamespacedStore,
    OneTime, Origin, Priority, Result, Schedule, ScheduleBackend, TaskId, TriggerId, TriggerSpec,
};
use tokio_util::sync::CancellationToken;

use crate::queue::{fallback, QueueInner};
use plan::Parsed;

const DIR: &str = "triggers";
/// Removed user schedules, until `scheduler.restore` or `scheduler.purge` (ADR 0038).
const REMOVED: &str = "removed";

/// Who owns a trigger. Module-declared ones are re-derived from `Module::triggers()` on
/// every start, so users may pause them but not remove them. `Added` ones are one-time schedules
/// a module added while running (ADR 0035 §5.6): kept across restarts, removed by that module or
/// once they've run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Source {
    User,
    Module { id: ModuleId },
    Added { id: ModuleId, label: String },
}

/// At most this many one-time schedules waiting per module: a runaway rule fills this, not the disk.
const MAX_ADDED: usize = 1000;
/// A label is one line for `scheduler list`.
const MAX_LABEL: usize = 200;

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
    /// Firings handed to the queue whose task hasn't ended, by the time each was for. On disk so
    /// a restart can tell a task that never started from one that did (#263 finding 1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    unfinished: Vec<DateTime<Utc>>,
    /// The latest firing a stop cut off while its task ran: never run again, shown instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interrupted: Option<Interrupted>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Interrupted {
    task_id: TaskId,
    scheduled_for: DateTime<Utc>,
    /// When the restart noticed it.
    noticed: DateTime<Utc>,
}

struct Entry {
    stored: Stored,
    parsed: Parsed,
    /// Tasks from earlier firings, with the time each was for. In memory, like the queue's own
    /// state; `Stored::unfinished` is what survives a restart.
    pending: Vec<(TaskId, DateTime<Utc>)>,
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
    /// Schedule files that couldn't be read, by id: listed, never overwritten, removable
    /// (#263 finding 5). The file and why.
    broken: Mutex<BTreeMap<String, (String, String)>>,
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

fn removed_path(id: &str) -> String {
    format!("{REMOVED}/{id}.json")
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
        Self {
            state: Mutex::default(),
            store,
            emitter,
            clock,
            queue,
            known_ops,
            lanes,
            local_tz,
            broken: Mutex::default(),
        }
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
                        let unrecorded = was.is_none();
                        stored.zone = Some(self.local_tz.name().to_owned());
                        let moved = match was.filter(|was| *was != self.local_tz) {
                            Some(was) => {
                                stored.next_due =
                                    stored.next_due.and_then(|due| parsed.rezoned(due, was, stored.last_run));
                                true
                            }
                            None => false,
                        };
                        // Saved so a later zone change can move it. A failed write is only logged,
                        // like an unreadable file: the move is kept in memory and simply redone on
                        // the next start, and one bad file never stops the daemon (#184's review).
                        if moved || unrecorded {
                            if let Err(e) = self.write(&stored, None) {
                                tracing::warn!(file, error = %e, "couldn't save a local trigger's zone; redone next start");
                            }
                        }
                    }
                    st.insert(stored.spec.id.to_string(), Entry { stored, parsed, pending: Vec::new() });
                }
                Err(e) => {
                    tracing::error!(file, error = %e, "skipping unreadable trigger file");
                    let id = file.trim_start_matches(&format!("{DIR}/")).trim_end_matches(".json").to_owned();
                    self.broken().insert(id, (format!("data/scheduler/{file}"), e.message));
                }
            }
        }
        Ok(())
    }

    fn broken(&self) -> MutexGuard<'_, BTreeMap<String, (String, String)>> {
        self.broken.lock().unwrap_or_else(|e| e.into_inner())
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
                        unfinished: Vec::new(),
                        interrupted: None,
                    };
                    // Declared again, so a broken file of it is replaced by the declaration.
                    self.write(&stored, Some(("scheduler.trigger.added", added_payload(&stored))))?;
                    self.broken().remove(&id);
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

    /// A `once` already past with `skip` could never run (#263 finding 3): refused, rather than
    /// made and left waiting forever. `run_once`/`backfill` run it now, as asked.
    fn refuse_past_skip(&self, spec: &TriggerSpec) -> Result<()> {
        match spec.schedule {
            Schedule::Once(at) if spec.catch_up == CatchUp::Skip && at <= self.clock.now() => {
                Err(Error::invalid_params(format!(
                    "that time ({at}) has passed, and catch-up skip would never run it; \
                     use catch-up run_once to run it now"
                )))
            }
            _ => Ok(()),
        }
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
        let mut st = self.lock();
        let mut gone = Vec::new();
        for (id, entry) in st.iter_mut() {
            let ended: Vec<DateTime<Utc>> =
                entry.pending.iter().filter(|(task, _)| !self.task_pending(*task)).map(|(_, at)| *at).collect();
            if !ended.is_empty() && !self.forget(entry, &ended) {
                gone.push(id.clone());
                continue;
            }
            if entry.stored.paused || entry.stored.done {
                continue;
            }
            if entry.stored.next_due.is_some_and(|due| due <= now) && !self.fire_due(entry, now) {
                gone.push(id.clone());
            }
        }
        // A module's one-time schedule is gone once its task has ended: its file was deleted.
        for id in gone {
            st.remove(&id);
        }
    }

    fn task_pending(&self, id: TaskId) -> bool {
        self.queue.task_view(id).map(|v| matches!(v["status"].as_str(), Some("queued" | "running"))).unwrap_or(false)
    }

    /// Forget these firings: their task ended, or never got into the queue. On disk first; a
    /// failed write keeps them and the next tick tries again. A module's finished one-time
    /// schedule is deleted once nothing of it is left. False when it was deleted.
    fn forget(&self, e: &mut Entry, ats: &[DateTime<Utc>]) -> bool {
        let mut next = e.stored.clone();
        next.unfinished.retain(|at| !ats.contains(at));
        let finished = matches!(next.source, Source::Added { .. }) && next.done && next.unfinished.is_empty();
        let saved = if finished {
            self.delete(
                next.spec.id.as_str(),
                ("scheduler.trigger.removed", json!({"trigger_id": next.spec.id, "reason": "ran"})),
            )
        } else {
            self.write(&next, None)
        };
        if let Err(err) = saved {
            tracing::error!(trigger = %e.stored.spec.id, error = %err, "could not persist trigger state; will retry");
            return true;
        }
        e.pending.retain(|(_, at)| !ats.contains(at));
        e.stored = next;
        !finished
    }

    /// False when the entry is gone (a module's one-time schedule that fired nothing).
    fn fire_due(&self, e: &mut Entry, now: DateTime<Utc>) -> bool {
        let Some(due) = e.stored.next_due else { return true };
        let decision = plan::decide(&e.parsed, e.stored.spec.catch_up, due, now);
        let overlap = !decision.fire.is_empty() && e.pending.iter().any(|(task, _)| self.task_pending(*task));

        let mut next = e.stored.clone();
        next.next_due = decision.next_due;
        next.done = decision.next_due.is_none();
        if !decision.fire.is_empty() && !overlap {
            next.last_run = Some(now);
            next.unfinished.extend(decision.fire.iter().copied());
        }
        // Persist first (at-most-once). If that fails nothing has happened and the next tick retries.
        // A module's finished one-time schedule is deleted instead of kept as `done` once its task
        // has ended (`forget`): nothing re-declares it, so there's nothing to remember (ADR 0035 §5.6).
        // "skipped" when nothing was enqueued this tick; an overlap drops the firing (#251's review).
        let gone = matches!(next.source, Source::Added { .. }) && next.done && next.unfinished.is_empty();
        let persisted = if gone {
            self.delete(
                next.spec.id.as_str(),
                ("scheduler.trigger.removed", json!({"trigger_id": next.spec.id, "reason": "skipped"})),
            )
        } else {
            self.write(&next, None)
        };
        if let Err(err) = persisted {
            tracing::error!(trigger = %e.stored.spec.id, error = %err, "could not persist trigger state; will retry");
            return true;
        }
        e.stored = next;

        let spec = e.stored.spec.clone();
        let id = spec.id.to_string();
        if decision.missed > 0 {
            self.emit(
                "scheduler.trigger.missed",
                json!({"trigger_id": id, "count": decision.missed, "catch_up": spec.catch_up}),
            );
        }
        if overlap {
            let pending: Vec<TaskId> = e.pending.iter().map(|(task, _)| *task).collect();
            self.emit(
                "scheduler.trigger.skipped",
                json!({"trigger_id": id, "reason": "overlap", "dropped": decision.fire.len(), "pending": pending}),
            );
            return !gone;
        }
        let mut failed = Vec::new();
        for scheduled_for in decision.fire {
            if !self.enqueue(e, &spec, scheduled_for, now, false) {
                failed.push(scheduled_for);
            }
        }
        if !failed.is_empty() {
            return self.forget(e, &failed);
        }
        !gone
    }

    /// Hand one firing to the queue. `after_restart`: queued again on start, never having run.
    /// False when the queue refused it (reported as skipped).
    fn enqueue(
        &self,
        e: &mut Entry,
        spec: &TriggerSpec,
        scheduled_for: DateTime<Utc>,
        now: DateTime<Utc>,
        after_restart: bool,
    ) -> bool {
        let id = spec.id.to_string();
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
                e.pending.push((h.task_id, scheduled_for));
                let mut payload = json!({"trigger_id": id, "op": spec.op, "task_id": h.task_id, "lane": h.lane,
                       "scheduled_for": scheduled_for, "late_by_s": (now - scheduled_for).num_seconds()});
                if after_restart {
                    payload["after_restart"] = json!(true);
                }
                self.emit("scheduler.trigger.fired", payload);
                true
            }
            Err(err) => {
                self.emit(
                    "scheduler.trigger.skipped",
                    json!({"trigger_id": id, "reason": "enqueue_failed", "error": err.message, "code": err.code}),
                );
                false
            }
        }
    }

    /// On start, after `load`: settle every firing a stop left unfinished, from the event log
    /// (`events_since(topic pattern, since)`, oldest first). A task that never started is queued
    /// again; one that started and never ended is not run twice but reported
    /// (`scheduler.trigger.interrupted`) and shown on the trigger; an ended one is forgotten
    /// (#263 finding 1, ADR 0004 amendment).
    pub fn recover(&self, events_since: impl Fn(&str, DateTime<Utc>) -> Result<Vec<Event>>) -> Result<()> {
        let now = self.clock.now();
        let mut st = self.lock();
        let Some(oldest) = st.values().flat_map(|e| e.stored.unfinished.iter()).min().copied() else { return Ok(()) };
        // A firing is logged after the time it was for; a minute's margin for a clock that moved.
        let since = oldest - chrono::Duration::minutes(1);
        // The task each firing was handed to: the latest, if an earlier restart queued it again.
        let mut task_for: HashMap<(String, DateTime<Utc>), TaskId> = HashMap::new();
        for ev in events_since("scheduler.trigger.fired", since)? {
            let p = &ev.payload;
            let trigger = p["trigger_id"].as_str().map(str::to_owned);
            let at = serde_json::from_value::<DateTime<Utc>>(p["scheduled_for"].clone()).ok();
            let task = serde_json::from_value::<TaskId>(p["task_id"].clone()).ok();
            if let (Some(trigger), Some(at), Some(task)) = (trigger, at, task) {
                task_for.insert((trigger, at), task);
            }
        }
        let (mut started, mut ended) = (HashSet::new(), HashSet::new());
        for ev in events_since("queue.task.*", since)? {
            let Ok(task) = serde_json::from_value::<TaskId>(ev.payload["task_id"].clone()) else { continue };
            match ev.topic.as_str() {
                "queue.task.started" => {
                    started.insert(task);
                }
                "queue.task.finished" | "queue.task.failed" | "queue.task.cancelled" => {
                    ended.insert(task);
                }
                _ => {}
            }
        }
        let mut gone = Vec::new();
        for (id, e) in st.iter_mut() {
            if e.stored.unfinished.is_empty() {
                continue;
            }
            let (mut settled, mut rerun, mut cut) = (Vec::new(), Vec::new(), None);
            for at in e.stored.unfinished.clone() {
                match task_for.get(&(id.clone(), at)) {
                    Some(task) if ended.contains(task) => settled.push(at),
                    Some(task) if started.contains(task) => {
                        settled.push(at);
                        cut = Some(Interrupted { task_id: *task, scheduled_for: at, noticed: now });
                    }
                    // Never queued, or queued and never started: it never ran.
                    _ => rerun.push(at),
                }
            }
            if let Some(cut) = &cut {
                let mut next = e.stored.clone();
                next.interrupted = Some(cut.clone());
                if let Err(err) = self.write(&next, None) {
                    tracing::error!(trigger = %id, error = %err, "could not record an interrupted firing");
                } else {
                    e.stored = next;
                }
                self.emit(
                    "scheduler.trigger.interrupted",
                    json!({"trigger_id": id, "op": e.stored.spec.op, "task_id": cut.task_id,
                           "scheduled_for": cut.scheduled_for}),
                );
            }
            if !settled.is_empty() && !self.forget(e, &settled) {
                gone.push(id.clone());
                continue;
            }
            let spec = e.stored.spec.clone();
            let mut failed = Vec::new();
            for at in rerun {
                if !self.enqueue(e, &spec, at, now, true) {
                    failed.push(at);
                }
            }
            if !failed.is_empty() && !self.forget(e, &failed) {
                gone.push(id.clone());
            }
        }
        for id in gone {
            st.remove(&id);
        }
        Ok(())
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
            "scheduler.list" => self.list(params),
            "scheduler.show" => self.get(&self.resolve(&trigger_id(params)?)?),
            "scheduler.add" => self.add(params),
            "scheduler.edit" => self.edit(params),
            "scheduler.remove" => {
                let written = trigger_id(params)?;
                if self.broken().contains_key(&written) {
                    return self.remove_broken(&written);
                }
                self.remove(&self.resolve(&written)?)
            }
            "scheduler.disable" => self.set_enabled(&self.resolve(&trigger_id(params)?)?, false),
            "scheduler.enable" => self.set_enabled(&self.resolve(&trigger_id(params)?)?, true),
            "scheduler.restore" => self.restore(&trigger_id(params)?),
            "scheduler.purge" => self.purge(params),
            _ => Err(Error::unknown_op(format!("no such scheduler op '{op}'"))),
        }
    }

    /// The live triggers, or with `{"removed": true}` the removed ones (ADR 0038 §3).
    fn list(&self, params: Value) -> Result<Value> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct P {
            removed: bool,
        }
        let p: P = if params.is_null() {
            P::default()
        } else {
            serde_json::from_value(params).map_err(|e| Error::invalid_params(e.to_string()))?
        };
        if p.removed {
            let triggers: Vec<Value> = self.removed()?.iter().map(trigger_view).collect();
            return Ok(json!({ "triggers": triggers }));
        }
        let triggers: Vec<Value> = self.lock().values().map(|e| trigger_view(&e.stored)).collect();
        // Files that couldn't be read: here so a schedule never silently stops existing.
        let broken: Vec<Value> =
            self.broken().iter().map(|(id, (file, why))| json!({"id": id, "file": file, "problem": why})).collect();
        Ok(json!({ "triggers": triggers, "broken": broken }))
    }

    /// Every removed trigger, by id. One that can't be read is skipped and logged, as `load` does.
    fn removed(&self) -> Result<Vec<Stored>> {
        let mut out = Vec::new();
        for file in self.store.list(REMOVED)? {
            let Some(text) = self.store.read_string(&file)? else { continue };
            match serde_json::from_str::<Stored>(&text) {
                Ok(s) => out.push(s),
                Err(e) => tracing::error!(file, error = %e, "ignoring unreadable removed trigger"),
            }
        }
        Ok(out)
    }

    /// A removed trigger's id as a client wrote it, like `resolve` over the live ones.
    fn resolve_removed(&self, written: &str, removed: &[Stored]) -> Result<String> {
        let ids: Vec<String> = removed.iter().map(|s| s.spec.id.to_string()).collect();
        if ids.iter().any(|id| id == written) {
            return Ok(written.to_owned());
        }
        shimmer_core::short_id::resolve(written, &ids, "removed trigger")
    }

    /// A trigger id as a client wrote it: the full id, or a unique ending of one, case insensitive
    /// (ADR 0031 §4). Several matches are `conflict`, none `not_found`.
    fn resolve(&self, written: &str) -> Result<String> {
        let ids: Vec<String> = self.lock().keys().cloned().collect();
        if ids.iter().any(|id| id == written) {
            return Ok(written.to_owned());
        }
        shimmer_core::short_id::resolve(written, &ids, "trigger")
    }

    /// One trigger, in exactly the shape a `scheduler.list` entry has (#76) — `queue show`
    /// uses `queue.show` and `workspaces show` uses `workspaces.show`; this was the odd
    /// one out, picking its one trigger out of the full list instead.
    fn get(&self, id: &str) -> Result<Value> {
        let st = self.lock();
        let e = st.get(id).ok_or_else(|| Error::not_found(format!("no trigger '{id}'")))?;
        Ok(trigger_view(&e.stored))
    }

    fn add(&self, params: Value) -> Result<Value> {
        let id = TriggerId::new(format!("usr-{}", ulid::Ulid::new().to_string().to_lowercase()));
        let spec = spec_from(params, id)?;
        self.check_target(&spec)?;
        self.refuse_past_skip(&spec)?;
        let parsed = Parsed::parse(&spec.schedule, self.local_tz)?;
        let stored = Stored {
            next_due: parsed.first_due(self.clock.now()),
            spec,
            source: Source::User,
            paused: false,
            last_run: None,
            done: false,
            zone: parsed.is_local().then(|| self.local_tz.name().to_owned()),
            unfinished: Vec::new(),
            interrupted: None,
        };
        self.write(&stored, Some(("scheduler.trigger.added", added_payload(&stored))))?;
        let id = stored.spec.id.clone();
        self.lock().insert(id.to_string(), Entry { stored, parsed, pending: Vec::new() });
        Ok(json!({ "trigger_id": id }))
    }

    /// Replace a person's schedule with a new one, keeping its id, whether it's on, and when it
    /// last ran (ADR 0038 §6). Before, changing a time meant remove and add: a new id, and a
    /// removed copy left behind. It counts from now, like `enable`.
    fn edit(&self, mut params: Value) -> Result<Value> {
        let written = match params.as_object_mut().and_then(|m| m.remove("trigger_id")) {
            Some(Value::String(id)) => id,
            _ => return Err(Error::invalid_params("missing field `trigger_id`")),
        };
        let id = self.resolve(&written)?;
        let spec = spec_from(params, TriggerId::new(id.as_str()))?;
        self.check_target(&spec)?;
        self.refuse_past_skip(&spec)?;
        let parsed = Parsed::parse(&spec.schedule, self.local_tz)?;
        let mut st = self.lock();
        let e = st.get_mut(&id).ok_or_else(|| Error::not_found(format!("no trigger '{id}'")))?;
        if let Source::Module { id: owner } = &e.stored.source {
            return Err(Error::conflict(format!("trigger '{id}' is declared by module '{owner}'; it can't be edited")));
        }
        let mut next = e.stored.clone();
        next.spec = spec;
        next.next_due = parsed.first_due(self.clock.now());
        next.done = false;
        next.zone = parsed.is_local().then(|| self.local_tz.name().to_owned());
        self.write(&next, Some(("scheduler.trigger.edited", added_payload(&next))))?;
        e.stored = next;
        e.parsed = parsed;
        Ok(trigger_view(&e.stored))
    }

    fn remove(&self, id: &str) -> Result<Value> {
        let mut st = self.lock();
        let e = st.get(id).ok_or_else(|| Error::not_found(format!("no trigger '{id}'")))?;
        match &e.stored.source {
            Source::Module { id: owner } => {
                return Err(Error::conflict(format!(
                    "trigger '{id}' is declared by module '{owner}'; disable it instead"
                )));
            }
            // A module's one-time schedule (an automation's delayed action) is the person's to
            // cancel (ADR 0035 §4), like their own: moved aside below for `restore` (ADR 0038 §3),
            // and out of the live set, so it no longer takes a slot in the module's quota (#251's
            // review).
            Source::Added { .. } | Source::User => {}
        }
        // Moved aside, not deleted: `scheduler.restore` brings it back (ADR 0038 §3). Only the
        // latest removal of an id is kept, like records and workspaces.
        let json = serde_json::to_vec_pretty(&e.stored).map_err(|e| Error::internal(format!("encode trigger: {e}")))?;
        self.store.transaction(|tx| {
            tx.put(&removed_path(id), json)?;
            tx.delete(&path(id))?;
            tx.emit("scheduler.trigger.removed", json!({"trigger_id": id, "reason": "removed by user"}))
        })?;
        st.remove(id);
        Ok(json!({"removed": true, "trigger_id": id}))
    }

    /// A schedule file that can't be read is deleted, not moved aside: there's no schedule in it
    /// to restore (#263 finding 5). Only by its full id, as `scheduler list` shows it.
    fn remove_broken(&self, id: &str) -> Result<Value> {
        let _st = self.lock();
        self.store.transaction(|tx| {
            tx.delete(&path(id))?;
            tx.emit("scheduler.trigger.removed", json!({"trigger_id": id, "reason": "unreadable file removed"}))
        })?;
        self.broken().remove(id);
        Ok(json!({"removed": true, "trigger_id": id}))
    }

    /// Bring a removed trigger back as it was, paused or not (ADR 0038 §3). Like `resume`, it
    /// counts from now: a recurring trigger's next run is its next time after now, and time spent
    /// removed is never caught up. A `once` keeps its time; if that has passed, its own `catch_up`
    /// decides on the next tick.
    fn restore(&self, written: &str) -> Result<Value> {
        let removed = self.removed()?;
        let id = self.resolve_removed(written, &removed)?;
        let mut stored = removed.into_iter().find(|s| s.spec.id.as_str() == id).expect("resolved from this list");
        let mut st = self.lock();
        if st.contains_key(&id) {
            return Err(Error::conflict(format!("a trigger '{id}' exists again; remove it first")));
        }
        // What it runs may have gone while it was removed (a module uninstalled).
        self.check_target(&stored.spec)?;
        let parsed = Parsed::parse(&stored.spec.schedule, self.local_tz)?;
        if !stored.done && !matches!(stored.spec.schedule, Schedule::Once(_)) {
            stored.next_due = parsed.next_after(self.clock.now());
        }
        stored.zone = parsed.is_local().then(|| self.local_tz.name().to_owned());
        // Firings from before it was removed aren't this schedule's any more: it counts from now.
        stored.unfinished.clear();
        let json = serde_json::to_vec_pretty(&stored).map_err(|e| Error::internal(format!("encode trigger: {e}")))?;
        self.store.transaction(|tx| {
            tx.put(&path(&id), json)?;
            tx.delete(&removed_path(&id))?;
            tx.emit("scheduler.trigger.restored", json!({"trigger_id": id}))
        })?;
        let view = trigger_view(&stored);
        st.insert(id, Entry { stored, parsed, pending: Vec::new() });
        Ok(view)
    }

    /// Delete removed triggers for good: one, or every one, after the caller confirms how many it
    /// was shown, as `records.purge` asks (ADR 0038 §1).
    fn purge(&self, params: Value) -> Result<Value> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct P {
            trigger_id: Option<String>,
            confirm: Option<Value>,
        }
        let p: P = if params.is_null() {
            P::default()
        } else {
            serde_json::from_value(params).map_err(|e| Error::invalid_params(e.to_string()))?
        };
        // Held throughout, so a `remove` can't land between the count and the delete.
        let _st = self.lock();
        let removed = self.removed()?;
        let one = p.trigger_id.as_deref().map(|w| self.resolve_removed(w, &removed)).transpose()?;
        let ids: Vec<String> = match &one {
            Some(id) => vec![id.clone()],
            None => removed.iter().map(|s| s.spec.id.to_string()).collect(),
        };
        let triggers = ids.len();
        if triggers == 0 {
            return Ok(json!({"purged": 0}));
        }
        let expected = json!({"triggers": triggers});
        if p.confirm.as_ref() != Some(&expected) {
            let what = match &one {
                Some(id) => format!("the removed trigger '{id}'"),
                None => format!("all {triggers} removed trigger(s)"),
            };
            return Err(Error::new(
                shimmer_core::ErrorCode::ConfirmationRequired,
                format!("purging permanently deletes {what}; confirm with {{\"triggers\": {triggers}}}"),
            )
            .with_detail(expected));
        }
        let mut event = json!({"triggers": triggers});
        if let Some(id) = &one {
            event["trigger_id"] = json!(id);
        }
        self.store.transaction(|tx| {
            for id in &ids {
                tx.delete(&removed_path(id))?;
            }
            tx.emit("scheduler.trigger.purged", event)
        })?;
        Ok(json!({"purged": triggers}))
    }

    /// `scheduler.enable` / `disable` (ADR 0038 §6). On disk a trigger keeps its `paused` flag;
    /// on the wire it's `enabled`, as automations' rules are.
    fn set_enabled(&self, id: &str, enabled: bool) -> Result<Value> {
        let paused = !enabled;
        let mut st = self.lock();
        let e = st.get_mut(id).ok_or_else(|| Error::not_found(format!("no trigger '{id}'")))?;
        if e.stored.paused == paused {
            return Ok(json!({ "enabled": enabled, "trigger_id": id }));
        }
        let mut next = e.stored.clone();
        next.paused = paused;
        if !paused && !next.done && !matches!(next.spec.schedule, Schedule::Once(_)) {
            // Time spent off is not "missed": enabling counts from now instead of replaying it.
            next.next_due = e.parsed.next_after(self.clock.now());
        }
        let topic = if paused { "scheduler.trigger.disabled" } else { "scheduler.trigger.enabled" };
        self.write(&next, Some((topic, json!({"trigger_id": id}))))?;
        e.stored = next;
        Ok(json!({ "enabled": enabled, "trigger_id": id }))
    }
}

/// `ctx.schedules` (ADR 0035 §5.6): one-time schedules a module adds and removes while running.
impl ScheduleBackend for Scheduler {
    fn add_once(&self, module: &ModuleId, once: OneTime) -> Result<TriggerId> {
        let label = once.label.trim();
        if label.is_empty() || label.len() > MAX_LABEL || label.contains('\n') {
            return Err(Error::invalid_params(format!("a schedule's label is one line of 1-{MAX_LABEL} characters")));
        }
        let spec = TriggerSpec {
            id: TriggerId::new(format!("{module}-{}", ulid::Ulid::new().to_string().to_lowercase())),
            schedule: Schedule::Once(once.at),
            // Still wanted if the machine was off then: run on wake, once.
            catch_up: CatchUp::RunOnce,
            op: once.op,
            params: once.params,
            lane: None,
            fallback: None,
        };
        self.check_target(&spec)?;
        let parsed = Parsed::parse(&spec.schedule, self.local_tz)?;
        let stored = Stored {
            next_due: parsed.first_due(self.clock.now()),
            spec,
            source: Source::Added { id: module.clone(), label: label.to_owned() },
            paused: false,
            last_run: None,
            done: false,
            zone: None,
            unfinished: Vec::new(),
            interrupted: None,
        };
        // The cap is checked and the schedule added under one lock, so two calls at once can't
        // both pass it (#251's review).
        let mut st = self.lock();
        let mine = st.values().filter(|e| matches!(&e.stored.source, Source::Added { id, .. } if id == module)).count();
        if mine >= MAX_ADDED {
            return Err(Error::conflict(format!("module '{module}' already has {MAX_ADDED} schedules waiting")));
        }
        self.write(&stored, Some(("scheduler.trigger.added", added_payload(&stored))))?;
        let id = stored.spec.id.clone();
        st.insert(id.to_string(), Entry { stored, parsed, pending: Vec::new() });
        Ok(id)
    }

    fn remove(&self, module: &ModuleId, id: &TriggerId) -> Result<bool> {
        let mut st = self.lock();
        // Another module's, or a person's, is "not there" as far as this module can tell.
        let Some(e) = st.get(id.as_str()) else { return Ok(false) };
        if !matches!(&e.stored.source, Source::Added { id: owner, .. } if owner == module) {
            return Ok(false);
        }
        self.delete(
            id.as_str(),
            ("scheduler.trigger.removed", json!({"trigger_id": id, "reason": "removed by module"})),
        )?;
        st.remove(id.as_str());
        Ok(true)
    }
}

/// The wire shape of one trigger, shared by `scheduler.list` (one per entry) and
/// `scheduler.show` (one, by id) so they can never drift apart.
fn trigger_view(s: &Stored) -> Value {
    json!({"id": s.spec.id, "source": s.source, "schedule": s.spec.schedule, "op": s.spec.op,
           "params": s.spec.params, "catch_up": s.spec.catch_up, "lane": s.spec.lane,
           "fallback": s.spec.fallback, "next_due": s.next_due, "last_run": s.last_run,
           "enabled": !s.paused, "done": s.done, "waiting": s.unfinished.len(), "interrupted": s.interrupted})
}

/// `scheduler.add`'s and `scheduler.edit`'s schedule, read the same way. `catch_up` has no
/// default: guessing wrong sends duplicate emails (§6.3).
fn spec_from(params: Value, id: TriggerId) -> Result<TriggerSpec> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Spec {
        schedule: Schedule,
        op: String,
        #[serde(default)]
        params: Value,
        catch_up: CatchUp,
        lane: Option<LaneId>,
        fallback: Option<Fallback>,
    }
    let s: Spec = serde_json::from_value(params).map_err(|e| Error::invalid_params(e.to_string()))?;
    Ok(TriggerSpec {
        id,
        schedule: s.schedule,
        catch_up: s.catch_up,
        op: s.op,
        params: s.params,
        lane: s.lane,
        fallback: s.fallback,
    })
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
