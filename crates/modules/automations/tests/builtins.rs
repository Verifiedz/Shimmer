//! Built-in rules (ADR 0035 §10) against an in-memory `Ctx`: always there, switched not removed,
//! and each one says exactly what the old notify alerts said, once.

use std::sync::Arc;

use serde_json::{json, Value};
use shimmer_automations::Automations;
use shimmer_core::testing::TestEnv;
use shimmer_core::{Catalog, CommandSpec, ErrorCode, Event, Execution, Module, ModuleId};

const IDS: [&str; 4] = ["loop-stopped", "scheduled-task-failed", "workspace-dirty", "workspace-half-launched"];

fn env(with_notify: bool) -> TestEnv {
    let mut env = TestEnv::new("automations");
    let op = |op: &str| CommandSpec {
        op: op.into(),
        summary: String::new(),
        params_schema: json!({}),
        execution: Execution::Inline,
    };
    let ops: Vec<CommandSpec> = if with_notify { vec![op("notify.send")] } else { vec![op("records.update")] };
    let topics = ["queue.task.failed", "workspaces.session.dirty", "automations.chain.stopped"];
    env.ctx.catalog = Arc::new(Catalog::new(ops, topics.map(|t| (t.to_owned(), None))));
    env
}

async fn started(env: &TestEnv) -> Automations {
    let m = Automations::default();
    m.init(&env.ctx).await.unwrap();
    m
}

fn ev(topic: &str, payload: Value) -> Event {
    Event::new(ModuleId::new(topic.split('.').next().unwrap()), topic, payload, chrono::DateTime::UNIX_EPOCH)
}

/// What each event made automations send: the `notify.send` params queued.
async fn sent(m: &Automations, env: &TestEnv, event: Event) -> Vec<Value> {
    let before = env.queue.requests.lock().unwrap().len();
    m.on_event(&event, &env.ctx).await.unwrap();
    env.queue.requests.lock().unwrap()[before..].iter().map(|r| r.params.clone()).collect()
}

fn dirty(fallback: Option<bool>, lane: &str) -> Value {
    let mut p = json!({"op": "workspaces.activate", "lane": lane, "origin": {"type": "scheduler", "trigger_id": "t"},
        "code": "workspace_dirty", "error": "failed",
        "detail": {"workspace": "site", "failed_step": "2/5 editor", "log": "logs/site.log",
                   "fix": "shimmer workspaces reset site"}});
    if let Some(f) = fallback {
        p["fallback"] = json!(f);
    }
    p
}

#[tokio::test]
async fn built_ins_are_always_there_switched_but_never_removed_or_edited() {
    let env = env(true);
    let m = started(&env).await;
    let call = |op: &'static str, p: Value| m.handle(op, p, &env.ctx);

    let listed = call("automations.list", json!({})).await.unwrap();
    let ids: Vec<&str> = listed["rules"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, IDS);
    assert!(listed["rules"].as_array().unwrap().iter().all(|r| r["builtin"] == true && r["enabled"] == true));
    let shown = call("automations.show", json!({"id": "workspace-dirty"})).await.unwrap();
    assert!(shown["text"].as_str().unwrap().contains("Then: {detail.fix}"), "{shown}");

    for (op, params) in [
        ("automations.remove", json!({"id": "loop-stopped"})),
        ("automations.edit", json!({"id": "loop-stopped", "text": "x"})),
        ("automations.new", json!({"id": "loop-stopped", "text": "x"})),
    ] {
        assert_eq!(call(op, params).await.unwrap_err().code, ErrorCode::Conflict, "{op}");
    }

    // Switched in the settings, and still off after a reload.
    call("automations.disable", json!({"id": "scheduled-task-failed"})).await.unwrap();
    let settings = String::from_utf8(env.ctx.store.read("settings.toml").unwrap().unwrap()).unwrap();
    assert!(settings.contains("[builtin]\nscheduled_task_failed = false"), "{settings}");
    call("automations.reload", json!({})).await.unwrap();
    let listed = call("automations.list", json!({})).await.unwrap();
    let off = listed["rules"].as_array().unwrap().iter().find(|r| r["id"] == "scheduled-task-failed").unwrap();
    assert_eq!(off["enabled"], false);
    let fired = sent(
        &m,
        &env,
        ev(
            "queue.task.failed",
            json!({"op": "records.add", "lane": "default",
        "origin": {"type": "scheduler"}, "code": "unavailable", "error": "down"}),
        ),
    )
    .await;
    assert!(fired.is_empty(), "switched off");
    call("automations.enable", json!({"id": "scheduled-task-failed"})).await.unwrap();
    assert!(String::from_utf8(env.ctx.store.read("settings.toml").unwrap().unwrap())
        .unwrap()
        .contains("scheduled_task_failed = true"));
}

#[tokio::test]
async fn each_built_in_says_what_happened_once() {
    let env = env(true);
    let m = started(&env).await;

    let msg = sent(&m, &env, ev("queue.task.failed", dirty(None, "workspaces"))).await;
    assert_eq!(msg.len(), 1, "one message, not also the scheduled-task one");
    assert_eq!(msg[0]["title"], "site couldn't launch");
    assert_eq!(msg[0]["body"], "Step 2/5 editor failed. Log: logs/site.log. Then: shimmer workspaces reset site");
    assert_eq!(msg[0]["priority"], "high");
    assert!(
        sent(&m, &env, ev("queue.task.failed", dirty(Some(true), "workspaces"))).await.is_empty(),
        "its own fallback says it"
    );
    assert_eq!(sent(&m, &env, ev("queue.task.failed", dirty(Some(false), "workspaces"))).await.len(), 1);

    let failed = |origin: &str, lane: &str| {
        ev(
            "queue.task.failed",
            json!({"op": "records.add", "lane": lane, "origin": {"type": origin}, "code": "unavailable", "error": "down"}),
        )
    };
    let msg = sent(&m, &env, failed("scheduler", "default")).await;
    assert_eq!(
        (msg[0]["title"].clone(), msg[0]["body"].clone()),
        (json!("Scheduled records.add failed"), json!("down"))
    );
    assert!(sent(&m, &env, failed("user", "default")).await.is_empty(), "only scheduled ones");
    assert!(sent(&m, &env, failed("scheduler", "notify")).await.is_empty(), "never notify's own work");

    let half = json!({"workspace": "site", "recovered": true, "failed_step": "3/5 tmux", "log": "logs/site.log"});
    let msg = sent(&m, &env, ev("workspaces.session.dirty", half)).await;
    assert_eq!(msg[0]["title"], "site was left half-launched");
    let normal = json!({"workspace": "site", "failed_step": "3/5 tmux", "log": "logs/site.log"});
    assert!(sent(&m, &env, ev("workspaces.session.dirty", normal)).await.is_empty(), "a failed task already said it");

    let stopped = json!({"rule": "again", "reason": "cycle", "chain": "c", "rules": ["again"], "event": {}});
    let msg = sent(&m, &env, ev("automations.chain.stopped", stopped)).await;
    assert_eq!(msg[0]["title"], "Automation again was stopped");
    assert!(msg[0]["body"].as_str().unwrap().contains("(cycle)"), "{}", msg[0]);
}

#[tokio::test]
async fn without_notify_they_wait_quietly() {
    let env = env(false);
    let m = started(&env).await;
    let status = m.handle("automations.status", json!({}), &env.ctx).await.unwrap();
    assert_eq!((status["rules"].clone(), status["problems"].clone()), (json!(0), json!([])));
    // A file of yours can't take a built-in's id.
    env.ctx.store.write("rules/loop-stopped.toml", "[rule]\non = \"*\"\n[[do]]\nop = \"records.update\"\n").unwrap();
    m.handle("automations.reload", json!({}), &env.ctx).await.unwrap();
    let status = m.handle("automations.status", json!({}), &env.ctx).await.unwrap();
    assert!(status["problems"][0]["error"].as_str().unwrap().contains("built-in rule's id"), "{status}");
}
