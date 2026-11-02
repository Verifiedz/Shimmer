//! Every module is initialised before any reacts to an event: a module reacting at start must not
//! queue work for one still starting, which the queue would refuse as "not initialised" (the
//! cause of `automations_notify::a_workspace_left_half_launched_by_a_crash_is_reported` failing
//! under load: automations queued notify.send before notify had started).

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use serde_json::{json, Value};
use shimmer_core::{CommandSpec, Ctx, Event, Execution, Manifest, Module, Result};

fn manifest(id: &str, topics: Vec<String>) -> Manifest {
    Manifest {
        id: id.into(),
        version: "0.0.1".into(),
        namespace: id.into(),
        topics,
        topic_descriptions: Default::default(),
        capabilities: vec![],
    }
}

/// Emits one event while it starts, as workspaces does for a launch it finds half done.
struct Early;

#[async_trait]
impl Module for Early {
    fn manifest(&self) -> Manifest {
        manifest("early", vec!["early.thing.found".into()])
    }
    async fn init(&self, ctx: &Ctx) -> Result<()> {
        ctx.bus.emit("early.thing.found", json!({}))?;
        Ok(())
    }
    async fn handle(&self, op: &str, _: Value, _: &Ctx) -> Result<Value> {
        Err(shimmer_core::Error::unknown_op(op))
    }
}

/// Reacts to it by queuing the late module's op, as automations does with notify.send.
struct Reactor;

#[async_trait]
impl Module for Reactor {
    fn manifest(&self) -> Manifest {
        manifest("reactor", vec![])
    }
    async fn init(&self, _: &Ctx) -> Result<()> {
        Ok(())
    }
    async fn handle(&self, op: &str, _: Value, _: &Ctx) -> Result<Value> {
        Err(shimmer_core::Error::unknown_op(op))
    }
    async fn on_event(&self, ev: &Event, ctx: &Ctx) -> Result<()> {
        if ev.topic == "early.thing.found" {
            ctx.queue.enqueue("late.work", json!({})).await?;
        }
        Ok(())
    }
}

/// Slow to start, as a module reading its files on a busy machine is.
struct Late;

#[async_trait]
impl Module for Late {
    fn manifest(&self) -> Manifest {
        manifest("late", vec![])
    }
    async fn init(&self, _: &Ctx) -> Result<()> {
        tokio::time::sleep(Duration::from_millis(400)).await;
        Ok(())
    }
    fn commands(&self) -> Vec<CommandSpec> {
        vec![spec("late.work", Execution::Queued { lane: "default".into() })]
    }
    async fn handle(&self, _: &str, _: Value, _: &Ctx) -> Result<Value> {
        Ok(json!("done"))
    }
}

#[tokio::test]
async fn a_module_reacting_at_start_never_reaches_one_still_starting() {
    let env = Env::new();
    let d = env.try_start(vec![Arc::new(Early), Arc::new(Reactor), Arc::new(Late)]).await.unwrap();
    let logged = |topic: &str| -> Vec<Event> {
        shimmer_store::Store::open(env.home.path())
            .unwrap()
            .read_events()
            .unwrap()
            .events
            .into_iter()
            .filter(|e| e.topic == topic && e.payload["op"] == "late.work")
            .collect()
    };
    for _ in 0..200 {
        if !logged("queue.task.finished").is_empty() || !logged("queue.task.failed").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let failed = logged("queue.task.failed");
    assert!(failed.is_empty(), "late.work failed: {:?}", failed.first().map(|e| &e.payload["error"]));
    assert_eq!(logged("queue.task.finished").len(), 1, "late.work ran once the late module had started");
    stop(d).await;
}
