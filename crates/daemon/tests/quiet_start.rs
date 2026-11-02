//! ADR 0041 §5a: a quiet start, for a vault copy's first run. What was copied in was another
//! folder's run, so nothing scheduled runs (then, or at the next ordinary start), and modules can't
//! queue tasks; a person still can, which is how `vaults copy` switches the copy off.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use serde_json::{json, Value};
use shimmer_core::{Ctx, ErrorCode, Event, Manifest, Module, Result};

const DAY: u64 = 86_400;

fn iso(secs_after_t0: u64) -> String {
    (t0() + chrono::Duration::seconds(secs_after_t0 as i64)).format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn logged(env: &Env, topic: &str) -> Vec<Event> {
    shimmer_store::Store::open(env.home.path())
        .unwrap()
        .read_events()
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.topic == topic)
        .collect()
}

fn ran(env: &Env, op: &str) -> usize {
    logged(env, "queue.task.started").iter().filter(|e| e.payload["op"] == op).count()
}

async fn settle(c: &mut Client) {
    tokio::time::sleep(Duration::from_millis(150)).await;
    c.call("core.ping", json!({})).await.unwrap();
}

async fn add(c: &mut Client, schedule: Value, catch_up: &str) -> String {
    let r = c
        .call("scheduler.add", json!({"schedule": schedule, "op": "demo.quick", "params": {}, "catch_up": catch_up}))
        .await
        .unwrap();
    r["trigger_id"].as_str().unwrap().to_owned()
}

async fn trigger(c: &mut Client, id: &str) -> Value {
    let list = c.call("scheduler.list", json!({})).await.unwrap();
    list["triggers"].as_array().unwrap().iter().find(|t| t["id"] == id).cloned().unwrap()
}

/// Wasay's case on #312: a firing left unfinished and a recurring schedule long overdue, copied in.
/// The quiet start runs neither and settles both on disk, so the next ordinary start runs neither.
#[tokio::test]
async fn copied_firings_and_overdue_schedules_never_run_then_or_at_the_next_start() {
    let env = Env::fake_clock();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["scheduler.trigger.*"]).await;
    // `slow` runs one task at a time: a spin keeps the once's firing waiting, so it's unfinished.
    c.call("demo.spin", json!({})).await.unwrap();
    let once = add(&mut c, json!({"once": iso(60)}), "run_once").await;
    let hourly = add(&mut c, json!({"every": 3600}), "run_once").await;
    env.clock.advance(Duration::from_secs(120));
    c.event("scheduler.trigger.fired").await;
    stop(d).await;
    assert_eq!(ran(&env, "demo.quick"), 0, "still waiting when it stopped");
    // The copy is made two days later: the hourly one is long overdue.
    env.clock.advance(Duration::from_secs(2 * DAY));
    let fired = logged(&env, "scheduler.trigger.fired").len();

    let d = env.try_start_as(vec![Arc::new(Demo::default())], true).await.unwrap();
    let mut c = Client::connect(&env.sock).await;
    settle(&mut c).await;
    assert_eq!(ran(&env, "demo.quick"), 0, "nothing ran in the quiet start");
    assert_eq!(logged(&env, "scheduler.trigger.fired").len(), fired, "nothing fired");
    assert_eq!(trigger(&mut c, &once).await["waiting"], 0, "the copied firing was forgotten");
    let next = trigger(&mut c, &hourly).await["next_due"].as_str().unwrap().to_owned();
    assert!(next > iso(2 * DAY + 120), "counts from now, not two days ago: {next}");
    // A person can still run things: that's how `vaults copy` switches the copy off.
    c.call("scheduler.disable", json!({"trigger_id": hourly})).await.unwrap();
    assert_eq!(c.call("demo.echo", json!({"x": 1})).await.unwrap(), json!({"x": 1}));
    stop(d).await;

    // The next start is ordinary, and finds nothing to run: the quiet start's settling was saved.
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    settle(&mut c).await;
    env.clock.advance(Duration::from_secs(10));
    settle(&mut c).await;
    assert_eq!(ran(&env, "demo.quick"), 0, "not run at the next ordinary start either");
    assert_eq!(logged(&env, "scheduler.trigger.fired").len(), fired);
    assert!(logged(&env, "scheduler.trigger.fired").iter().all(|e| e.payload["after_restart"] != true));
    stop(d).await;
}

/// Without a quiet start, the same overdue schedule does catch up: the test above isn't passing
/// because nothing would have fired anyway.
#[tokio::test]
async fn an_ordinary_start_does_catch_up_the_same_schedule() {
    let env = Env::fake_clock();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    add(&mut c, json!({"every": 3600}), "run_once").await;
    stop(d).await;
    env.clock.advance(Duration::from_secs(2 * DAY));
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    for _ in 0..300 {
        if ran(&env, "demo.quick") > 0 {
            break;
        }
        settle(&mut c).await;
    }
    assert_eq!(ran(&env, "demo.quick"), 1);
    stop(d).await;
}

/// A module that asks for a task while it starts, as automations and notify can from `init`, and
/// says what the queue answered.
struct Eager(Arc<Mutex<Option<ErrorCode>>>);

#[async_trait]
impl Module for Eager {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "eager".into(),
            version: "0.0.1".into(),
            namespace: "eager".into(),
            topics: vec![],
            topic_descriptions: Default::default(),
            capabilities: vec![],
        }
    }

    async fn init(&self, ctx: &Ctx) -> Result<()> {
        let answer = ctx.queue.enqueue("demo.quick", json!({})).await.err().map(|e| e.code);
        *self.0.lock().unwrap() = answer;
        Ok(())
    }

    async fn handle(&self, op: &str, _params: Value, _ctx: &Ctx) -> Result<Value> {
        Err(shimmer_core::Error::unknown_op(op))
    }
}

#[tokio::test]
async fn a_task_a_module_asks_for_is_refused_and_never_queued() {
    for quiet in [true, false] {
        let env = Env::fake_clock();
        let answer = Arc::new(Mutex::new(None));
        let modules: Vec<Arc<dyn Module>> = vec![Arc::new(Demo::default()), Arc::new(Eager(answer.clone()))];
        let d = env.try_start_as(modules, quiet).await.unwrap();
        let mut c = Client::connect(&env.sock).await;
        settle(&mut c).await;
        let queued = logged(&env, "queue.task.enqueued").len();
        stop(d).await;
        if quiet {
            assert_eq!(*answer.lock().unwrap(), Some(ErrorCode::Unavailable), "refused while quiet");
            assert_eq!(queued, 0, "never enqueued, so nothing is left for a later start");
        } else {
            assert_eq!(*answer.lock().unwrap(), None, "an ordinary start queues it");
            assert_eq!(queued, 1);
        }
    }
}
