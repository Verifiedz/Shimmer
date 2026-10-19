//! End to end over a real Unix socket, speaking only `shimmer-proto` frames: exactly what any
//! client sees.

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use serde_json::{json, Value};
use shimmer_core::{Clock, CommandSpec, Ctx, ErrorCode, Execution, Manifest, Module, Result};
use shimmer_daemon::{Daemon, DaemonConfig};
use shimmer_proto::ServerFrame;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;

// ---------------------------------------------------------------- M0: ping and manifest

#[tokio::test]
async fn ping_and_manifest_round_trip() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;

    let pong = c.call("core.ping", json!({})).await.unwrap();
    assert_eq!(pong["pong"], true);
    assert!(pong["uptime_s"].is_u64());

    let m = c.call("core.manifest", json!({})).await.unwrap();
    assert_eq!(m["protocol"], 1);
    let lanes: Vec<_> = m["lanes"].as_array().unwrap().iter().map(|l| l["id"].as_str().unwrap()).collect();
    assert_eq!(lanes, ["default", "slow"]);
    let demo = &m["modules"][0];
    assert_eq!(demo["id"], "demo");
    assert_eq!(demo["namespace"], "demo");
    let ops: Vec<_> = demo["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["op"].as_str().unwrap(), c["execution"].clone()))
        .collect();
    assert!(ops.contains(&("demo.echo", json!("inline"))));
    assert!(ops.contains(&("demo.quick", json!({"queued": {"lane": "slow"}}))));
    stop(d).await;
}

#[tokio::test]
async fn handshake_rejects_wrong_version_and_missing_hello() {
    let env = Env::new();
    let d = env.start().await;

    let mut c = Client::raw(&env.sock).await;
    c.send_json(json!({"v":2,"kind":"hello","client":"t","client_version":"0"})).await;
    let Some(ServerFrame::Error { error, .. }) = c.recv().await else { panic!("want error frame") };
    assert_eq!(error.code, ErrorCode::UnsupportedVersion);
    assert!(c.recv().await.is_none(), "daemon closes after rejecting");

    let mut c = Client::raw(&env.sock).await;
    c.send_json(json!({"v":1,"kind":"request","id":"1","op":"core.ping","params":{}})).await;
    let Some(ServerFrame::Error { error, .. }) = c.recv().await else { panic!("want error frame") };
    assert_eq!(error.code, ErrorCode::BadRequest);
    assert!(c.recv().await.is_none());
    stop(d).await;
}

#[tokio::test]
async fn malformed_frames_are_reported_and_do_not_kill_the_connection() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.wr.write_all(b"this is not json\n\n").await.unwrap();
    let Some(ServerFrame::Error { error, .. }) = c.recv().await else { panic!() };
    assert_eq!(error.code, ErrorCode::BadRequest);
    assert_eq!(c.call("core.ping", json!({})).await.unwrap()["pong"], true);
    stop(d).await;
}

#[tokio::test]
async fn unknown_ops_and_aliases_are_rejected() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    for op in ["nope.nothing", "demo.missing", "bankai", "core.pong"] {
        assert_eq!(c.call(op, json!({})).await.unwrap_err().code, ErrorCode::UnknownOp, "{op}");
    }
    stop(d).await;
}

#[tokio::test]
async fn requests_are_pipelined_and_matched_by_id() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    for i in 0..20 {
        c.send_json(json!({"v":1,"kind":"request","id":format!("p{i}"),"op":"demo.echo","params":{"i":i}})).await;
    }
    let mut seen = [false; 20];
    for _ in 0..20 {
        let Some(ServerFrame::Response { id, ok, data, .. }) = c.recv().await else { panic!() };
        assert!(ok);
        let i = data.unwrap()["i"].as_u64().unwrap() as usize;
        assert_eq!(id, format!("p{i}"));
        seen[i] = true;
    }
    assert!(seen.iter().all(|s| *s));
    stop(d).await;
}

#[tokio::test]
async fn a_panicking_handler_is_an_internal_error_not_a_dead_daemon() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    assert_eq!(c.call("demo.panic", json!({})).await.unwrap_err().code, ErrorCode::Internal);
    assert_eq!(c.call("core.ping", json!({})).await.unwrap()["pong"], true);
    stop(d).await;
}

// ---------------------------------------------------------------- subscriptions

#[tokio::test]
async fn subscription_filters_by_topic_pattern() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["demo.*"]).await;
    c.call("demo.put", json!({"key":"a","val":"1"})).await.unwrap();
    let ev = c.event("demo.item.saved").await;
    assert_eq!(ev.payload["key"], "a");
    assert_eq!(ev.source.as_str(), "demo");

    // Queue traffic is not in the subscription.
    let h = c.call("demo.quick", json!({})).await.unwrap();
    let id = task_id(&h);
    let mut watcher = Client::connect(&env.sock).await;
    for _ in 0..50 {
        if watcher.call("queue.task", json!({"task_id": id})).await.unwrap()["status"] == "succeeded" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(c.call("core.ping", json!({})).await.unwrap()["pong"], true);
    assert!(c.events.iter().all(|e| e.topic.starts_with("demo.")), "{:?}", c.events);

    c.send_json(json!({"v":1,"kind":"subscribe","id":"bad","topics":["rec*"]})).await;
    assert_eq!(c.response("bad").await.unwrap_err().code, ErrorCode::InvalidParams);
    stop(d).await;
}

// ---------------------------------------------------------------- queue

#[tokio::test]
async fn queued_op_returns_a_handle_then_reports_through_events() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.*"]).await;

    let h = c.call("demo.quick", json!({})).await.unwrap();
    assert_eq!(h["lane"], "slow");
    assert_eq!(h["position"], 0);
    let id = task_id(&h);

    for topic in ["queue.task.enqueued", "queue.task.started", "queue.task.finished"] {
        assert_eq!(c.event(topic).await.payload["task_id"], id.as_str(), "{topic}");
    }
    let t = c.call("queue.task", json!({"task_id": id})).await.unwrap();
    assert_eq!(t["status"], "succeeded");
    assert_eq!(t["origin"], json!({"type": "user"}));
    stop(d).await;
}

#[tokio::test]
async fn a_failed_task_never_blocks_its_lane() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.task.*"]).await;

    let failing = task_id(&c.call("demo.fail", json!({})).await.unwrap());
    let after = task_id(&c.call("demo.quick", json!({})).await.unwrap());

    let failed = c.event("queue.task.failed").await;
    assert_eq!(failed.payload["task_id"], failing.as_str());
    assert_eq!(failed.payload["code"], "unavailable");
    assert_eq!(failed.payload["lane"], "slow");
    assert_eq!(failed.payload["attempts"], 1, "the queue does not retry");
    assert_eq!(c.event("queue.task.finished").await.payload["task_id"], after.as_str());
    stop(d).await;
}

#[tokio::test]
async fn lane_concurrency_and_cancellation() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.task.*"]).await;

    // Lane `slow` has max_concurrent = 1: the second task must wait.
    let running = task_id(&c.call("demo.spin", json!({})).await.unwrap());
    let h2 = c.call("demo.quick", json!({})).await.unwrap();
    assert_eq!(h2["position"], 0, "first in the waiting list");
    let waiting = task_id(&h2);
    c.event("queue.task.started").await;
    let progress = c.event("queue.task.progress").await;
    assert_eq!(progress.payload["note"], "spinning");

    let list = c.call("queue.list", json!({"lane": "slow"})).await.unwrap();
    let lane = &list["lanes"][0];
    assert_eq!(lane["running"][0]["task_id"], running.as_str());
    assert_eq!(lane["queued"][0]["task_id"], waiting.as_str());
    assert_eq!(lane["max_concurrent"], 1);

    // Cancel the waiting task, then the running one.
    assert_eq!(c.call("queue.cancel", json!({"task_id": waiting})).await.unwrap()["was"], "queued");
    assert_eq!(c.event("queue.task.cancelled").await.payload["task_id"], waiting.as_str());
    assert_eq!(c.call("queue.cancel", json!({"task_id": running})).await.unwrap()["was"], "running");
    assert_eq!(c.event("queue.task.cancelled").await.payload["task_id"], running.as_str());

    assert_eq!(c.call("queue.cancel", json!({"task_id": running})).await.unwrap_err().code, ErrorCode::NotCancellable);
    assert_eq!(
        c.call("queue.cancel", json!({"task_id": shimmer_core::TaskId::new()})).await.unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(c.call("queue.list", json!({"lane": "ghost"})).await.unwrap_err().code, ErrorCode::LaneUnknown);
    stop(d).await;
}

// ---------------------------------------------------------------- durability and lifecycle

#[tokio::test]
async fn committed_data_survives_a_restart_and_reaches_log_and_index() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.call("demo.put", json!({"key":"two-sum","val":"done"})).await.unwrap();
    drop(c);
    stop(d).await;
    assert!(!env.sock.exists(), "socket removed on clean shutdown");

    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    assert_eq!(c.call("demo.get", json!({"key":"two-sum"})).await.unwrap()["val"], "done");
    stop(d).await;

    // The file is truth, human-readable, where CLAUDE.md §7 says it is.
    let file = env.home.path().join("data/demo/items/two-sum.txt");
    assert_eq!(std::fs::read_to_string(file).unwrap(), "done");
    let scan = shimmer_store::Store::open(env.home.path()).unwrap().read_events().unwrap();
    assert!(scan.events.iter().any(|e| e.topic == "demo.item.saved"));

    // The index is derived: it has the event too, and would be rebuilt if deleted.
    let idx = shimmer_store::Index::open(&env.home.path().join("index.sqlite")).unwrap();
    assert_eq!(idx.events_matching("demo.item.saved", 10).unwrap().len(), 1);
}

#[tokio::test]
async fn core_shutdown_replies_then_stops_and_cleans_up() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    assert_eq!(c.call("core.shutdown", json!({})).await.unwrap()["ok"], true);
    tokio::time::timeout(T, d.wait()).await.expect("daemon did not stop");
    assert!(!env.sock.exists());
}

#[tokio::test]
async fn second_daemon_on_the_same_home_is_refused() {
    let env = Env::new();
    let d = env.start().await;
    let other = TempDir::new().unwrap();
    let err = Daemon::start(
        DaemonConfig {
            home: env.home.path().to_owned(),
            socket: other.path().join("b.sock"),
            clock: Clock::system(),
            scheduler_tick: Duration::from_secs(1),
        },
        vec![],
    )
    .await
    .err()
    .expect("second daemon must not start");
    assert_eq!(err.code, ErrorCode::Conflict);
    stop(d).await;
}

#[tokio::test]
async fn stale_socket_file_is_replaced() {
    let env = Env::new();
    drop(std::os::unix::net::UnixListener::bind(&env.sock).unwrap()); // dies without unlinking
    assert!(env.sock.exists());
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    assert_eq!(c.call("core.ping", json!({})).await.unwrap()["pong"], true);
    stop(d).await;
}

// ---------------------------------------------------------------- registration rules

struct Rogue(Manifest, Vec<CommandSpec>);

#[async_trait]
impl Module for Rogue {
    fn manifest(&self) -> Manifest {
        self.0.clone()
    }
    async fn init(&self, _: &Ctx) -> Result<()> {
        Ok(())
    }
    fn commands(&self) -> Vec<CommandSpec> {
        self.1.clone()
    }
    async fn handle(&self, _: &str, _: Value, _: &Ctx) -> Result<Value> {
        Ok(Value::Null)
    }
}

fn rogue(id: &str, ns: &str, ops: &[&str]) -> Arc<dyn Module> {
    Arc::new(Rogue(
        Manifest {
            id: id.into(),
            version: "0".into(),
            namespace: ns.into(),
            topics: vec![],
            topic_descriptions: Default::default(),
            capabilities: vec![],
        },
        ops.iter().map(|o| spec(o, Execution::Inline)).collect(),
    ))
}

#[tokio::test]
async fn registration_collisions_are_startup_failures() {
    type Case = (&'static str, Vec<Arc<dyn Module>>, ErrorCode);
    let cases: Vec<Case> = vec![
        ("duplicate id", vec![rogue("a", "a", &[]), rogue("a", "a2", &[])], ErrorCode::Conflict),
        ("shared namespace", vec![rogue("a", "shared", &[]), rogue("b", "shared", &[])], ErrorCode::Conflict),
        ("op outside own prefix", vec![rogue("a", "a", &["b.steal"])], ErrorCode::InvalidParams),
        ("reserved namespace", vec![rogue("a", "scheduler", &[])], ErrorCode::InvalidParams),
        ("reserved id", vec![rogue("core", "core", &[])], ErrorCode::InvalidParams),
        ("path-like namespace", vec![rogue("a", "../x", &[])], ErrorCode::InvalidParams),
        ("duplicate op", vec![rogue("a", "a", &["a.x", "a.x"])], ErrorCode::Conflict),
    ];
    for (name, modules, code) in cases {
        let env = Env::new();
        let err = env.try_start(modules).await.err().unwrap_or_else(|| panic!("{name}: should fail"));
        assert_eq!(err.code, code, "{name}: {err}");
        assert!(!env.sock.exists(), "{name}: nothing may be left listening");
    }
}
