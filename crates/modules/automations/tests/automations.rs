//! The automations module against an in-memory `Ctx` (§12 rule 13): rules on disk, firing,
//! delayed actions and the loop guard (ADR 0035).

use std::sync::Arc;

use serde_json::{json, Value};
use shimmer_automations::Automations;
use shimmer_core::testing::TestEnv;
use shimmer_core::{
    Catalog, CommandSpec, Ctx, ErrorCode, Event, Execution, Module, ModuleId, Origin, StoreBackend, TaskId,
};

fn env() -> TestEnv {
    let mut env = TestEnv::new("automations");
    let op = |op: &str| CommandSpec {
        op: op.into(),
        summary: op.into(),
        params_schema: json!({}),
        execution: Execution::Inline,
    };
    let topics = [
        ("records.item.updated", Some("a record's fields were changed")),
        ("records.item.completed", Some("a record was marked done")),
        ("queue.task.failed", Some("a task failed")),
    ];
    env.ctx.catalog = Arc::new(Catalog::new(
        ["notify.send", "workspaces.activate", "records.update"].map(op),
        topics.map(|(t, d)| (t.to_owned(), d.map(str::to_owned))),
    ));
    env
}

async fn call(m: &Automations, ctx: &Ctx, op: &str, params: Value) -> shimmer_core::Result<Value> {
    m.handle(op, params, ctx).await
}

fn event(topic: &str, payload: Value, cause: Option<TaskId>) -> Event {
    Event::new(ModuleId::new("records"), topic, payload, chrono::DateTime::UNIX_EPOCH).caused_by(cause)
}

/// The built-in rules this catalog loads: the two on `queue.task.failed` (§10).
const BUILTINS: usize = 2;

fn rule<'a>(listed: &'a Value, id: &str) -> &'a Value {
    listed["rules"].as_array().unwrap().iter().find(|r| r["id"] == id).unwrap()
}

fn topics(env: &TestEnv) -> Vec<String> {
    env.backend.events().into_iter().map(|e| e.topic).collect()
}

fn last(env: &TestEnv, topic: &str) -> Value {
    env.backend.events().into_iter().rev().find(|e| e.topic == topic).map(|e| e.payload).unwrap_or_default()
}

const DONE: &str = r#"
[rule]
label = "Tell me when it's done"
on = "records.item.completed"
when = { collection = "leetcode" }

[[do]]
op = "notify.send"
params = { title = "Solved: {title}", tags = "{item.topic}" }

[[do]]
op = "notify.send"
params = { title = "Redo {title}" }
after = "60d"
"#;

async fn started(env: &TestEnv) -> Automations {
    let m = Automations::default();
    m.init(&env.ctx).await.unwrap();
    m
}

#[tokio::test]
async fn rules_are_files_made_checked_switched_and_removed() {
    let env = env();
    let m = started(&env).await;
    let ctx = &env.ctx;

    let made = call(&m, ctx, "automations.create", json!({"id": "solved", "text": DONE})).await.unwrap();
    assert_eq!((made["label"].clone(), made["enabled"].clone()), (json!("Tell me when it's done"), json!(true)));
    assert_eq!(env.backend.read("automations", "rules/solved.toml").unwrap().unwrap(), DONE.as_bytes());
    let dup = call(&m, ctx, "automations.create", json!({"id": "solved", "text": DONE})).await.unwrap_err();
    assert_eq!(dup.code, ErrorCode::Conflict);

    // A broken edit is refused and the old rule kept.
    let broken = DONE.replace("notify.send", "notify.nope");
    let err = call(&m, ctx, "automations.save", json!({"id": "solved", "text": broken})).await.unwrap_err();
    assert!(err.message.contains("no op 'notify.nope'"), "{}", err.message);
    assert_eq!(call(&m, ctx, "automations.get", json!({"id": "solved"})).await.unwrap()["text"], DONE);

    // Switching off keeps the file as written, comments and all.
    call(&m, ctx, "automations.disable", json!({"id": "solved"})).await.unwrap();
    let text = String::from_utf8(env.backend.read("automations", "rules/solved.toml").unwrap().unwrap()).unwrap();
    assert!(text.contains("enabled = false") && text.contains("label = \"Tell me when it's done\""), "{text}");
    let listed = call(&m, ctx, "automations.list", json!({})).await.unwrap();
    assert_eq!(rule(&listed, "solved")["enabled"], false);
    call(&m, ctx, "automations.enable", json!({"id": "solved"})).await.unwrap();

    // A starter rule checks, and is off until edited.
    let starter = call(&m, ctx, "automations.create", json!({"id": "mine", "label": "Mine"})).await.unwrap();
    assert_eq!((starter["enabled"].clone(), starter["on"].clone()), (json!(false), json!("records.item.completed")));

    call(&m, ctx, "automations.remove", json!({"id": "mine"})).await.unwrap();
    assert!(env.backend.read("automations", "rules/mine.toml").unwrap().is_none());
    assert_eq!(call(&m, ctx, "automations.get", json!({"id": "mine"})).await.unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(call(&m, ctx, "automations.list", json!({})).await.unwrap()["removed"], json!(["mine"]));

    // Restored as it was; not over a rule of the same id, and only once.
    call(&m, ctx, "automations.create", json!({"id": "mine", "text": DONE})).await.unwrap();
    let busy = call(&m, ctx, "automations.restore", json!({"id": "mine"})).await.unwrap_err();
    assert_eq!(busy.code, ErrorCode::Conflict);
    let before = env.backend.read("automations", "rules/solved.toml").unwrap().unwrap();
    call(&m, ctx, "automations.remove", json!({"id": "solved"})).await.unwrap();
    let back = call(&m, ctx, "automations.restore", json!({"id": "solved"})).await.unwrap();
    assert_eq!(
        (back["rule"]["label"].clone(), back["problem"].clone()),
        (json!("Tell me when it's done"), json!(null))
    );
    assert_eq!(env.backend.read("automations", "rules/solved.toml").unwrap().unwrap(), before, "as it was");
    let gone = call(&m, ctx, "automations.restore", json!({"id": "solved"})).await.unwrap_err();
    assert_eq!(gone.code, ErrorCode::NotFound);
    assert_eq!(
        topics(&env),
        [
            "automations.rule.created",
            "automations.rule.disabled",
            "automations.rule.enabled",
            "automations.rule.created",
            "automations.rule.removed",
            "automations.rule.created",
            "automations.rule.removed",
            "automations.rule.restored"
        ]
    );
}

#[tokio::test]
async fn a_matching_event_queues_its_actions_and_schedules_the_delayed_one() {
    let env = env();
    let m = started(&env).await;
    call(&m, &env.ctx, "automations.create", json!({"id": "solved", "text": DONE})).await.unwrap();

    let payload = json!({"collection": "leetcode", "title": "Two Sum", "item": {"topic": ["arrays", "hashing"]}});
    m.on_event(&event("records.item.completed", payload.clone(), None), &env.ctx).await.unwrap();
    m.on_event(&event("records.item.completed", json!({"collection": "jobs"}), None), &env.ctx).await.unwrap();

    let queued = env.queue.requests.lock().unwrap().clone();
    assert_eq!(queued.len(), 1, "the other collection doesn't match");
    assert_eq!(queued[0].op, "notify.send");
    assert_eq!(queued[0].params, json!({"title": "Solved: Two Sum", "tags": ["arrays", "hashing"]}));
    assert_eq!(queued[0].lane, None, "the op's own lane");
    assert_eq!(queued[0].origin, Origin::Module { id: ModuleId::new("automations") });

    let added = env.schedules.added.lock().unwrap().clone();
    assert_eq!(added.len(), 1);
    let (_, _, once) = &added[0];
    // Scheduled through automations, carrying the chain (ADR 0035 §3).
    assert_eq!(once.op, "automations.deferred");
    assert_eq!(
        (once.params["rule"].clone(), once.params["op"].clone(), once.params["params"].clone()),
        (json!("solved"), json!("notify.send"), json!({"title": "Redo Two Sum"}))
    );
    assert_eq!(once.params["chain"]["rules"], json!(["solved"]));
    assert_eq!(once.at, env.clock.now() + chrono::Duration::days(60));
    assert_eq!(once.label, "rule solved: Tell me when it's done");

    let fired = last(&env, "automations.rule.fired");
    assert_eq!(
        (fired["rule"].clone(), fired["depth"].clone()),
        (json!("solved"), json!(1)),
        "the first rule of a chain"
    );
    assert_eq!(fired["queued"].as_array().unwrap().len(), 1);
    assert_eq!(fired["scheduled"].as_array().unwrap().len(), 1);

    let history = call(&m, &env.ctx, "automations.history", json!({"id": "solved"})).await.unwrap();
    assert_eq!(history["history"].as_array().unwrap().len(), 1);
    let listed = call(&m, &env.ctx, "automations.list", json!({})).await.unwrap();
    assert!(rule(&listed, "solved")["last_fired"].is_string());
}

/// §3: "when a record is updated, update a record" re-triggers itself; A → B → A too.
#[tokio::test]
async fn the_loop_guard_stops_cycles_and_deep_chains() {
    let env = env();
    let m = started(&env).await;
    let ctx = &env.ctx;
    let rule = |on: &str, op: &str| format!("[rule]\non = \"{on}\"\n[[do]]\nop = \"{op}\"\n");
    call(&m, ctx, "automations.create", json!({"id": "again", "text": rule("records.item.updated", "records.update")}))
        .await
        .unwrap();

    m.on_event(&event("records.item.updated", json!({}), None), ctx).await.unwrap();
    let task: TaskId =
        serde_json::from_value(last(&env, "automations.rule.fired")["queued"][0]["task_id"].clone()).unwrap();

    // The update that task made comes back as an event it caused: the rule is already in the chain.
    m.on_event(&event("records.item.updated", json!({}), Some(task)), ctx).await.unwrap();
    assert_eq!(env.queue.requests.lock().unwrap().len(), 1, "no second task");
    let stopped = last(&env, "automations.chain.stopped");
    assert_eq!((stopped["rule"].clone(), stopped["reason"].clone()), (json!("again"), json!("cycle")));

    // Depth: with a limit of 1, a different rule can't continue the chain either.
    call(&m, ctx, "automations.depth", json!({"depth": 1})).await.unwrap();
    call(&m, ctx, "automations.create", json!({"id": "other", "text": rule("records.item.completed", "notify.send")}))
        .await
        .unwrap();
    m.on_event(&event("records.item.completed", json!({}), Some(task)), ctx).await.unwrap();
    assert_eq!(last(&env, "automations.chain.stopped")["reason"], "depth");
    assert_eq!(env.queue.requests.lock().unwrap().len(), 1);

    // What a person does starts a new chain.
    m.on_event(&event("records.item.completed", json!({}), None), ctx).await.unwrap();
    assert_eq!(env.queue.requests.lock().unwrap().len(), 2);

    let status = call(&m, ctx, "automations.status", json!({})).await.unwrap();
    assert_eq!((status["depth"].clone(), status["stopped"].as_array().unwrap().len()), (json!(1), 2));
    let health = call(&m, ctx, "automations.health", json!({})).await.unwrap();
    assert_eq!(health["checks"][0]["status"], "warn");
    let listed = call(&m, ctx, "automations.list", json!({})).await.unwrap();
    assert!(listed["rules"].as_array().unwrap().iter().any(|r| r["loop_stopped_at"].is_string()));
}

#[tokio::test]
async fn settings_and_hand_edited_files_are_read_and_problems_reported() {
    let env = env();
    env.ctx.store.write("rules/good.toml", DONE).unwrap();
    env.ctx.store.write("rules/bad.toml", "[rule]\non = \"nope\"\n").unwrap();
    env.ctx.store.write("settings.toml", "depth = 5\n").unwrap();
    let m = started(&env).await;
    let ctx = &env.ctx;

    let status = call(&m, ctx, "automations.status", json!({})).await.unwrap();
    assert_eq!((status["rules"].clone(), status["depth"].clone()), (json!(1 + BUILTINS), json!(5)));
    assert_eq!(status["problems"][0]["file"], "rules/bad.toml");
    let health = call(&m, ctx, "automations.health", json!({})).await.unwrap();
    assert_eq!(health["checks"][0]["status"], "fail");

    // Fixed by hand, then reloaded.
    env.ctx.store.write("rules/bad.toml", DONE).unwrap();
    let reloaded = call(&m, ctx, "automations.reload", json!({})).await.unwrap();
    assert_eq!((reloaded["rules"].clone(), reloaded["problems"].clone()), (json!(2 + BUILTINS), json!([])));

    // A broken file can be removed: one way to fix it.
    env.ctx.store.write("rules/worse.toml", "x").unwrap();
    call(&m, ctx, "automations.reload", json!({})).await.unwrap();
    call(&m, ctx, "automations.remove", json!({"id": "worse"})).await.unwrap();
    assert_eq!(call(&m, ctx, "automations.status", json!({})).await.unwrap()["problems"], json!([]));
    // Restored, it's still broken: reported again, not lost.
    let back = call(&m, ctx, "automations.restore", json!({"id": "worse"})).await.unwrap();
    assert_eq!(back["rule"], json!(null));
    assert!(back["problem"].as_str().unwrap().contains("not a rule file"));
    assert_eq!(
        call(&m, ctx, "automations.status", json!({})).await.unwrap()["problems"][0]["file"],
        "rules/worse.toml"
    );
    call(&m, ctx, "automations.remove", json!({"id": "worse"})).await.unwrap();

    let set = call(&m, ctx, "automations.depth", json!({"depth": 11})).await.unwrap_err();
    assert_eq!(set.code, ErrorCode::InvalidParams);
    call(&m, ctx, "automations.depth", json!({"depth": 2})).await.unwrap();
    let text = String::from_utf8(env.backend.read("automations", "settings.toml").unwrap().unwrap()).unwrap();
    assert_eq!(text.trim(), "depth = 2");
    assert_eq!(last(&env, "automations.settings.changed"), json!({"depth": 2}));
}

#[tokio::test]
async fn test_shows_what_a_rule_would_do_and_events_lists_what_it_can_react_to() {
    let env = env();
    let m = started(&env).await;
    let ctx = &env.ctx;
    call(&m, ctx, "automations.create", json!({"id": "solved", "text": DONE})).await.unwrap();
    call(&m, ctx, "automations.disable", json!({"id": "solved"})).await.unwrap();

    let sample = json!({"topic": "records.item.completed", "payload": {"collection": "leetcode", "title": "Two Sum"}});
    let t = call(&m, ctx, "automations.test", json!({"id": "solved", "event": sample})).await.unwrap();
    assert_eq!((t["matches"].clone(), t["enabled"].clone()), (json!(true), json!(false)), "tested as if on");
    assert_eq!(t["actions"][0]["params"]["title"], "Solved: Two Sum");
    assert_eq!(t["actions"][1]["after_s"], 60 * 86_400);
    assert!(env.queue.requests.lock().unwrap().is_empty(), "nothing ran");

    let other = json!({"topic": "records.item.completed", "payload": {"collection": "jobs"}});
    let t = call(&m, ctx, "automations.test", json!({"id": "solved", "event": other})).await.unwrap();
    assert_eq!((t["matches"].clone(), t["actions"].clone()), (json!(false), json!([])));

    let events = call(&m, ctx, "automations.events", json!({})).await.unwrap();
    assert!(events["events"]
        .as_array()
        .unwrap()
        .contains(&json!({"topic": "queue.task.failed", "description": "a task failed"})));
}

/// Fails the first transaction that writes the history file, then behaves.
struct FailFirstHistory {
    inner: Arc<shimmer_core::testing::MemBackend>,
    failed: std::sync::atomic::AtomicBool,
}

impl StoreBackend for FailFirstHistory {
    fn read(&self, ns: &str, path: &str) -> shimmer_core::Result<Option<Vec<u8>>> {
        self.inner.read(ns, path)
    }

    fn list(&self, ns: &str, prefix: &str) -> shimmer_core::Result<Vec<String>> {
        self.inner.list(ns, prefix)
    }

    fn commit(&self, ns: &str, plan: shimmer_core::TxPlan) -> shimmer_core::Result<()> {
        let history =
            plan.writes.iter().any(|w| matches!(w, shimmer_core::Write::Put { path, .. } if path == "history.json"));
        if history && !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Err(shimmer_core::Error::internal("disk full"));
        }
        self.inner.commit(ns, plan)
    }
}

/// "Every matching rule fires": one rule failing never stops the next (#252's review).
#[tokio::test]
async fn one_rule_failing_never_stops_the_others() {
    let mut env = env();
    let rule = |label: &str| {
        format!("[rule]\nlabel = \"{label}\"\non = \"records.item.completed\"\n\n[[do]]\nop = \"notify.send\"\nparams = {{ title = \"{label}\" }}\n")
    };
    let m = started(&env).await;
    for id in ["a-first", "b-second"] {
        call(&m, &env.ctx, "automations.create", json!({"id": id, "text": rule(id)})).await.unwrap();
    }
    let failing = FailFirstHistory { inner: env.backend.clone(), failed: Default::default() };
    env.ctx.store = shimmer_core::NamespacedStore::new(
        "automations",
        ModuleId::new("automations"),
        Arc::new(failing),
        env.clock.clone(),
    );
    let err = m.on_event(&event("records.item.completed", json!({}), None), &env.ctx).await.unwrap_err();
    assert!(err.message.contains("a-first") && err.message.contains("disk full"), "{}", err.message);
    let fired: Vec<Value> = env
        .backend
        .events()
        .into_iter()
        .filter(|e| e.topic == "automations.rule.fired")
        .map(|e| e.payload["rule"].clone())
        .collect();
    assert_eq!(fired, [json!("b-second")], "the second rule still fired");
}

/// A delayed action that causes the rule's own event is still a cycle: the chain travels with
/// the delay, so "re-set the stage in 3 days" can't loop every 3 days (#238's review).
#[tokio::test]
async fn a_delayed_action_continues_its_chain() {
    let env = env();
    let m = started(&env).await;
    let ctx = &env.ctx;
    let text = "[rule]\non = \"records.item.updated\"\n[[do]]\nop = \"records.update\"\nafter = \"3d\"\n";
    call(&m, ctx, "automations.create", json!({"id": "waiting", "text": text})).await.unwrap();

    m.on_event(&event("records.item.updated", json!({}), None), ctx).await.unwrap();
    let (_, _, once) = env.schedules.added.lock().unwrap()[0].clone();
    // Three days later the scheduler runs it, stamping `scheduled_for` as it does.
    let mut params = once.params.clone();
    params["scheduled_for"] = json!("2026-10-20T09:00:00Z");
    let ran = m.handle(&once.op, params.clone(), ctx).await.unwrap();
    let task: TaskId = serde_json::from_value(ran["task_id"].clone()).unwrap();
    assert_eq!(env.queue.requests.lock().unwrap().last().unwrap().op, "records.update");

    // The update it made comes back caused by that task: the rule is already in the chain.
    m.on_event(&event("records.item.updated", json!({}), Some(task)), ctx).await.unwrap();
    assert_eq!(env.schedules.added.lock().unwrap().len(), 1, "no second delayed action");
    assert_eq!(last(&env, "automations.chain.stopped")["reason"], "cycle");

    // Switched off meanwhile: the delayed action does nothing.
    call(&m, ctx, "automations.disable", json!({"id": "waiting"})).await.unwrap();
    let before = env.queue.requests.lock().unwrap().len();
    let skipped = m.handle(&once.op, params, ctx).await.unwrap();
    assert!(skipped["skipped"].is_string(), "{skipped}");
    assert_eq!(env.queue.requests.lock().unwrap().len(), before);
}
