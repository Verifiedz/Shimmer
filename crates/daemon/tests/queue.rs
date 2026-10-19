//! The queue end to end, over the socket: lane ordering, the failure guarantee (§11.1), and
//! priority promotion with its confirmation round trip (§6.2, `docs/protocol.md`).
//!
//! Lane `slow` (max_concurrent = 1) is the fixture. A `demo.spin` task holds its one slot so
//! everything enqueued after it has to wait, which makes the waiting list observable.

mod common;

use std::time::Duration;

use common::*;
use serde_json::{json, Value};
use shimmer_core::{ErrorCode, Event};
use shimmer_proto::ServerFrame;

const OVERRIDE: fn() -> Value = || json!({"priority": "override"});

fn confirm(version: u64) -> Value {
    json!({"priority": "override", "confirm": true, "queue_version": version})
}

/// Task ids waiting in lane `slow`, next-up first, and the lane's `queue_version`.
async fn waiting(c: &mut Client) -> (Vec<String>, u64) {
    let list = c.call("queue.list", json!({"lane": "slow"})).await.unwrap();
    let lane = &list["lanes"][0];
    let ids = lane["queued"].as_array().unwrap().iter().map(|t| t["task_id"].as_str().unwrap().to_owned()).collect();
    (ids, lane["queue_version"].as_u64().unwrap())
}

async fn quick(c: &mut Client) -> String {
    task_id(&c.call("demo.quick", json!({})).await.unwrap())
}

/// Start a `demo.spin` and wait until it holds the slot, so later tasks queue behind it.
async fn hold_the_lane(c: &mut Client) -> String {
    let id = task_id(&c.call("demo.spin", json!({})).await.unwrap());
    assert_eq!(c.event("queue.task.started").await.payload["task_id"], id.as_str());
    id
}

/// Every event, in stream order, up to and including `topic` for task `id`.
async fn until(c: &mut Client, topic: &str, id: &str) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        let ev = match c.events.pop_front() {
            Some(e) => e,
            None => match c.recv().await.expect("connection closed") {
                ServerFrame::Event { event, .. } => event,
                other => panic!("unexpected frame {other:?}"),
            },
        };
        let done = ev.topic == topic && ev.payload["task_id"] == id;
        seen.push(ev);
        if done {
            return seen;
        }
    }
}

fn index_of(events: &[Event], topic: &str, id: &str) -> usize {
    events
        .iter()
        .position(|e| e.topic == topic && e.payload["task_id"] == id)
        .unwrap_or_else(|| panic!("no {topic} for {id} in {:?}", events.iter().map(|e| &e.topic).collect::<Vec<_>>()))
}

fn ids_of(v: &Value) -> Vec<&str> {
    v.as_array().unwrap().iter().map(|t| t["task_id"].as_str().unwrap()).collect()
}

// ------------------------------------------------------------------ §11.1

/// Not "both events eventually appear": the failed task's slot is released, and its failure
/// reported, *before* the next task in the lane starts.
#[tokio::test]
async fn a_failed_tasks_slot_frees_before_the_next_task_starts() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.task.*"]).await;

    let failing = task_id(&c.call("demo.slow_fail", json!({})).await.unwrap());
    let h = c.call("demo.quick", json!({})).await.unwrap();
    assert_eq!(h["position"], 0, "the failing task still holds the only slot, so this one waits");
    let next = task_id(&h);

    let seen = until(&mut c, "queue.task.finished", &next).await;
    let started_failing = index_of(&seen, "queue.task.started", &failing);
    let failed = index_of(&seen, "queue.task.failed", &failing);
    let started_next = index_of(&seen, "queue.task.started", &next);
    let finished_next = index_of(&seen, "queue.task.finished", &next);

    assert!(started_failing < failed, "it ran, then failed");
    assert!(failed < started_next, "the failure is reported before the next task takes the slot");
    assert!(started_next < finished_next);
    assert_eq!(seen[failed].payload["lane"], "slow");
    assert_eq!(seen[failed].payload["attempts"], 1, "and the queue did not retry it");
    assert!(
        seen.iter().all(|e| e.topic != "queue.task.started"
            || e.payload["task_id"] != failing.as_str()
            || index_of(&seen, "queue.task.started", &failing) == started_failing),
        "the failed task started exactly once"
    );

    let t = c.call("queue.task", json!({"task_id": failing})).await.unwrap();
    assert_eq!(t["status"], "failed");
    let (waiting, _) = waiting(&mut c).await;
    assert!(waiting.is_empty());
    stop(d).await;
}

// ------------------------------------------------------------------ §6.2 ordering

/// §6.2 (as reworded): `Scheduled` and `Normal` share one arrival-order queue. Tier records
/// where a task came from, not a rank. So a scheduled task lands *between* two manual ones,
/// exactly where it arrived, and runs there.
#[tokio::test]
async fn scheduled_and_normal_tasks_run_in_arrival_order() {
    let env = Env::fake_clock();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.task.*", "scheduler.trigger.fired"]).await;
    let spin = hold_the_lane(&mut c).await;

    let first = quick(&mut c).await;
    c.call("scheduler.add", json!({"schedule": {"every": 3600}, "op": "demo.quick", "params": {}, "catch_up": "skip"}))
        .await
        .unwrap();
    env.clock.advance(Duration::from_secs(3600));
    let fired = c.event("scheduler.trigger.fired").await;
    let scheduled = fired.payload["task_id"].as_str().unwrap().to_owned();
    let last = quick(&mut c).await;

    let (order, _) = waiting(&mut c).await;
    assert_eq!(order, [first.clone(), scheduled.clone(), last.clone()], "arrival order, neither tier outranks");
    let view = c.call("queue.task", json!({"task_id": scheduled})).await.unwrap();
    assert_eq!(view["priority"], "scheduled", "provenance is still recorded");

    c.call("queue.cancel", json!({"task_id": spin})).await.unwrap();
    let seen = until(&mut c, "queue.task.finished", &last).await;
    let started: Vec<&str> = seen
        .iter()
        .filter(|e| e.topic == "queue.task.started" && e.payload["task_id"] != spin.as_str())
        .map(|e| e.payload["task_id"].as_str().unwrap())
        .collect();
    assert_eq!(started, [first.as_str(), scheduled.as_str(), last.as_str()]);
    stop(d).await;
}

// ------------------------------------------------------------------ §6.2 promotion

#[tokio::test]
async fn promotion_is_refused_until_confirmed_and_the_refusal_changes_nothing() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.task.*"]).await;
    let spin = hold_the_lane(&mut c).await;
    let (a, b) = (quick(&mut c).await, quick(&mut c).await);
    let (before, version) = waiting(&mut c).await;
    assert_eq!(before, [a.clone(), b.clone()]);

    // 1. Ask to jump the lane. Refused, with what it would cost.
    let refused = c.call_with("demo.quick", json!({}), Some(OVERRIDE())).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::ConfirmationRequired);
    let detail = refused.detail.expect("detail carries the picture the user must see");
    assert_eq!(detail["lane"], "slow");
    assert_eq!(detail["queue_version"], version);
    assert_eq!(ids_of(&detail["would_displace"]), [a.as_str(), b.as_str()]);
    assert_eq!(detail["would_displace"][0]["op"], "demo.quick");
    assert_eq!(detail["would_displace"][0]["priority"], "normal");
    assert_eq!(waiting(&mut c).await, (before.clone(), version), "all or nothing: nothing was enqueued or bumped");

    // 2. Confirmed, echoing the version the user was shown.
    let granted = c.call_with("demo.quick", json!({}), Some(confirm(version))).await.unwrap();
    assert_eq!(granted["position"], 0);
    let promoted_id = task_id(&granted);
    let (after, _) = waiting(&mut c).await;
    assert_eq!(after, [promoted_id.clone(), a.clone(), b.clone()]);

    // 3. Audited, and recorded on the task.
    let ev = c.event("queue.task.promoted").await;
    assert_eq!(ev.payload["task_id"], promoted_id.as_str());
    assert_eq!(ev.payload["displaced"], json!([a, b]));
    let view = c.call("queue.task", json!({"task_id": promoted_id})).await.unwrap();
    assert_eq!(view["priority"]["overridden"]["by"], "user");

    // 4. It really does run first.
    c.call("queue.cancel", json!({"task_id": spin})).await.unwrap();
    assert_eq!(c.event("queue.task.started").await.payload["task_id"], promoted_id.as_str());
    stop(d).await;
}

#[tokio::test]
async fn a_stale_queue_version_is_refused_again_with_the_fresh_picture() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.task.*"]).await;
    hold_the_lane(&mut c).await;
    let a = quick(&mut c).await;

    let shown = c.call_with("demo.quick", json!({}), Some(OVERRIDE())).await.unwrap_err().detail.unwrap();
    let stale = shown["queue_version"].as_u64().unwrap();
    assert_eq!(ids_of(&shown["would_displace"]), [a.as_str()]);

    // The lane changes while the user is reading the prompt.
    let b = quick(&mut c).await;

    let again = c.call_with("demo.quick", json!({}), Some(confirm(stale))).await.unwrap_err();
    assert_eq!(again.code, ErrorCode::ConfirmationRequired, "never act on a picture the user did not see");
    let fresh = again.detail.unwrap();
    assert!(fresh["queue_version"].as_u64().unwrap() > stale);
    assert_eq!(ids_of(&fresh["would_displace"]), [a.as_str(), b.as_str()]);

    let ok = c.call_with("demo.quick", json!({}), Some(confirm(fresh["queue_version"].as_u64().unwrap()))).await;
    assert!(ok.is_ok(), "{ok:?}");
    stop(d).await;
}

#[tokio::test]
async fn confirm_without_the_version_shown_is_not_a_confirmation() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.task.*"]).await;
    hold_the_lane(&mut c).await;
    quick(&mut c).await;

    let bare = json!({"priority": "override", "confirm": true});
    let e = c.call_with("demo.quick", json!({}), Some(bare)).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::ConfirmationRequired);
    stop(d).await;
}

#[tokio::test]
async fn promotion_that_displaces_nobody_needs_no_confirmation_but_is_still_audited() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.task.*"]).await;
    hold_the_lane(&mut c).await;

    let granted = c.call_with("demo.quick", json!({}), Some(OVERRIDE())).await.unwrap();
    let id = task_id(&granted);
    assert_eq!(granted["position"], 0);
    let ev = c.event("queue.task.promoted").await;
    assert_eq!(ev.payload["task_id"], id.as_str());
    assert_eq!(ev.payload["displaced"], json!([]));
    stop(d).await;
}

#[tokio::test]
async fn a_second_promotion_queues_behind_the_first_and_only_ordinary_tasks_are_displaced() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.task.*"]).await;
    hold_the_lane(&mut c).await;
    let a = quick(&mut c).await;

    let v = waiting(&mut c).await.1;
    let p1 = task_id(&c.call_with("demo.quick", json!({}), Some(confirm(v))).await.unwrap());

    let shown = c.call_with("demo.quick", json!({}), Some(OVERRIDE())).await.unwrap_err().detail.unwrap();
    assert_eq!(ids_of(&shown["would_displace"]), [a.as_str()], "the earlier promotion keeps its turn");
    let p2 = task_id(
        &c.call_with("demo.quick", json!({}), Some(confirm(shown["queue_version"].as_u64().unwrap()))).await.unwrap(),
    );

    assert_eq!(waiting(&mut c).await.0, [p1, p2, a]);
    stop(d).await;
}

// ------------------------------------------------------------------ §6.2 reorder (ADR 0006)

#[tokio::test]
async fn queue_reorder_moves_a_waiting_task_over_the_real_socket() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.task.*"]).await;
    hold_the_lane(&mut c).await;
    let (a, b) = (quick(&mut c).await, quick(&mut c).await);
    let (before, version) = waiting(&mut c).await;
    assert_eq!(before, [a.clone(), b.clone()]);

    let moved = c
        .call("queue.reorder", json!({"lane": "slow", "task_id": b, "before": a, "queue_version": version}))
        .await
        .unwrap();
    let new_version = moved["queue_version"].as_u64().unwrap();
    assert!(new_version > version);

    let (after, _) = waiting(&mut c).await;
    assert_eq!(after, [b.clone(), a.clone()], "b now sits ahead of a");

    let ev = c.event("queue.task.reordered").await;
    assert_eq!(ev.payload["task_id"], b.as_str());
    assert_eq!(ev.payload["lane"], "slow");
    assert_eq!(ev.payload["before"], a.as_str());
    assert_eq!(ev.payload["queue_version"], new_version);
    stop(d).await;
}

#[tokio::test]
async fn a_stale_queue_version_on_reorder_is_a_conflict_and_changes_nothing() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.task.*"]).await;
    hold_the_lane(&mut c).await;
    let a = quick(&mut c).await;
    let (before, stale) = waiting(&mut c).await;

    // The lane changes while the version is stale.
    let b = quick(&mut c).await;

    let err = c
        .call("queue.reorder", json!({"lane": "slow", "task_id": b, "before": a, "queue_version": stale}))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert!(err.detail.unwrap()["queue_version"].as_u64().unwrap() > stale);

    let (after, _) = waiting(&mut c).await;
    assert_eq!(after, [before[0].clone(), b], "the refused reorder changed nothing");
    stop(d).await;
}

#[tokio::test]
async fn the_running_task_cannot_be_reordered_over_the_real_socket() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.task.*"]).await;
    let spin = hold_the_lane(&mut c).await;
    let a = quick(&mut c).await;
    let (_, version) = waiting(&mut c).await;

    let err = c
        .call("queue.reorder", json!({"lane": "slow", "task_id": spin, "before": a, "queue_version": version}))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound, "the running task is not in the waiting list");
    stop(d).await;
}

#[tokio::test]
async fn a_noop_reorder_changes_nothing_and_emits_no_event() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.task.*"]).await;
    hold_the_lane(&mut c).await;
    let (a, b) = (quick(&mut c).await, quick(&mut c).await);
    let (before, version) = waiting(&mut c).await;
    assert_eq!(before, [a.clone(), b.clone()]);

    // `a` is already directly before `b`, so this asks for the order it's already in.
    let moved = c
        .call("queue.reorder", json!({"lane": "slow", "task_id": a, "before": b, "queue_version": version}))
        .await
        .unwrap();
    assert_eq!(moved["queue_version"].as_u64().unwrap(), version, "nothing moved, so the version is unchanged");

    // Enqueue a third task and look at every event up to its `enqueued` — the no-op above
    // must not have snuck a `queue.task.reordered` in among them.
    let c_id = quick(&mut c).await;
    let seen = until(&mut c, "queue.task.enqueued", &c_id).await;
    assert!(
        seen.iter().all(|e| e.topic != "queue.task.reordered"),
        "a no-op reorder must not emit queue.task.reordered"
    );

    let (after, _) = waiting(&mut c).await;
    assert_eq!(after, [a, b, c_id], "order unchanged by the no-op");
    stop(d).await;
}

#[tokio::test]
async fn inline_ops_ignore_queue_control() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.call_with("demo.echo", json!({}), Some(OVERRIDE())).await.unwrap();
    stop(d).await;
}

// ------------------------------------------------------------------ §11.3 fallback (ADR 0005)

/// Every event, in stream order, up to and including the first with this topic.
async fn until_topic(c: &mut Client, topic: &str) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        let ev = match c.events.pop_front() {
            Some(e) => e,
            None => match c.recv().await.expect("connection closed") {
                ServerFrame::Event { event, .. } => event,
                other => panic!("unexpected frame {other:?}"),
            },
        };
        let done = ev.topic == topic;
        seen.push(ev);
        if done {
            return seen;
        }
    }
}

/// A trigger that fires `op` an hour from now, carrying a `notify.*` fallback.
async fn trigger_with_fallback(env: &Env, c: &mut Client, op: &str, fallback_op: &str) {
    let fallback =
        json!({"notify": {"fallback_op": fallback_op, "params": {"message": "couldn't run"}, "priority": "high"}});
    c.call(
        "scheduler.add",
        json!({"schedule": {"every": 3600}, "op": op, "params": {}, "catch_up": "skip", "fallback": fallback}),
    )
    .await
    .unwrap();
    env.clock.advance(Duration::from_secs(3600));
}

/// Let anything that is going to happen happen, then report what arrived on this connection.
async fn settle(c: &mut Client) {
    tokio::time::sleep(Duration::from_millis(150)).await;
    c.call("core.ping", json!({})).await.unwrap();
}

fn fallback_events(c: &Client) -> Vec<&str> {
    c.events.iter().filter(|e| e.topic.starts_with("queue.fallback.")).map(|e| e.topic.as_str()).collect()
}

#[tokio::test]
async fn a_structural_failure_enqueues_the_fallback_in_the_notify_lane() {
    let env = Env::fake_clock();
    let d = env.start_with_notify().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.*"]).await;
    trigger_with_fallback(&env, &mut c, "demo.fail", "notify.send").await;

    let seen = until_topic(&mut c, "queue.fallback.enqueued").await;
    let enqueued = seen.last().unwrap();
    let primary = enqueued.payload["task_id"].as_str().unwrap().to_owned();
    let fallback = enqueued.payload["fallback_task_id"].as_str().unwrap().to_owned();
    assert_eq!(enqueued.payload["lane"], "notify");
    assert_eq!(enqueued.payload["op"], "notify.send");
    assert!(
        index_of(&seen, "queue.task.failed", &primary) < seen.len() - 1,
        "the failure is reported before the fallback is enqueued"
    );
    let failed = &seen[index_of(&seen, "queue.task.failed", &primary)];
    assert_eq!(failed.payload["code"], "unavailable");
    assert_eq!(failed.payload["lane"], "slow", "the primary ran in its own lane");

    // The fallback is an ordinary task, configured exactly as the trigger declared it.
    let done = until(&mut c, "queue.task.finished", &fallback).await;
    let finished = done.last().unwrap();
    assert_eq!(
        finished.payload["result"],
        json!({"message": "couldn't run", "priority": "high", "failure_reason": "upstream down",
               "failure_code": "unavailable", "failure_detail": null}),
        "as configured, plus what the queue stamps"
    );
    let view = c.call("queue.task", json!({"task_id": fallback})).await.unwrap();
    assert_eq!(view["lane"], "notify");
    assert_eq!(view["priority"], "normal", "a fallback takes its turn and cannot promote itself");
    assert_eq!(view["origin"], json!({"type": "module", "id": "queue"}));
    stop(d).await;
}

/// The notice has to say *what* went wrong, not just that something did: the op receives the
/// dirty reason, the failed step and the log path, taken from the same error that produced
/// `queue.task.failed` (ADR 0005).
#[tokio::test]
async fn a_workspace_dirty_fallback_carries_the_dirty_reason_and_log_path() {
    let env = Env::fake_clock();
    let d = env.start_with_notify().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.*"]).await;
    trigger_with_fallback(&env, &mut c, "demo.dirty", "notify.send").await;

    let seen = until_topic(&mut c, "queue.fallback.enqueued").await;
    let primary = seen.last().unwrap().payload["task_id"].as_str().unwrap().to_owned();
    let fallback = seen.last().unwrap().payload["fallback_task_id"].as_str().unwrap().to_owned();
    let failed = seen[index_of(&seen, "queue.task.failed", &primary)].clone();
    let params = until(&mut c, "queue.task.finished", &fallback).await.pop().unwrap().payload["result"].clone();

    assert_eq!(params["failure_code"], "workspace_dirty");
    assert_eq!(params["failure_reason"], "workspace 'deep-work' failed setup and was not cleaned up");
    assert_eq!(params["failure_detail"]["log"], "logs/deep-work-01JD2T.log");
    assert_eq!(params["failure_detail"]["failed_step"], "3/7 tmux-session");
    assert_eq!(params["failure_detail"]["workspace"], "deep-work");
    assert_eq!(params["failure_detail"]["has_cleanup_script"], true);
    assert_eq!(params["message"], "couldn't run", "the configured params are still there");
    assert_eq!(params["priority"], "high");

    // Same source as the failure event, not a second copy that can drift from it.
    assert_eq!(params["failure_reason"], failed.payload["error"]);
    assert_eq!(params["failure_code"], failed.payload["code"]);
    assert_eq!(params["failure_detail"], failed.payload["detail"]);
    stop(d).await;
}

#[tokio::test]
async fn a_failing_fallback_ends_the_chain_and_stalls_nothing() {
    let env = Env::fake_clock();
    let d = env.start_with_notify().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.*"]).await;
    trigger_with_fallback(&env, &mut c, "demo.fail", "notify.fail").await;

    let seen = until_topic(&mut c, "queue.fallback.enqueued").await;
    let fallback = seen.last().unwrap().payload["fallback_task_id"].as_str().unwrap().to_owned();
    let done = until(&mut c, "queue.task.failed", &fallback).await;
    assert_eq!(done.last().unwrap().payload["code"], "unavailable", "the fallback itself failed structurally");

    settle(&mut c).await;
    assert!(fallback_events(&c).is_empty(), "no fallback of a fallback: {:?}", fallback_events(&c));
    let ok = c.call("notify.send", json!({"message": "still working"})).await.unwrap();
    let id = task_id(&ok);
    assert_eq!(until(&mut c, "queue.task.finished", &id).await.last().unwrap().payload["task_id"], id.as_str());
    stop(d).await;
}

#[tokio::test]
async fn a_failure_that_is_not_structural_gets_no_fallback() {
    let env = Env::fake_clock();
    let d = env.start_with_notify().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.*"]).await;
    trigger_with_fallback(&env, &mut c, "demo.bad", "notify.send").await;

    let seen = until_topic(&mut c, "queue.task.failed").await;
    assert_eq!(seen.last().unwrap().payload["code"], "module_error");
    settle(&mut c).await;
    assert!(fallback_events(&c).is_empty(), "{:?}", fallback_events(&c));
    assert!(c.events.iter().all(|e| e.payload["op"] != "notify.send"), "nothing was enqueued in the notify lane");
    stop(d).await;
}

#[tokio::test]
async fn a_cancelled_task_gets_no_fallback() {
    let env = Env::fake_clock();
    let d = env.start_with_notify().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.*", "scheduler.trigger.fired"]).await;
    trigger_with_fallback(&env, &mut c, "demo.spin", "notify.send").await;

    let spin = c.event("scheduler.trigger.fired").await.payload["task_id"].as_str().unwrap().to_owned();
    assert_eq!(c.event("queue.task.started").await.payload["task_id"], spin.as_str());
    c.call("queue.cancel", json!({"task_id": spin})).await.unwrap();
    until(&mut c, "queue.task.cancelled", &spin).await;

    settle(&mut c).await;
    assert!(fallback_events(&c).is_empty(), "{:?}", fallback_events(&c));
    stop(d).await;
}

/// ADR 0005 checks the namespace at creation, not that the op exists: the notify module may
/// be absent or broken later. Then the failure is one more event, and the lane carries on.
#[tokio::test]
async fn a_fallback_that_cannot_be_enqueued_is_reported_and_never_stalls_the_lane() {
    let env = Env::fake_clock();
    let d = env.start_with_notify().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.*"]).await;
    trigger_with_fallback(&env, &mut c, "demo.fail", "notify.ghost").await;

    let seen = until_topic(&mut c, "queue.fallback.failed").await;
    let ev = seen.last().unwrap();
    assert_eq!(ev.payload["op"], "notify.ghost");
    assert_eq!(ev.payload["code"], "unknown_op");

    let next = quick(&mut c).await;
    assert_eq!(until(&mut c, "queue.task.finished", &next).await.last().unwrap().payload["task_id"], next.as_str());
    stop(d).await;
}

/// ADR 0035 §5.1: what automations follows a chain of rules by.
#[tokio::test]
async fn events_a_queued_task_emits_carry_it_as_their_cause() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["demo.item.saved"]).await;
    let task = task_id(&c.call("demo.put_queued", json!({})).await.unwrap());
    for _ in 0..2 {
        let ev = c.event("demo.item.saved").await;
        assert_eq!(ev.cause.map(|t| t.to_string()), Some(task.clone()), "{:?}", ev.payload);
    }
    c.call("demo.put", json!({"key": "k", "val": "v"})).await.unwrap();
    assert_eq!(c.event("demo.item.saved").await.cause, None, "answered inline: no task");
    stop(d).await;
}

/// #251's review: the queue's own events about a task carry that task as their cause, so a rule
/// on `queue.task.*` set off by its own action continues the chain and the loop guard stops it.
#[tokio::test]
async fn the_queues_events_about_a_task_carry_it_as_their_cause() {
    let env = Env::new();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["queue.task.*"]).await;
    let task = task_id(&c.call("demo.put_queued", json!({})).await.unwrap());
    for topic in ["queue.task.enqueued", "queue.task.started", "queue.task.finished"] {
        let ev = c.event(topic).await;
        assert_eq!(ev.payload["task_id"], task.as_str(), "{topic}");
        assert_eq!(ev.cause.map(|t| t.to_string()), Some(task.clone()), "{topic}: {:?}", ev.payload);
    }
    stop(d).await;
}
