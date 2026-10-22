//! Events reach a module's `on_event` every one, in order: a module slower than the bus catches
//! up from the log (#261 finding 1), and one panic doesn't stop the rest (#261 finding 2).

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use serde_json::{json, Value};
use shimmer_core::{CommandSpec, Ctx, Event, Execution, Manifest, Module, Result};

/// Emits `n` events in one transaction (`counter.burst`) and writes down the `i` of each one
/// its `on_event` is given, slowly, as a module reading a file per event would.
#[derive(Default)]
struct Counter {
    seen: Arc<Mutex<Vec<u64>>>,
}

#[async_trait]
impl Module for Counter {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "counter".into(),
            version: "0.0.1".into(),
            namespace: "counter".into(),
            topics: vec!["counter.tick.happened".into()],
            topic_descriptions: Default::default(),
            capabilities: vec![],
        }
    }

    async fn init(&self, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    fn commands(&self) -> Vec<CommandSpec> {
        vec![spec("counter.burst", Execution::Inline), spec("counter.seen", Execution::Inline)]
    }

    async fn handle(&self, op: &str, params: Value, ctx: &Ctx) -> Result<Value> {
        match op {
            "counter.burst" => {
                let (from, n) = (params["from"].as_u64().unwrap_or(0), params["n"].as_u64().unwrap_or(0));
                let panic_at = params["panic_at"].as_u64();
                ctx.store.transaction(|tx| {
                    for i in from..from + n {
                        tx.emit("counter.tick.happened", json!({"i": i, "panic": Some(i) == panic_at}))?;
                    }
                    Ok(())
                })?;
                Ok(json!({"emitted": n}))
            }
            "counter.seen" => Ok(json!(*self.seen.lock().unwrap())),
            _ => Err(shimmer_core::Error::unknown_op(op)),
        }
    }

    async fn on_event(&self, ev: &Event, _ctx: &Ctx) -> Result<()> {
        if ev.topic != "counter.tick.happened" {
            return Ok(());
        }
        if ev.payload["panic"] == true {
            panic!("a module bug on one event");
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
        self.seen.lock().unwrap().push(ev.payload["i"].as_u64().unwrap());
        Ok(())
    }
}

async fn seen_until(c: &mut Client, n: usize) -> Vec<u64> {
    for _ in 0..3000 {
        let seen: Vec<u64> = serde_json::from_value(c.call("counter.seen", json!({})).await.unwrap()).unwrap();
        if seen.len() >= n {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    serde_json::from_value(c.call("counter.seen", json!({})).await.unwrap()).unwrap()
}

/// #261 finding 1: 2,000 events in one transaction, more than the bus holds, reach a module
/// slower than the bus: every one, once, in order. Before, it got 1,024 and lost the rest.
#[tokio::test]
async fn a_module_slower_than_the_bus_gets_every_event_once_in_order() {
    let env = Env::new();
    let d = env.try_start(vec![Arc::new(Counter::default())]).await.unwrap();
    let mut c = Client::connect(&env.sock).await;
    c.call("counter.burst", json!({"n": 2000})).await.unwrap();
    let seen = seen_until(&mut c, 2000).await;
    assert_eq!(seen.len(), 2000, "every one");
    assert_eq!(seen, (0..2000).collect::<Vec<_>>(), "once each, in order");

    // And it goes on listening as normal afterwards.
    c.call("counter.burst", json!({"from": 2000, "n": 3})).await.unwrap();
    let seen = seen_until(&mut c, 2003).await;
    assert_eq!(&seen[2000..], [2000, 2001, 2002]);
    stop(d).await;
}

/// #261 finding 2: one event that makes `on_event` panic doesn't stop the module getting the
/// rest. Before, it got none of them until a restart.
#[tokio::test]
async fn a_panic_on_one_event_doesnt_stop_the_rest() {
    let env = Env::new();
    let d = env.try_start(vec![Arc::new(Counter::default())]).await.unwrap();
    let mut c = Client::connect(&env.sock).await;
    c.call("counter.burst", json!({"n": 11, "panic_at": 0})).await.unwrap();
    let seen = seen_until(&mut c, 10).await;
    assert_eq!(seen, (1..11).collect::<Vec<_>>(), "all 10 after the panic");
    stop(d).await;
}

/// #310's review: a module that falls behind right after a restart catches up from the log, but
/// never from before it started listening: the previous run's last events were given to it then,
/// and giving them again would fire its rules twice.
#[tokio::test]
async fn a_lag_right_after_a_restart_never_replays_the_previous_runs_events() {
    let env = Env::new();
    let d = env.try_start(vec![Arc::new(Counter::default())]).await.unwrap();
    let mut c = Client::connect(&env.sock).await;
    c.call("counter.burst", json!({"from": 90000, "n": 5})).await.unwrap();
    assert_eq!(seen_until(&mut c, 5).await.len(), 5);
    stop(d).await;

    // Started again at once, with a fresh module, and made to fall behind straight away.
    let d = env.try_start(vec![Arc::new(Counter::default())]).await.unwrap();
    let mut c = Client::connect(&env.sock).await;
    c.call("counter.burst", json!({"n": 2000})).await.unwrap();
    let seen = seen_until(&mut c, 2000).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let seen_after: Vec<u64> = serde_json::from_value(c.call("counter.seen", json!({})).await.unwrap()).unwrap();
    assert!(seen.iter().all(|i| *i < 90000), "the previous run's events came again");
    assert_eq!(seen_after, (0..2000).collect::<Vec<_>>(), "this run's, once each, in order");
    stop(d).await;
}
