//! Automations on the real daemon (ADR 0035): a rule reacts to a real event, the loop guard
//! follows `Event.cause` back from the task it queued, and a delayed action is a real schedule.

mod common;

use std::time::{Duration, Instant};

use common::{stderr, Home};
use serde_json::{json, Value};

fn call(home: &Home, op: &str, params: Value) -> Value {
    let o = home.shimmer(&["call", op, &params.to_string(), "--json"]);
    assert!(o.status.success(), "{op} failed: {}", stderr(&o));
    serde_json::from_slice(&o.stdout).unwrap()
}

/// Poll `op` until `done` holds; the queue runs actions a moment after the event.
fn until(home: &Home, op: &str, params: Value, done: impl Fn(&Value) -> bool) -> Value {
    let start = Instant::now();
    loop {
        let v = call(home, op, params.clone());
        if done(&v) {
            return v;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "{op} never got there: {v}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// "When a record is updated, update it": without the guard, a task per update forever.
#[test]
fn a_rule_that_triggers_itself_runs_once_then_is_stopped() {
    let home = Home::with_leetcode();
    call(&home, "records.add", json!({"collection": "leetcode", "id": "two-sum", "fields": {"title": "Two Sum"}}));
    let rule = r#"
[rule]
on = "records.item.updated"
when = { collection = "leetcode" }

[[do]]
op = "records.update"
params = { collection = "leetcode", id = "{id}", fields = { url = "https://x.test/{id}" } }
"#;
    call(&home, "automations.create", json!({"id": "fill-url", "text": rule}));

    call(&home, "records.update", json!({"collection": "leetcode", "id": "two-sum", "fields": {"difficulty": "easy"}}));
    let status = until(&home, "automations.status", json!({}), |s| !s["stopped"].as_array().unwrap().is_empty());
    assert_eq!(status["stopped"][0]["reason"], "cycle", "{status}");

    let record = call(&home, "records.get", json!({"collection": "leetcode", "id": "two-sum"}));
    assert_eq!(record["url"], "https://x.test/two-sum", "the rule ran: {record}");
    let history = call(&home, "automations.history", json!({"id": "fill-url"}))["history"].clone();
    let kinds: Vec<&str> = history.as_array().unwrap().iter().filter_map(|e| e["kind"].as_str()).collect();
    assert_eq!(kinds, ["stopped", "fired"], "fired once, then stopped: {history}");
}

/// How many times rule `id` has fired, from its history.
fn fired(home: &Home, id: &str) -> usize {
    let history = call(home, "automations.history", json!({"id": id, "limit": 300}))["history"].clone();
    history.as_array().unwrap().iter().filter(|e| e["kind"] == "fired").count()
}

/// A rule `on` some topic whose action is a queued task: its own action's `queue.task.*` events
/// set it off again. They carry the task as their cause, so the guard stops it as a cycle instead
/// of it firing forever (#251/#252's review). Started by `records.item.updated` through `kick`.
fn a_rule_set_off_by_its_own_task_is_stopped(on: &str) {
    let home = Home::with_leetcode();
    call(&home, "records.add", json!({"collection": "leetcode", "id": "two-sum", "fields": {"title": "Two Sum"}}));
    let kick = r#"
[rule]
on = "records.item.updated"
when = { collection = "leetcode", "item.difficulty" = "easy" }

[[do]]
op = "records.update"
params = { collection = "leetcode", id = "{id}", fields = { difficulty = "medium" } }
"#;
    let looping = format!(
        r#"
[rule]
on = "{on}"

[[do]]
op = "records.update"
params = {{ collection = "leetcode", id = "two-sum", fields = {{ url = "https://x.test/{{task_id}}" }} }}
"#
    );
    call(&home, "automations.create", json!({"id": "kick", "text": kick}));
    call(&home, "automations.create", json!({"id": "looping", "text": looping}));

    call(&home, "records.update", json!({"collection": "leetcode", "id": "two-sum", "fields": {"difficulty": "easy"}}));
    let status = until(&home, "automations.status", json!({}), |s| {
        s["stopped"].as_array().unwrap().iter().any(|x| x["rule"] == "looping")
    });
    let stop = status["stopped"].as_array().unwrap().iter().find(|x| x["rule"] == "looping").unwrap().clone();
    assert!(matches!(stop["reason"].as_str(), Some("cycle" | "depth")), "{stop}");

    // Stopped means stopped: no more firings a while later.
    let after_stop = fired(&home, "looping");
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(fired(&home, "looping"), after_stop, "it kept firing after it was stopped");
    // Bounded, not once: a rule on `*` reacts once to each of its first task's few events
    // (enqueued, started, finished, the record's update); every follow-up is then a cycle.
    assert!(after_stop <= 8, "fired {after_stop} times before the guard stopped it");
}

#[test]
fn a_rule_on_its_own_tasks_finishing_is_stopped() {
    a_rule_set_off_by_its_own_task_is_stopped("queue.task.finished");
}

#[test]
fn a_rule_on_every_event_is_stopped() {
    a_rule_set_off_by_its_own_task_is_stopped("*");
}

#[test]
fn a_delayed_action_waits_as_a_schedule_named_after_its_rule() {
    let home = Home::with_leetcode();
    call(&home, "records.add", json!({"collection": "leetcode", "id": "two-sum", "fields": {"title": "Two Sum"}}));
    let rule = r#"
[rule]
label = "Redo it later"
on = "records.item.completed"

[[do]]
op = "records.reopen"
params = { collection = "{collection}", id = "{id}" }
after = "60d"
"#;
    call(&home, "automations.create", json!({"id": "redo", "text": rule}));
    call(&home, "records.complete", json!({"collection": "leetcode", "id": "two-sum"}));

    // Other schedules may be there too (notify declares its own): find the one the rule added.
    let added = |l: &Value| l["triggers"].as_array().unwrap().iter().find(|t| t["source"]["type"] == "added").cloned();
    let listed = until(&home, "scheduler.list", json!({}), |l| added(l).is_some());
    let t = &added(&listed).unwrap();
    assert_eq!(t["source"], json!({"type": "added", "id": "automations", "label": "rule redo: Redo it later"}));
    // It waits as automations' own op, carrying the action and its chain (ADR 0035 §4).
    assert_eq!(t["op"], "automations.deferred");
    assert_eq!(
        (t["params"]["rule"].clone(), t["params"]["op"].clone(), t["params"]["params"].clone()),
        (json!("redo"), json!("records.reopen"), json!({"collection": "leetcode", "id": "two-sum"}))
    );
    assert_eq!(t["params"]["chain"]["rules"], json!(["redo"]));
}
