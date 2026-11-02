//! Events to a module's `on_event`: every one, in order, however slow the module or bad the event.
//!
//! * **Lossless (#261 finding 1).** Each module listens on the bus, whose queue holds
//!   [`shimmer_proto::EVENT_QUEUE_CAPACITY`] events. One that falls further behind (a big
//!   `records import` in one transaction) used to lose the rest, silently. Now a lag replays the
//!   log from just before the last event the module was given, as the indexer does, so "the log
//!   has them all" holds for modules too. A short memory of what was given (by id) keeps the
//!   replay and the bus from giving an event twice: a rule firing twice is a duplicate email.
//!   A replay never reaches back before the module started listening: events from before that
//!   were the previous run's, already given to it then (#310's review). Each lag reads the day
//!   files from `since` on again; fine at today's sizes, a byte offset could replace it later.
//! * **A panic is one event's (#261 finding 2).** Each `on_event` runs in its own task, as each
//!   queued op does: a panic is logged with the module and topic, and the module keeps getting
//!   events, where before it got none until a restart.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use shimmer_core::{Ctx, Event, Module, ModuleId};
use shimmer_store::Store;
use tokio::sync::broadcast::{error::RecvError, Receiver};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use ulid::Ulid;

/// How far before the last given event a replay starts: two transactions committed at once can
/// reach the log and the bus in different orders (#261 finding 6).
const REPLAY_MARGIN: chrono::Duration = chrono::Duration::seconds(5);
/// Ids remembered, newest kept: more than a replay's margin and a full bus queue hold.
const REMEMBERED: usize = 16 * shimmer_proto::EVENT_QUEUE_CAPACITY;

/// The ids of the events a module was given, newest kept.
#[derive(Default)]
struct Given {
    order: VecDeque<Ulid>,
    set: HashSet<Ulid>,
}

impl Given {
    /// False when it was given already.
    fn first_time(&mut self, id: Ulid) -> bool {
        if !self.set.insert(id) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > REMEMBERED {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
}

/// `listening_since`: when the module was subscribed to the bus, taken just before subscribing.
/// `tracker`: the daemon's, so `Daemon::wait` also waits for an `on_event` still running.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    id: ModuleId,
    module: Arc<dyn Module>,
    ctx: Ctx,
    mut rx: Receiver<Event>,
    store: Arc<Store>,
    shutdown: CancellationToken,
    listening_since: DateTime<Utc>,
    tracker: TaskTracker,
) {
    let mut given = Given::default();
    // Nothing given yet: a lag before the first event replays from when listening began.
    let mut last: DateTime<Utc> = listening_since;
    loop {
        let ev = tokio::select! {
            _ = shutdown.cancelled() => return,
            ev = rx.recv() => ev,
        };
        match ev {
            Ok(ev) => {
                if given.first_time(ev.id) {
                    last = last.max(ev.at);
                    give(&id, &module, &ctx, ev, &tracker).await;
                }
            }
            Err(RecvError::Lagged(n)) => {
                // Never before listening began: those were the previous run's (#310's review).
                let since = (last - REPLAY_MARGIN).max(listening_since);
                let store = store.clone();
                let read = tokio::task::spawn_blocking(move || store.read_events_since(since)).await;
                let events = match read {
                    Ok(Ok(scan)) => scan.events,
                    Ok(Err(e)) => {
                        tracing::error!(module = %id, dropped = n, error = %e, "module lagged the bus and the log couldn't be read; events lost");
                        continue;
                    }
                    Err(e) => {
                        tracing::error!(module = %id, dropped = n, error = %e, "module lagged the bus and reading the log failed; events lost");
                        continue;
                    }
                };
                let mut replayed = 0;
                for ev in events {
                    if given.first_time(ev.id) {
                        last = last.max(ev.at);
                        give(&id, &module, &ctx, ev, &tracker).await;
                        replayed += 1;
                    }
                }
                tracing::warn!(module = %id, dropped = n, replayed, "module lagged the bus; caught up from the log");
            }
            Err(RecvError::Closed) => return,
        }
    }
}

/// One event to `on_event`, in a task of its own so a panic stays this event's.
async fn give(id: &ModuleId, module: &Arc<dyn Module>, ctx: &Ctx, ev: Event, tracker: &TaskTracker) {
    let (module, ctx, topic) = (module.clone(), ctx.clone(), ev.topic.clone());
    let handled = tracker.spawn(async move { module.on_event(&ev, &ctx).await }).await;
    match handled {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(module = %id, topic = %topic, error = %e, "on_event failed"),
        Err(e) if e.is_panic() => {
            tracing::error!(module = %id, topic = %topic, "on_event panicked; the module keeps getting events (if it keeps failing after this, it may need a restart)")
        }
        Err(e) => tracing::error!(module = %id, topic = %topic, error = %e, "on_event task failed"),
    }
}
