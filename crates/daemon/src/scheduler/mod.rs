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
//! * A person's removed schedule waits in `removed/<id>.json` until restored or purged
//!   (ADR 0038). It is never loaded, so it never fires.

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
/// Removed user schedules, until `scheduler.restore` or `scheduler.purge` (ADR 0038).
const REMOVED: &str = "removed";

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
            "scheduler.list" => self.list(params),
            "scheduler.show" => self.get(&self.resolve(&trigger_id(params)?)?),
            "scheduler.add" => self.add(params),
            "scheduler.edit" => self.edit(params),
            "scheduler.remove" => self.remove(&self.resolve(&trigger_id(params)?)?),
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
        let triggers: Vec<Value> = if p.removed {
            self.removed()?.iter().map(trigger_view).collect()
        } else {
            self.lock().values().map(|e| trigger_view(&e.stored)).collect()
        };
        Ok(json!({ "triggers": triggers }))
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
        if let Source::Module { id: owner } = &e.stored.source {
            return Err(Error::conflict(format!("trigger '{id}' is declared by module '{owner}'; disable it instead")));
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

/// The wire shape of one trigger, shared by `scheduler.list` (one per entry) and
/// `scheduler.show` (one, by id) so they can never drift apart.
fn trigger_view(s: &Stored) -> Value {
    json!({"id": s.spec.id, "source": s.source, "schedule": s.spec.schedule, "op": s.spec.op,
           "params": s.spec.params, "catch_up": s.spec.catch_up, "lane": s.spec.lane,
           "fallback": s.spec.fallback, "next_due": s.next_due, "last_run": s.last_run,
           "enabled": !s.paused, "done": s.done})
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
