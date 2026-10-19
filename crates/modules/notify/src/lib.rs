//! The `notify` module (ADR 0032): every message lands in an inbox first, then goes to each way
//! of reaching the user (a [`Sink`]). Quiet hours and pause hold back what isn't urgent until
//! they end, and two built-in alerts say when a workspace went dirty or a scheduled task failed.
//!
//! Notify decides *how* to reach you, never *when*: that's the caller's (a fallback, an alert, a
//! schedule, or later an automation, ADR 0035).
//!
//! Layout, inside the module's namespace `data/notify/`:
//! - `inbox/<id>.toml`: one message, the id a ULID (so names sort oldest first);
//! - `held/<id>`: an empty marker per held message, so `notify.flush` finds them without
//!   reading the inbox, written and removed in the same transaction as the message's state;
//! - `settings.toml`: cap, quiet hours, pause and the built-in alerts (§3), changed by the ops;
//! - `removed.toml`: how many unread messages the cap has removed.

mod alerts;
mod filter;
mod inbox;
mod message;
mod quiet;
mod settings;
mod sink;

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use shimmer_core::short_id;
use shimmer_core::store::Tx;
use shimmer_core::trigger::{CatchUp, Schedule, TriggerSpec};
use shimmer_core::{
    CommandSpec, Ctx, Error, ErrorCode, Event, Execution, LaneConfig, Manifest, Module, Result, TriggerId, WriteLock,
};

pub use alerts::Alerts;
pub use message::{Delivery, Message, Priority, State};
pub use sink::{DesktopSink, Sink};

use filter::Filter;
use message::{held_path, path, HELD, INBOX, REMOVED};
use settings::{Pause, Settings};

/// Its own lane, so a message is never stuck behind long fetches: fallbacks depend on it
/// (CLAUDE.md §6.1).
pub const LANE: &str = "notify";

pub struct Notify {
    sinks: Vec<Arc<dyn Sink>>,
    /// Serialises each read-change-write of notify's files. Never held across an `.await`.
    write: WriteLock,
    /// A `notify.flush` is already queued: one is enough.
    flush_queued: AtomicBool,
    /// Held while a flush runs: flushes go one at a time, so two never send the same summary
    /// (#276's review).
    flushing: tokio::sync::Mutex<()>,
    /// Ids in the order messages were sent, even several in one millisecond: the inbox and the
    /// summary read them in that order.
    ids: Mutex<ulid::Generator>,
}

impl Default for Notify {
    /// The MVP's one way of reaching you: desktop notifications (ADR 0032 §3). Phone, email and
    /// webhooks are one more sink each, later.
    fn default() -> Self {
        Self::with_sinks(vec![Arc::new(DesktopSink)])
    }
}

impl Notify {
    pub fn with_sinks(sinks: Vec<Arc<dyn Sink>>) -> Self {
        Self {
            sinks,
            write: WriteLock::default(),
            flush_queued: AtomicBool::new(false),
            flushing: tokio::sync::Mutex::new(()),
            ids: Mutex::new(ulid::Generator::new()),
        }
    }
}

// ---------------------------------------------------------------- reading state

fn tz(ctx: &Ctx) -> chrono_tz::Tz {
    ctx.local_tz.name().parse().unwrap_or(chrono_tz::UTC)
}

/// The settings in force, and what's wrong with the file, if anything (§3).
fn settings(ctx: &Ctx) -> Result<(Settings, Option<String>)> {
    Ok(settings::read(ctx.store.read_string(settings::FILE)?.as_deref()))
}

fn quiet_now(s: &Settings, ctx: &Ctx) -> bool {
    s.quiet.contains(ctx.clock.now().with_timezone(&tz(ctx)).naive_local())
}

fn pause_now<'a>(s: &'a Settings, ctx: &Ctx) -> Option<&'a Pause> {
    s.pause.as_ref().filter(|p| p.active(ctx.clock.now()))
}

/// Whether a message of `priority` waits now: low and normal in quiet hours or a pause, high
/// only in a pause `--all` (ADR 0032 §4).
fn holds(s: &Settings, ctx: &Ctx, priority: Priority) -> bool {
    let urgent = priority == Priority::High;
    (!urgent && quiet_now(s, ctx)) || pause_now(s, ctx).is_some_and(|p| p.all || !urgent)
}

fn read_message(ctx: &Ctx, id: &str) -> Result<Option<Message>> {
    let p = path(id);
    ctx.store.read_string(&p)?.map(|text| Message::decode(&text, &p)).transpose()
}

fn tx_message(tx: &Tx<'_>, id: &str) -> Result<Option<Message>> {
    let p = path(id);
    match tx.read(&p)? {
        Some(bytes) => Message::decode(&String::from_utf8_lossy(&bytes), &p).map(Some),
        None => Ok(None),
    }
}

/// The ids in the inbox, oldest first.
fn inbox_ids(ctx: &Ctx) -> Result<Vec<String>> {
    Ok(ctx
        .store
        .list(INBOX)?
        .into_iter()
        .filter_map(|p| p.strip_prefix("inbox/").and_then(|f| f.strip_suffix(".toml")).map(String::from))
        .collect())
}

/// Every message, oldest first. One that can't be read is skipped, never fatal to the rest.
/// What `shimmer doctor` shows for notify (ADR 0033, ADR 0038 §6): a settings file it can't use,
/// inbox files it can't read, messages no sink could deliver, and a pause left on. Writes nothing.
fn health(ctx: &Ctx) -> Result<Value> {
    let mut checks = Vec::new();
    let (s, problem) = settings(ctx)?;
    if let Some(problem) = problem {
        checks.push(json!({"status": "warn", "subject": settings::FILE,
            "message": format!("{} can't be used, so the defaults apply: {problem}", settings::FILE),
            "fix": {"summary": format!("fix data/notify/{} by hand", settings::FILE)}}));
    }
    let (mut messages, mut damaged) = (Vec::new(), Vec::new());
    for id in inbox_ids(ctx)? {
        match read_message(ctx, &id) {
            Ok(Some(m)) => messages.push(m),
            Ok(None) => {}
            Err(e) => damaged.push(e.message),
        }
    }
    for problem in damaged {
        checks.push(json!({"status": "warn", "message": problem,
            "fix": {"summary": "fix or delete the file in data/notify/inbox/"}}));
    }
    let failed = messages.iter().filter(|m| m.state == State::Failed).count();
    if failed > 0 {
        checks.push(json!({"status": "warn",
            "message": format!("{failed} message(s) couldn't be delivered by any way of notifying you; they're in the inbox"),
            "fix": {"summary": "send them again", "op": "notify.resend", "params": {"state": "failed"}}}));
    }
    if let Some(p) = pause_now(&s, ctx) {
        let until = p.until.map_or("until you resume".to_owned(), |t| format!("until {t}"));
        checks.push(json!({"status": "warn", "message": format!("notifications are paused {until}"),
            "fix": {"summary": "resume them", "op": "notify.resume", "params": {}}}));
    }
    if checks.is_empty() {
        checks.push(
            json!({"status": "ok", "message": format!("{} message(s) in the inbox, none failed", messages.len())}),
        );
    }
    Ok(json!({"checks": checks}))
}

fn all_messages(ctx: &Ctx) -> Result<Vec<Message>> {
    Ok(inbox_ids(ctx)?.iter().filter_map(|id| read_message(ctx, id).ok().flatten()).collect())
}

fn held_ids(ctx: &Ctx) -> Result<Vec<String>> {
    Ok(ctx.store.list(HELD)?.into_iter().filter_map(|p| p.strip_prefix("held/").map(String::from)).collect())
}

fn removed_unread(ctx: &Ctx) -> Result<i64> {
    let text = ctx.store.read_string(REMOVED)?;
    Ok(text
        .and_then(|t| toml::from_str::<toml::Table>(&t).ok())
        .and_then(|t| t.get("unread").and_then(toml::Value::as_integer))
        .unwrap_or(0))
}

fn json_of(m: &Message) -> Value {
    serde_json::to_value(m).expect("a message is always JSON")
}

fn counts(messages: &[Message]) -> Value {
    let n = |f: &dyn Fn(&Message) -> bool| messages.iter().filter(|m| f(m)).count();
    json!({
        "total": messages.len(),
        "unread": n(&|m| !m.read),
        "high": n(&|m| !m.read && m.priority == Priority::High),
        "held": n(&|m| m.state == State::Held),
        "failed": n(&|m| m.state == State::Failed),
    })
}

/// Which messages an op is about: `{"ids": […]}`, `{"all": true}`, or filters (ADR 0032 §6).
enum Selection {
    Ids(Vec<String>),
    All,
    Filter(Filter),
}

impl Selection {
    fn parse(params: &Value, ctx: &Ctx, op: &str) -> Result<Self> {
        let ids = params.get("ids").filter(|v| !v.is_null());
        let all = params["all"] == true;
        let filtered = Filter::named_in(params);
        match (ids, all, filtered) {
            (Some(ids), false, false) => {
                let ids = ids.as_array().ok_or_else(|| Error::invalid_params("ids is a list"))?;
                if ids.is_empty() {
                    return Err(Error::invalid_params("ids is empty: name at least one message"));
                }
                ids.iter()
                    .map(|v| v.as_str().map(String::from).ok_or_else(|| Error::invalid_params("ids are text")))
                    .collect::<Result<_>>()
                    .map(Self::Ids)
            }
            (None, true, false) => Ok(Self::All),
            (None, false, true) => Ok(Self::Filter(Filter::parse(params, &tz(ctx))?)),
            (None, false, false) => Err(Error::invalid_params(format!(
                "say which messages for {op}: {{\"ids\": [...]}}, {{\"all\": true}}, or filters such as {{\"read\": true}}"
            ))),
            _ => Err(Error::invalid_params(format!("{op} takes ids, all, or filters: one of them, not several"))),
        }
    }

    fn is_bulk(&self) -> bool {
        !matches!(self, Self::Ids(_))
    }

    /// The ids it means, oldest first. A named id may be a unique ending of one (ADR 0031 §4's
    /// short ids); one that doesn't exist is an error.
    fn resolve(&self, ctx: &Ctx) -> Result<Vec<String>> {
        match self {
            Self::Ids(written) => {
                let ids = inbox_ids(ctx)?;
                written.iter().map(|w| short_id::resolve(w, &ids, "message")).collect()
            }
            Self::All => inbox_ids(ctx),
            Self::Filter(f) => Ok(all_messages(ctx)?.into_iter().filter(|m| f.matches(m)).map(|m| m.id).collect()),
        }
    }
}

// ---------------------------------------------------------------- the module

#[async_trait]
impl Module for Notify {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "notify".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            namespace: "notify".into(),
            topics: [
                "notify.message.received",
                "notify.message.held",
                "notify.message.sent",
                "notify.message.exhausted",
                "notify.message.read",
                "notify.message.unread",
                "notify.message.removed",
                "notify.pause.started",
                "notify.pause.ended",
                "notify.settings.changed",
            ]
            .map(String::from)
            .to_vec(),
            // The real `ctx.desktop` (ADR 0032 §1); every other module's fails closed.
            capabilities: vec!["desktop".into()],
        }
    }

    /// Nothing to set up: settings are read when needed, and a broken settings file never stops
    /// notify (§3); `status` reports it.
    async fn init(&self, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    fn lanes(&self) -> Vec<LaneConfig> {
        vec![LaneConfig::new(LANE, 2)]
    }

    /// Hourly, so the summary of held messages goes out at most an hour after quiet hours (or a
    /// timed pause) end (ADR 0032 §4); with nothing held it returns at once and writes nothing.
    /// The id is `notify.flush`: the daemon doesn't namespace a module's trigger ids.
    fn triggers(&self) -> Vec<TriggerSpec> {
        vec![TriggerSpec {
            id: TriggerId::new("notify.flush"),
            schedule: Schedule::Every(Duration::from_secs(3600)),
            catch_up: CatchUp::Skip,
            op: "notify.flush".into(),
            params: json!({}),
            lane: None,
            fallback: None,
        }]
    }

    fn commands(&self) -> Vec<CommandSpec> {
        let queued = || Execution::Queued { lane: LANE.into() };
        let object = || json!({"type": "object"});
        [
            ("notify.send", "Send a message: to the inbox, then every way Shimmer can reach you", queued()),
            ("notify.test", "Send a test message to every way Shimmer can reach you", queued()),
            ("notify.resend", "Try messages again (e.g. the failed ones)", queued()),
            ("notify.flush", "Send what quiet hours or a pause held back, as one summary", queued()),
            ("notify.list", "The inbox, newest first", Execution::Inline),
            ("notify.count", "How many messages match", Execution::Inline),
            ("notify.show", "One message in full", Execution::Inline),
            ("notify.mark_read", "Mark messages read", Execution::Inline),
            ("notify.mark_unread", "Mark messages unread", Execution::Inline),
            ("notify.remove", "Delete messages", Execution::Inline),
            (
                "notify.pause",
                "Hold messages until resume (high priority still comes through unless all)",
                Execution::Inline,
            ),
            ("notify.resume", "Stop holding messages and send what waited", Execution::Inline),
            ("notify.quiet", "Show or set quiet hours", Execution::Inline),
            ("notify.builtin_alerts", "Show or switch the built-in alerts", Execution::Inline),
            ("notify.cap", "Show or set how many messages the inbox keeps", Execution::Inline),
            ("notify.status", "Notify at a glance: settings, pause, counts", Execution::Inline),
            ("notify.health", "Check notify for problems; changes nothing (ADR 0033)", Execution::Inline),
        ]
        .into_iter()
        .map(|(op, summary, execution)| CommandSpec {
            op: op.into(),
            summary: summary.into(),
            params_schema: object(),
            execution,
        })
        .collect()
    }

    async fn handle(&self, op: &str, params: Value, ctx: &Ctx) -> Result<Value> {
        match op {
            "notify.send" => self.send(&params, ctx).await,
            "notify.test" => self.test(ctx).await,
            "notify.resend" => self.resend(&params, ctx).await,
            "notify.flush" => self.flush(ctx).await,
            "notify.list" => list(&params, ctx),
            "notify.count" => count(&params, ctx),
            "notify.show" => show(&params, ctx),
            "notify.mark_read" => self.mark(&params, ctx, true),
            "notify.mark_unread" => self.mark(&params, ctx, false),
            "notify.remove" => self.remove(&params, ctx),
            "notify.pause" => self.pause(&params, ctx),
            "notify.resume" => self.resume(ctx).await,
            "notify.quiet" => self.quiet(&params, ctx),
            "notify.builtin_alerts" => self.builtin_alerts(&params, ctx),
            "notify.cap" => self.cap(&params, ctx),
            "notify.status" => self.status(ctx),
            "notify.health" => health(ctx),
            other => Err(Error::unknown_op(format!("notify has no op '{other}'"))),
        }
    }

    /// The built-in alerts, and one flush once quiet hours or a pause are over. Never reacts to
    /// notify's own topics; [`alerts::alert`] also skips notify's own tasks.
    async fn on_event(&self, ev: &Event, ctx: &Ctx) -> Result<()> {
        if ev.topic.starts_with("notify.") {
            return Ok(());
        }
        let (s, _) = settings(ctx)?;
        // Enqueued, never sent inline: a slow popup must not hold up the bus, and every send
        // shows in the queue (ADR 0032 §4).
        if let Some(params) = alerts::alert(ev, &s.alerts) {
            ctx.queue.enqueue("notify.send", params).await?;
        }
        if !holds(&s, ctx, Priority::Normal)
            && !self.flush_queued.load(Ordering::Acquire)
            && !held_ids(ctx)?.is_empty()
            && !self.flush_queued.swap(true, Ordering::AcqRel)
        {
            ctx.queue.enqueue("notify.flush", json!({})).await?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- sending

impl Notify {
    fn next_id(&self, now: DateTime<Utc>) -> Result<String> {
        let mut ids = self.ids.lock().unwrap_or_else(|e| e.into_inner());
        ids.generate_from_datetime(now.into())
            .map(|id| id.to_string())
            .map_err(|_| Error::internal("too many messages in one millisecond"))
    }

    /// `notify.send` (ADR 0032 §4): into the inbox, then held or delivered. The task succeeds
    /// even when every sink fails: the inbox has the message, as `failed`.
    async fn send(&self, params: &Value, ctx: &Ctx) -> Result<Value> {
        let draft = message::draft(params)?;
        let (s, _) = settings(ctx)?;
        let now = ctx.clock.now();
        let held = holds(&s, ctx, draft.priority);
        let msg = Message {
            id: self.next_id(now)?,
            at: now,
            priority: draft.priority,
            title: draft.title,
            body: draft.body,
            source: draft.source,
            state: if held { State::Held } else { State::Pending },
            read: false,
            deliveries: Vec::new(),
        };
        {
            let _g = self.write.lock();
            let older = inbox_ids(ctx)?;
            ctx.store.transaction(|tx| {
                tx.put(&path(&msg.id), msg.encode())?;
                let removed_unread = cap_inbox(tx, older, Some(&msg.id), s.cap)?;
                let mut payload = json!({"id": msg.id, "title": msg.title, "body": msg.body,
                                         "priority": msg.priority, "source": msg.source});
                if removed_unread > 0 {
                    payload["removed_unread"] = json!(removed_unread);
                }
                tx.emit("notify.message.received", payload)?;
                if held {
                    tx.put(&held_path(&msg.id), "")?;
                    tx.emit("notify.message.held", json!({"id": msg.id}))?;
                }
                Ok(())
            })?;
        }
        if held {
            return Ok(json_of(&msg));
        }
        let deliveries = self.deliver(&msg, ctx).await;
        let done = self.finish(ctx, std::slice::from_ref(&msg.id), &deliveries)?;
        Ok(done.first().map(json_of).unwrap_or_else(|| json_of(&msg)))
    }

    /// `notify.test`: to every sink, past quiet hours and pause. Not kept in the inbox.
    async fn test(&self, ctx: &Ctx) -> Result<Value> {
        let msg = Message {
            id: self.next_id(ctx.clock.now())?,
            at: ctx.clock.now(),
            priority: Priority::Normal,
            title: "Shimmer can reach you".into(),
            body: "A test from 'shimmer notify test'.".into(),
            source: "send".into(),
            state: State::Pending,
            read: false,
            deliveries: Vec::new(),
        };
        let deliveries = self.deliver(&msg, ctx).await;
        Ok(json!({"deliveries": deliveries, "sinks": self.sinks.iter().map(|s| s.id()).collect::<Vec<_>>()}))
    }

    /// `notify.resend`: each message tried again, the attempt added to it, its state updated.
    async fn resend(&self, params: &Value, ctx: &Ctx) -> Result<Value> {
        let ids = Selection::parse(params, ctx, "resend")?.resolve(ctx)?;
        let mut items = Vec::new();
        for id in ids {
            let Some(m) = read_message(ctx, &id)? else { continue };
            let deliveries = self.deliver(&m, ctx).await;
            items.extend(self.finish(ctx, &[id], &deliveries)?.iter().map(json_of));
        }
        Ok(json!({"items": items}))
    }

    /// `notify.flush`: once quiet hours or a pause are over, what they held goes out as one
    /// summary. A pause that reached its end is cleared here.
    async fn flush(&self, ctx: &Ctx) -> Result<Value> {
        // One at a time: a second flush (another `on_event`, or the hourly trigger) waits for the
        // first to finish and clear its held markers, then finds only what's left (#276's review).
        let _one = self.flushing.lock().await;
        self.flush_queued.store(false, Ordering::Release);
        self.clear_ended_pause(ctx)?;
        let (s, _) = settings(ctx)?;
        if holds(&s, ctx, Priority::Normal) {
            return Ok(json!({"delivered": 0}));
        }
        let mut held = Vec::new();
        for id in held_ids(ctx)? {
            match read_message(ctx, &id)? {
                Some(m) => held.push(m),
                // Its message is gone (removed, or the cap): only the marker is left.
                None => {
                    let _g = self.write.lock();
                    ctx.store.delete(&held_path(&id))?;
                }
            }
        }
        let Some(summary) = summary(&held, ctx) else { return Ok(json!({"delivered": 0})) };
        let deliveries = self.deliver(&summary, ctx).await;
        let ids: Vec<String> = held.iter().map(|m| m.id.clone()).collect();
        let done = self.finish(ctx, &ids, &deliveries)?;
        Ok(json!({"delivered": done.iter().filter(|m| m.state == State::Delivered).count()}))
    }

    /// To every sink that isn't only a fallback; a failed one's fallback is tried once.
    async fn deliver(&self, msg: &Message, ctx: &Ctx) -> Vec<Delivery> {
        let fallbacks: HashSet<&str> = self.sinks.iter().filter_map(|s| s.fallback()).collect();
        let mut out = Vec::new();
        for sink in self.sinks.iter().filter(|s| !fallbacks.contains(s.id())) {
            if !attempt(sink.as_ref(), msg, ctx, &mut out).await {
                if let Some(other) = sink.fallback().and_then(|id| self.sinks.iter().find(|s| s.id() == id)) {
                    attempt(other.as_ref(), msg, ctx, &mut out).await;
                }
            }
        }
        out
    }

    /// Records `deliveries` on each message: `delivered` if any worked, else `failed`.
    fn finish(&self, ctx: &Ctx, ids: &[String], deliveries: &[Delivery]) -> Result<Vec<Message>> {
        let _g = self.write.lock();
        ctx.store.transaction(|tx| {
            let mut done = Vec::new();
            for id in ids {
                // Removed in the meantime: nothing left to update.
                let Some(mut m) = tx_message(tx, id)? else { continue };
                m.deliveries.extend(deliveries.iter().cloned());
                let worked: Vec<&str> = deliveries.iter().filter(|d| d.ok).map(|d| d.medium.as_str()).collect();
                if worked.is_empty() {
                    m.state = State::Failed;
                    let errors: Vec<Value> =
                        deliveries.iter().map(|d| json!({"medium": d.medium, "error": d.error})).collect();
                    tx.emit("notify.message.exhausted", json!({"id": m.id, "errors": errors}))?;
                } else {
                    m.state = State::Delivered;
                    tx.emit("notify.message.sent", json!({"id": m.id, "mediums": worked}))?;
                }
                tx.put(&path(id), m.encode())?;
                if tx.read(&held_path(id))?.is_some() {
                    tx.delete(&held_path(id))?;
                }
                done.push(m);
            }
            Ok(done)
        })
    }
}

async fn attempt(sink: &dyn Sink, msg: &Message, ctx: &Ctx, out: &mut Vec<Delivery>) -> bool {
    let result = sink.deliver(msg, ctx).await;
    let ok = result.is_ok();
    out.push(Delivery {
        medium: sink.id().to_owned(),
        at: ctx.clock.now(),
        ok,
        error: result.err().map(|e| e.message),
    });
    ok
}

/// What a flush sends: the one held message itself, or one note naming them.
fn summary(held: &[Message], ctx: &Ctx) -> Option<Message> {
    let first = held.first()?;
    if held.len() == 1 {
        return Some(first.clone());
    }
    let titles: Vec<&str> = held.iter().take(3).map(|m| m.title.as_str()).collect();
    let more = held.len().saturating_sub(3);
    let body = if more > 0 { format!("{} and {more} more", titles.join("; ")) } else { titles.join("; ") };
    Some(Message {
        title: format!("{} messages while you were away", held.len()),
        body,
        priority: held.iter().map(|m| m.priority).max().unwrap_or_default(),
        at: ctx.clock.now(),
        deliveries: Vec::new(),
        ..first.clone()
    })
}

/// Keeps the inbox to `max` (ADR 0032 §3) in the transaction that adds `new` (or sets the cap):
/// removes what [`inbox::prune`] says, with their held markers. Returns how many were unread.
fn cap_inbox(tx: &mut Tx<'_>, older: Vec<String>, new: Option<&str>, max: usize) -> Result<usize> {
    let total = older.len() + usize::from(new.is_some());
    if total <= max {
        return Ok(0);
    }
    let mut entries = Vec::with_capacity(total);
    for id in older {
        // A message that can't be read (a damaged file, or an I/O error reading it) is left alone
        // and not counted, as `list` skips it: one bad file must never make every later send fail
        // (#276's review).
        if let Ok(m) = tx_message(tx, &id) {
            entries.push((id, m.is_some_and(|m| m.read)));
        }
    }
    if let Some(new) = new {
        entries.push((new.to_owned(), false));
    }
    let (remove, unread) = inbox::prune(&entries, max);
    for id in &remove {
        tx.delete(&path(id))?;
        if tx.read(&held_path(id))?.is_some() {
            tx.delete(&held_path(id))?;
        }
    }
    tx.emit("notify.message.removed", json!({"ids": remove, "reason": "cap"}))?;
    if unread > 0 {
        let before = tx
            .read(REMOVED)?
            .and_then(|b| toml::from_str::<toml::Table>(&String::from_utf8_lossy(&b)).ok())
            .and_then(|t| t.get("unread").and_then(toml::Value::as_integer))
            .unwrap_or(0);
        tx.put(REMOVED, format!("unread = {}\n", before + unread as i64))?;
    }
    Ok(unread)
}

// ---------------------------------------------------------------- the inbox

/// `notify.list`: filters, `limit` (default 50); newest first, with counts for the whole inbox.
fn list(params: &Value, ctx: &Ctx) -> Result<Value> {
    let filter = Filter::parse(params, &tz(ctx))?;
    let limit = match params.get("limit") {
        None | Some(Value::Null) => 50,
        Some(v) => {
            v.as_u64().filter(|n| *n > 0).ok_or_else(|| Error::invalid_params("limit is a number above 0"))? as usize
        }
    };
    let messages = all_messages(ctx)?;
    // `short` is unique across the whole inbox, not only this page, so it always works given back.
    let ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
    let items: Vec<Value> = messages
        .iter()
        .rev()
        .filter(|m| filter.matches(m))
        .take(limit)
        .map(|m| {
            let mut item = json_of(m);
            item["short"] = json!(short_id::ending(&m.id, &ids));
            item
        })
        .collect();
    let mut out = json!({"items": items, "counts": counts(&messages)});
    let removed = removed_unread(ctx)?;
    if removed > 0 {
        out["removed_unread"] = json!(removed);
    }
    Ok(out)
}

/// `notify.count`: how many match the filters, and the inbox's own counts.
fn count(params: &Value, ctx: &Ctx) -> Result<Value> {
    let filter = Filter::parse(params, &tz(ctx))?;
    let messages = all_messages(ctx)?;
    let mut out = counts(&messages);
    out["count"] = json!(messages.iter().filter(|m| filter.matches(m)).count());
    Ok(out)
}

/// `notify.show`: `{"id"}`, the whole id or a unique ending of one.
fn show(params: &Value, ctx: &Ctx) -> Result<Value> {
    let written = params
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::invalid_params("which message? {\"id\": ...}"))?;
    let id = short_id::resolve(written, &inbox_ids(ctx)?, "message")?;
    match read_message(ctx, &id)? {
        Some(m) => Ok(json_of(&m)),
        None => Err(Error::not_found(format!("no message '{written}'"))),
    }
}

impl Notify {
    /// `notify.mark_read` / `notify.mark_unread`: ids, all, or filters.
    fn mark(&self, params: &Value, ctx: &Ctx, read: bool) -> Result<Value> {
        let op = if read { "mark_read" } else { "mark_unread" };
        let selection = Selection::parse(params, ctx, op)?;
        let _g = self.write.lock();
        let ids = selection.resolve(ctx)?;
        let marked = ctx.store.transaction(|tx| {
            let mut marked = Vec::new();
            for id in &ids {
                let Some(mut m) = tx_message(tx, id)? else { continue };
                if m.read != read {
                    m.read = read;
                    tx.put(&path(id), m.encode())?;
                    marked.push(id.clone());
                }
            }
            if !marked.is_empty() {
                let topic = if read { "notify.message.read" } else { "notify.message.unread" };
                tx.emit(topic, json!({"ids": marked}))?;
            }
            Ok(marked.len())
        })?;
        Ok(json!({"marked": marked}))
    }

    /// `notify.remove`: named ids go at once; filters or all need `confirm`, and without it the
    /// answer is `confirmation_required` with what would go (as §6.2's promotion does).
    fn remove(&self, params: &Value, ctx: &Ctx) -> Result<Value> {
        let selection = Selection::parse(params, ctx, "remove")?;
        let _g = self.write.lock();
        let ids = selection.resolve(ctx)?;
        if selection.is_bulk() && params["confirm"] != true {
            let unread = ids.iter().filter_map(|id| read_message(ctx, id).ok().flatten()).filter(|m| !m.read).count();
            return Err(Error::new(
                ErrorCode::ConfirmationRequired,
                format!("this removes {} messages ({unread} unread); send again with \"confirm\": true", ids.len()),
            )
            .with_detail(json!({"count": ids.len(), "unread": unread})));
        }
        ctx.store.transaction(|tx| {
            for id in &ids {
                tx.delete(&path(id))?;
                if tx.read(&held_path(id))?.is_some() {
                    tx.delete(&held_path(id))?;
                }
            }
            if !ids.is_empty() {
                tx.emit("notify.message.removed", json!({"ids": ids, "reason": "removed"}))?;
            }
            Ok(())
        })?;
        Ok(json!({"removed": ids.len()}))
    }
}

// ---------------------------------------------------------------- settings

impl Notify {
    /// Changes the settings file under the write lock, with `f` saying which events to emit. A
    /// file that can't be read is never overwritten (§3).
    fn write_settings(
        &self,
        ctx: &Ctx,
        f: impl FnOnce(&mut settings::File) -> Result<Vec<(&'static str, Value)>>,
    ) -> Result<settings::File> {
        self.write_settings_with(ctx, |file, _| f(file))
    }

    /// [`write_settings`](Self::write_settings), where `f` can also stage other changes in the
    /// same transaction, so a setting and what it does land together or not at all (§7.1).
    fn write_settings_with(
        &self,
        ctx: &Ctx,
        f: impl FnOnce(&mut settings::File, &mut Tx<'_>) -> Result<Vec<(&'static str, Value)>>,
    ) -> Result<settings::File> {
        let _g = self.write.lock();
        let text = ctx.store.read_string(settings::FILE)?;
        let mut file = settings::editable(text.as_deref()).map_err(Error::invalid_params)?;
        ctx.store.transaction(|tx| {
            let events = f(&mut file, tx)?;
            tx.put(settings::FILE, settings::encode(&file))?;
            for (topic, payload) in events {
                tx.emit(topic, payload)?;
            }
            Ok(())
        })?;
        Ok(file)
    }

    /// `notify.pause`: `{"until"?, "all"?}`. `until` is a time (RFC 3339) after now; without it,
    /// until `resume`.
    fn pause(&self, params: &Value, ctx: &Ctx) -> Result<Value> {
        let now = ctx.clock.now();
        let until = match params.get("until") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let t = v
                    .as_str()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| t.with_timezone(&Utc))
                    .ok_or_else(|| {
                        Error::invalid_params(format!("until is a time like 2026-10-16T14:00:00Z, not {v}"))
                    })?;
                if t <= now {
                    return Err(Error::invalid_params("until is already past"));
                }
                Some(t)
            }
        };
        let pause = Pause { since: now, until, all: params["all"] == true };
        let started = json!({"until": until, "all": pause.all});
        self.write_settings(ctx, |file| {
            file.pause = Some(pause.clone());
            Ok(vec![("notify.pause.started", started)])
        })?;
        Ok(json!({"paused": pause}))
    }

    /// `notify.resume`: stop holding, and send what waited (one flush).
    async fn resume(&self, ctx: &Ctx) -> Result<Value> {
        let mut held = 0;
        let mut was_paused = false;
        self.write_settings(ctx, |file| {
            // Counted under the write lock that holds and releases messages, so the number is
            // exact (#276's review).
            held = held_ids(ctx)?.len();
            was_paused = file.pause.take().is_some();
            Ok(if was_paused { vec![("notify.pause.ended", json!({"held": held}))] } else { Vec::new() })
        })?;
        if held > 0 && !self.flush_queued.swap(true, Ordering::AcqRel) {
            ctx.queue.enqueue("notify.flush", json!({})).await?;
        }
        Ok(json!({"was_paused": was_paused, "held": held}))
    }

    /// A pause whose `until` has passed: removed, with `notify.pause.ended`.
    fn clear_ended_pause(&self, ctx: &Ctx) -> Result<()> {
        let (s, _) = settings(ctx)?;
        if s.pause.as_ref().is_none_or(|p| p.active(ctx.clock.now())) {
            return Ok(());
        }
        let held = held_ids(ctx)?.len();
        // A file that can't be rewritten keeps its ended pause: it no longer holds anything.
        let _ = self.write_settings(ctx, |file| {
            file.pause = None;
            Ok(vec![("notify.pause.ended", json!({"held": held}))])
        });
        Ok(())
    }

    /// `notify.quiet`: `{}` shows; `{"every_day"|"weekdays"|"weekends"|"mon"…: "22:00-08:00"}`
    /// sets those (`null` clears one); `{"off": true}` clears them all.
    fn quiet(&self, params: &Value, ctx: &Ctx) -> Result<Value> {
        let obj = params.as_object().cloned().unwrap_or_default();
        if obj.is_empty() {
            let (s, _) = settings(ctx)?;
            let text = ctx.store.read_string(settings::FILE)?;
            let quiet = settings::editable(text.as_deref()).map(|f| f.quiet).unwrap_or_default();
            return Ok(json!({"quiet": quiet, "quiet_now": quiet_now(&s, ctx)}));
        }
        let off = obj.get("off") == Some(&Value::Bool(true));
        let file = self.write_settings(ctx, |file| {
            if off {
                file.quiet.clear();
            }
            for (key, value) in obj.iter().filter(|(k, _)| k.as_str() != "off") {
                if !quiet::KEYS.contains(&key.as_str()) {
                    return Err(Error::invalid_params(format!(
                        "'{key}' isn't a day word: every_day, weekdays, weekends or mon…sun"
                    )));
                }
                match value {
                    Value::Null => {
                        file.quiet.remove(key);
                    }
                    Value::String(range) => {
                        file.quiet.insert(key.clone(), range.trim().to_owned());
                    }
                    other => {
                        return Err(Error::invalid_params(format!(
                            "{key} is a range like \"22:00-08:00\", not {other}"
                        )))
                    }
                }
            }
            quiet::Quiet::parse(&file.quiet).map_err(Error::invalid_params)?;
            Ok(vec![("notify.settings.changed", json!({"quiet": file.quiet}))])
        })?;
        let (s, _) = settings(ctx)?;
        Ok(json!({"quiet": file.quiet, "quiet_now": quiet_now(&s, ctx)}))
    }

    /// `notify.builtin_alerts`: `{}` shows; `{"name", "on"}` switches one.
    fn builtin_alerts(&self, params: &Value, ctx: &Ctx) -> Result<Value> {
        let show = |a: &Alerts| json!({"alerts": {"workspace_dirty": a.workspace_dirty, "scheduled_task_failed": a.scheduled_task_failed}});
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Ok(show(&settings(ctx)?.0.alerts));
        };
        let on =
            params.get("on").and_then(Value::as_bool).ok_or_else(|| Error::invalid_params("on is true or false"))?;
        let file = self.write_settings(ctx, |file| {
            let mut alerts = file.builtin_alerts.unwrap_or_default();
            alerts.set(name, on).map_err(Error::invalid_params)?;
            file.builtin_alerts = Some(alerts);
            Ok(vec![("notify.settings.changed", json!({"builtin_alerts": alerts}))])
        })?;
        Ok(show(&file.builtin_alerts.unwrap_or_default()))
    }

    /// `notify.cap`: `{}` shows; `{"max": N}` sets it (at least 100) and applies it at once.
    fn cap(&self, params: &Value, ctx: &Ctx) -> Result<Value> {
        let Some(max) = params.get("max").filter(|v| !v.is_null()) else {
            let (s, _) = settings(ctx)?;
            return Ok(json!({"max": s.cap, "messages": inbox_ids(ctx)?.len()}));
        };
        let max = max
            .as_u64()
            .map(|n| n as usize)
            .filter(|n| *n >= settings::MIN_CAP)
            .ok_or_else(|| Error::invalid_params(format!("max is a number, at least {}", settings::MIN_CAP)))?;
        // The new cap and the trimming it causes are one transaction (§7.1, #276's review).
        let mut removed_unread = 0;
        self.write_settings_with(ctx, |file, tx| {
            file.cap = Some(max);
            removed_unread = cap_inbox(tx, inbox_ids(ctx)?, None, max)?;
            Ok(vec![("notify.settings.changed", json!({"cap": max}))])
        })?;
        Ok(json!({"max": max, "messages": inbox_ids(ctx)?.len(), "removed_unread": removed_unread}))
    }

    /// `notify.status`: everything at a glance.
    fn status(&self, ctx: &Ctx) -> Result<Value> {
        let (s, problem) = settings(ctx)?;
        let text = ctx.store.read_string(settings::FILE)?;
        let quiet: BTreeMap<String, String> = settings::editable(text.as_deref()).map(|f| f.quiet).unwrap_or_default();
        Ok(json!({
            "paused": pause_now(&s, ctx),
            "quiet": quiet,
            "quiet_now": quiet_now(&s, ctx),
            "builtin_alerts": s.alerts,
            "cap": s.cap,
            "counts": counts(&all_messages(ctx)?),
            "removed_unread": removed_unread(ctx)?,
            "sinks": self.sinks.iter().map(|s| s.id()).collect::<Vec<_>>(),
            "settings_problem": problem,
        }))
    }
}
