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
    call(&home, "automations.new", json!({"id": "fill-url", "text": rule}));

    call(&home, "records.update", json!({"collection": "leetcode", "id": "two-sum", "fields": {"difficulty": "easy"}}));
    let status = until(&home, "automations.status", json!({}), |s| !s["stopped"].as_array().unwrap().is_empty());
    assert_eq!(status["stopped"][0]["reason"], "cycle", "{status}");

    let record = call(&home, "records.show", json!({"collection": "leetcode", "id": "two-sum"}));
    assert_eq!(record["url"], "https://x.test/two-sum", "the rule ran: {record}");
    let history = call(&home, "automations.history", json!({"id": "fill-url"}))["history"].clone();
    let kinds: Vec<&str> = history.as_array().unwrap().iter().filter_map(|e| e["kind"].as_str()).collect();
    assert_eq!(kinds, ["stopped", "fired"], "fired once, then stopped: {history}");
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
    call(&home, "automations.new", json!({"id": "redo", "text": rule}));
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
