//! Scheduler end to end: a fake clock advanced past due times, observed through the socket
//! (`scheduler.*` ops, the event stream) and the files it leaves behind.

mod common;

use std::time::Duration;

use common::*;
use serde_json::{json, Value};
use shimmer_core::{CatchUp, ErrorCode, Event, Schedule, TriggerId, TriggerSpec};

const DAY: u64 = 86_400;

fn advance(env: &Env, secs: u64) {
    env.clock.advance(Duration::from_secs(secs));
}

fn iso(secs_after_t0: u64) -> String {
    (t0() + chrono::Duration::seconds(secs_after_t0 as i64)).format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

async fn add(c: &mut Client, schedule: Value, op: &str, catch_up: &str) -> String {
    let r = c
        .call("scheduler.add", json!({"schedule": schedule, "op": op, "params": {"x": 1}, "catch_up": catch_up}))
        .await
        .unwrap();
    r["trigger_id"].as_str().unwrap().to_owned()
}

fn fb(op: &str) -> Value {
    json!({"notify": {"fallback_op": op, "params": {"message": "didn't run"}, "priority": "high"}})
}

async fn triggers(c: &mut Client) -> Vec<Value> {
    c.call("scheduler.list", json!({})).await.unwrap()["triggers"].as_array().unwrap().clone()
}

async fn trigger(c: &mut Client, id: &str) -> Option<Value> {
    triggers(c).await.into_iter().find(|t| t["id"] == id)
}

/// Wait until the scheduler has processed the clock: the trigger's `next_due` equals `want`.
async fn until_next_due(c: &mut Client, id: &str, want: Value) {
    for _ in 0..300 {
        if trigger(c, id).await.is_some_and(|t| t["next_due"] == want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("trigger {id} never reached next_due {want}: {:?}", trigger(c, id).await);
}

/// Let any further ticks happen, then report everything queued up for this connection.
async fn settle(c: &mut Client) {
    tokio::time::sleep(Duration::from_millis(150)).await;
    c.call("core.ping", json!({})).await.unwrap();
}

fn count(c: &Client, topic: &str, trigger: &str) -> usize {
    c.events.iter().filter(|e| e.topic == topic && e.payload["trigger_id"] == trigger).count()
}

fn fired_times(events: &[Event]) -> Vec<String> {
    events.iter().map(|e| e.payload["scheduled_for"].as_str().unwrap().to_owned()).collect()
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

// ------------------------------------------------------------------ the M2 milestone

#[tokio::test]
async fn three_days_on_a_fake_clock_fire_the_right_number_of_times() {
    let env = Env::fake_clock();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["scheduler.*", "queue.task.finished"]).await;

    let skip = add(&mut c, json!({"every": DAY}), "demo.echo", "skip").await;
    let once = add(&mut c, json!({"every": DAY}), "demo.echo", "run_once").await;
    let fill = add(&mut c, json!({"every": DAY}), "demo.echo", "backfill").await;

    // Laptop asleep: three periods pass, and it wakes an hour after the third came due.
    advance(&env, 3 * DAY + 3600);
    until_next_due(&mut c, &fill, json!(iso(4 * DAY))).await;
    settle(&mut c).await;

    assert_eq!(count(&c, "scheduler.trigger.fired", &skip), 0, "skip drops what was missed");
    assert_eq!(count(&c, "scheduler.trigger.missed", &skip), 1);
    assert_eq!(count(&c, "scheduler.trigger.fired", &once), 1, "run_once fires once however many were missed");
    assert_eq!(count(&c, "scheduler.trigger.fired", &fill), 3, "backfill fires once per missed period");

    let fired: Vec<Event> = c
        .events
        .iter()
        .filter(|e| e.topic == "scheduler.trigger.fired" && e.payload["trigger_id"] == fill.as_str())
        .cloned()
        .collect();
    assert_eq!(fired_times(&fired), [iso(DAY), iso(2 * DAY), iso(3 * DAY)], "historical timestamps, in order");
    stop(d).await;
}

#[tokio::test]
async fn a_firing_that_is_on_time_runs_under_every_policy() {
    let env = Env::fake_clock();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["scheduler.*"]).await;
    let ids = [
        add(&mut c, json!({"every": DAY}), "demo.echo", "skip").await,
        add(&mut c, json!({"every": DAY}), "demo.echo", "run_once").await,
        add(&mut c, json!({"every": DAY}), "demo.echo", "backfill").await,
    ];
    advance(&env, DAY + 5);
    until_next_due(&mut c, &ids[0], json!(iso(2 * DAY))).await;
    settle(&mut c).await;
    for id in &ids {
        assert_eq!(count(&c, "scheduler.trigger.fired", id), 1, "{id}");
        assert_eq!(count(&c, "scheduler.trigger.missed", id), 0, "{id}");
    }
    stop(d).await;
}

// ------------------------------------------------------------------ what a firing is

#[tokio::test]
async fn a_firing_enqueues_a_scheduled_task_with_scheduler_origin_and_stamped_params() {
    let env = Env::fake_clock();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["scheduler.trigger.fired", "queue.task.finished"]).await;
    let id = add(&mut c, json!({"every": 3600}), "demo.echo", "skip").await;

    advance(&env, 3600);
    let fired = c.event("scheduler.trigger.fired").await;
    assert_eq!(fired.source.as_str(), "scheduler");
    assert_eq!(fired.payload["scheduled_for"], iso(3600));
    assert_eq!(fired.payload["op"], "demo.echo");

    let task_id = fired.payload["task_id"].as_str().unwrap();
    let finished = c.event("queue.task.finished").await;
    assert_eq!(finished.payload["task_id"], task_id);
    assert_eq!(
        finished.payload["result"],
        json!({"x": 1, "scheduled_for": iso(3600)}),
        "the op receives its params plus the period it is for"
    );
    let t = c.call("queue.task", json!({"task_id": task_id})).await.unwrap();
    assert_eq!(t["priority"], "scheduled");
    assert_eq!(t["origin"], json!({"type": "scheduler", "trigger_id": id}));
    stop(d).await;
}

#[tokio::test]
async fn a_trigger_never_overlaps_itself() {
    let env = Env::fake_clock();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["scheduler.trigger.*", "queue.task.started", "queue.task.cancelled"]).await;
    let id = add(&mut c, json!({"every": DAY}), "demo.spin", "skip").await; // runs for seconds

    advance(&env, DAY);
    let first = c.event("scheduler.trigger.fired").await;
    c.event("queue.task.started").await;

    advance(&env, DAY); // previous task is still running
    let skipped = c.event("scheduler.trigger.skipped").await;
    assert_eq!(skipped.payload["reason"], "overlap");
    assert_eq!(skipped.payload["pending"], json!([first.payload["task_id"]]));
    settle(&mut c).await;
    assert_eq!(count(&c, "scheduler.trigger.fired", &id), 0, "no second task while the first is pending");

    // Once the earlier task is gone, the trigger fires again.
    c.call("queue.cancel", json!({"task_id": first.payload["task_id"]})).await.unwrap();
    // Cancelling a running task is cooperative; wait until it has actually gone.
    c.event("queue.task.cancelled").await;
    advance(&env, DAY);
    assert_eq!(c.event("scheduler.trigger.fired").await.payload["scheduled_for"], iso(3 * DAY));
    stop(d).await;
}

#[tokio::test]
async fn a_failing_task_does_not_stop_its_trigger() {
    let env = Env::fake_clock();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["scheduler.trigger.fired", "queue.task.failed"]).await;
    add(&mut c, json!({"every": 3600}), "demo.fail", "skip").await;
    for hour in 1..=3 {
        advance(&env, 3600);
        assert_eq!(c.event("scheduler.trigger.fired").await.payload["scheduled_for"], iso(hour * 3600));
        c.event("queue.task.failed").await;
    }
    stop(d).await;
}

// ------------------------------------------------------------------ cron

#[tokio::test]
async fn cron_monday_means_monday() {
    let env = Env::fake_clock(); // Monday 2026-09-21 08:00 UTC
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["scheduler.trigger.fired"]).await;
    let id = add(&mut c, json!({"cron": "0 9 * * 1"}), "demo.echo", "skip").await;
    assert_eq!(trigger(&mut c, &id).await.unwrap()["next_due"], "2026-09-21T09:00:00Z");

    advance(&env, 3600 + 10);
    assert_eq!(c.event("scheduler.trigger.fired").await.payload["scheduled_for"], "2026-09-21T09:00:00Z");
    until_next_due(&mut c, &id, json!("2026-09-28T09:00:00Z")).await;
    stop(d).await;
}

// ------------------------------------------------------------------ once

#[tokio::test]
async fn once_fires_once_and_stays_finished_across_restarts() {
    let env = Env::fake_clock();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["scheduler.trigger.fired"]).await;
    let id = add(&mut c, json!({"once": iso(2 * 3600)}), "demo.echo", "skip").await;

    advance(&env, 2 * 3600 + 10);
    c.event("scheduler.trigger.fired").await;
    until_next_due(&mut c, &id, Value::Null).await;
    assert_eq!(trigger(&mut c, &id).await.unwrap()["done"], true);
    drop(c);
    stop(d).await;

    let d = env.start().await;
    advance(&env, DAY);
    let mut c = Client::connect(&env.sock).await;
    settle(&mut c).await;
    assert_eq!(trigger(&mut c, &id).await.unwrap()["done"], true);
    assert_eq!(logged(&env, "scheduler.trigger.fired").len(), 1, "a finished once never fires again");
    stop(d).await;
}

#[tokio::test]
async fn a_once_already_past_follows_its_catch_up_policy() {
    let env = Env::fake_clock();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["scheduler.trigger.*"]).await;
    // Both were due an hour before they were created.
    let ran = add(&mut c, json!({"once": "2026-09-21T07:00:00Z"}), "demo.echo", "run_once").await;
    let never = add(&mut c, json!({"once": "2026-09-21T07:00:00Z"}), "demo.echo", "skip").await;
    settle(&mut c).await;
    assert_eq!(count(&c, "scheduler.trigger.fired", &ran), 1);
    assert_eq!(count(&c, "scheduler.trigger.fired", &never), 0);
    assert_eq!(count(&c, "scheduler.trigger.missed", &never), 1);
    stop(d).await;
}

// ------------------------------------------------------------------ persistence

#[tokio::test]
async fn state_is_on_disk_and_downtime_is_caught_up_on_the_next_start() {
    let env = Env::fake_clock();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["scheduler.trigger.fired"]).await;
    let id = add(&mut c, json!({"every": DAY}), "demo.echo", "backfill").await;

    advance(&env, DAY);
    c.event("scheduler.trigger.fired").await;
    until_next_due(&mut c, &id, json!(iso(2 * DAY))).await;
    let last_run = trigger(&mut c, &id).await.unwrap()["last_run"].clone();
    assert_eq!(last_run, iso(DAY));
    drop(c);
    stop(d).await;

    // The truth is a readable file, not memory.
    let file = env.home.path().join(format!("data/scheduler/triggers/{id}.json"));
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    assert_eq!(on_disk["next_due"], iso(2 * DAY));
    assert_eq!(on_disk["spec"]["catch_up"], "backfill");

    // Restart with no clock movement: nothing re-fires.
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    settle(&mut c).await;
    assert_eq!(logged(&env, "scheduler.trigger.fired").len(), 1);
    assert_eq!(trigger(&mut c, &id).await.unwrap()["last_run"], last_run);
    drop(c);
    stop(d).await;

    // Down for two more days: on start the two missed periods are backfilled, once each.
    advance(&env, 2 * DAY);
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    until_next_due(&mut c, &id, json!(iso(4 * DAY))).await;
    let fired = logged(&env, "scheduler.trigger.fired");
    assert_eq!(fired_times(&fired), [iso(DAY), iso(2 * DAY), iso(3 * DAY)]);
    stop(d).await;
}

#[tokio::test]
async fn an_unreadable_trigger_file_is_ignored_not_fatal() {
    let env = Env::fake_clock();
    let dir = env.home.path().join("data/scheduler/triggers");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hand-edited.json"), "{ this is not json").unwrap();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    assert!(triggers(&mut c).await.is_empty());
    add(&mut c, json!({"every": 60}), "demo.echo", "skip").await;
    assert_eq!(triggers(&mut c).await.len(), 1);
    stop(d).await;
}

// ------------------------------------------------------------------ ops

#[tokio::test]
async fn add_validates_and_catch_up_is_mandatory() {
    let env = Env::fake_clock();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    let base = |f: &dyn Fn(&mut Value)| {
        let mut v = json!({"schedule": {"every": 60}, "op": "demo.echo", "catch_up": "skip"});
        f(&mut v);
        v
    };
    let cases = [
        ("missing catch_up", base(&|v| drop(v.as_object_mut().unwrap().remove("catch_up"))), ErrorCode::InvalidParams),
        ("bad catch_up", base(&|v| v["catch_up"] = json!("sometimes")), ErrorCode::InvalidParams),
        ("unknown field", base(&|v| v["catchup"] = json!("skip")), ErrorCode::InvalidParams),
        ("unknown op", base(&|v| v["op"] = json!("nope.nothing")), ErrorCode::UnknownOp),
        ("unknown lane", base(&|v| v["lane"] = json!("ghost")), ErrorCode::LaneUnknown),
        ("zero interval", base(&|v| v["schedule"] = json!({"every": 0})), ErrorCode::InvalidParams),
        ("bad cron", base(&|v| v["schedule"] = json!({"cron": "not a cron"})), ErrorCode::InvalidParams),
        ("bad schedule shape", base(&|v| v["schedule"] = json!("daily")), ErrorCode::InvalidParams),
        // ADR 0005: a fallback is a `notify.*` op, and a `notify.*` op has none.
        ("fallback outside notify.*", base(&|v| v["fallback"] = fb("demo.echo")), ErrorCode::InvalidParams),
        ("fallback op without a verb", base(&|v| v["fallback"] = fb("notify.")), ErrorCode::InvalidParams),
        ("lookalike namespace", base(&|v| v["fallback"] = fb("notifications.send")), ErrorCode::InvalidParams),
        (
            "notify op declaring a fallback",
            base(&|v| {
                v["op"] = json!("notify.send");
                v["fallback"] = fb("notify.send");
            }),
            ErrorCode::InvalidParams,
        ),
        (
            "the old notify_only shape",
            base(&|v| v["fallback"] = json!({"notify_only": {"message": "x", "priority": "high"}})),
            ErrorCode::InvalidParams,
        ),
    ];
    for (name, params, code) in cases {
        assert_eq!(c.call("scheduler.add", params).await.unwrap_err().code, code, "{name}");
    }
    assert!(triggers(&mut c).await.is_empty(), "rejected adds leave nothing behind");

    // Lane and fallback are accepted and kept.
    let ok = base(&|v| {
        v["lane"] = json!("slow");
        v["fallback"] =
            json!({"notify": {"fallback_op": "notify.send", "params": {"message": "didn't run"}, "priority": "high"}});
    });
    let id = c.call("scheduler.add", ok).await.unwrap()["trigger_id"].as_str().unwrap().to_owned();
    let t = trigger(&mut c, &id).await.unwrap();
    assert_eq!(t["lane"], "slow");
    assert_eq!(t["fallback"]["notify"]["fallback_op"], "notify.send");
    assert_eq!(t["fallback"]["notify"]["priority"], "high");
    assert_eq!(t["source"], json!({"type": "user"}));
    stop(d).await;
}

#[tokio::test]
async fn pause_resume_and_remove() {
    let env = Env::fake_clock();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["scheduler.trigger.*"]).await;
    let id = add(&mut c, json!({"every": DAY}), "demo.echo", "backfill").await;

    assert_eq!(c.call("scheduler.pause", json!({"trigger_id": id})).await.unwrap()["paused"], true);
    advance(&env, 5 * DAY);
    settle(&mut c).await;
    assert_eq!(count(&c, "scheduler.trigger.fired", &id), 0, "paused triggers do not fire");
    let t = trigger(&mut c, &id).await.unwrap();
    assert_eq!((t["paused"].clone(), t["next_due"].clone()), (json!(true), json!(iso(DAY))));

    // Time spent paused is not replayed as missed work, even under `backfill`.
    assert_eq!(c.call("scheduler.resume", json!({"trigger_id": id})).await.unwrap()["paused"], false);
    assert_eq!(trigger(&mut c, &id).await.unwrap()["next_due"], iso(6 * DAY));
    settle(&mut c).await;
    assert_eq!(count(&c, "scheduler.trigger.fired", &id), 0);
    assert_eq!(count(&c, "scheduler.trigger.paused", &id), 1);
    assert_eq!(count(&c, "scheduler.trigger.resumed", &id), 1);

    assert_eq!(c.call("scheduler.remove", json!({"trigger_id": id})).await.unwrap()["removed"], true);
    assert!(trigger(&mut c, &id).await.is_none());
    assert!(!env.home.path().join(format!("data/scheduler/triggers/{id}.json")).exists());
    for op in ["scheduler.remove", "scheduler.pause", "scheduler.resume", "scheduler.get"] {
        assert_eq!(c.call(op, json!({"trigger_id": id})).await.unwrap_err().code, ErrorCode::NotFound, "{op}");
    }
    assert_eq!(c.call("scheduler.frobnicate", json!({})).await.unwrap_err().code, ErrorCode::UnknownOp);
    stop(d).await;
}

/// #76: `scheduler show` used to call `scheduler.list` and pick its one trigger out. `get`
/// is the odd-one-out fix — same shape as a `scheduler.list` entry, or `not_found`.
#[tokio::test]
async fn get_returns_one_trigger_in_the_list_shape_or_not_found() {
    let env = Env::fake_clock();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    let id = add(&mut c, json!({"every": DAY}), "demo.echo", "backfill").await;

    let got = c.call("scheduler.get", json!({"trigger_id": id})).await.unwrap();
    let listed = trigger(&mut c, &id).await.unwrap();
    assert_eq!(got, listed, "scheduler.get must return exactly the shape its scheduler.list entry has");

    assert_eq!(c.call("scheduler.get", json!({"trigger_id": "nope"})).await.unwrap_err().code, ErrorCode::NotFound);
    stop(d).await;
}

// ------------------------------------------------------------------ module-declared triggers

fn nightly(schedule: Schedule) -> TriggerSpec {
    TriggerSpec {
        id: TriggerId::new("demo-nightly"),
        schedule,
        catch_up: CatchUp::Skip,
        op: "demo.echo".into(),
        params: json!({"from": "module"}),
        lane: None,
        fallback: None,
    }
}

#[tokio::test]
async fn module_triggers_are_registered_owned_and_reconciled() {
    let env = Env::fake_clock();
    let ghost = TriggerSpec {
        id: TriggerId::new("ghost"),
        op: "nope.gone".into(),
        ..nightly(Schedule::Every(Duration::from_secs(60)))
    };
    let declared = Demo { triggers: vec![nightly(Schedule::Every(Duration::from_secs(DAY))), ghost] };
    let d = env.start_with(declared).await;
    let mut c = Client::connect(&env.sock).await;
    c.subscribe(&["scheduler.trigger.fired"]).await;

    let all = triggers(&mut c).await;
    assert_eq!(all.len(), 1, "a trigger aimed at a missing op degrades instead of failing startup");
    assert_eq!(all[0]["source"], json!({"type": "module", "id": "demo"}));

    // Users can pause a module's trigger but not delete it; it would only come back.
    assert_eq!(
        c.call("scheduler.remove", json!({"trigger_id": "demo-nightly"})).await.unwrap_err().code,
        ErrorCode::Conflict
    );

    advance(&env, DAY);
    c.event("scheduler.trigger.fired").await;
    until_next_due(&mut c, "demo-nightly", json!(iso(2 * DAY))).await;
    drop(c);
    stop(d).await;

    // Same declaration after restart: state (last_run) survives.
    let d = env.start_with(Demo { triggers: vec![nightly(Schedule::Every(Duration::from_secs(DAY)))] }).await;
    let mut c = Client::connect(&env.sock).await;
    assert_eq!(trigger(&mut c, "demo-nightly").await.unwrap()["last_run"], iso(DAY));
    drop(c);
    stop(d).await;

    // A changed schedule takes effect, keeping last_run.
    let d = env.start_with(Demo { triggers: vec![nightly(Schedule::Every(Duration::from_secs(3600)))] }).await;
    let mut c = Client::connect(&env.sock).await;
    let t = trigger(&mut c, "demo-nightly").await.unwrap();
    assert_eq!(t["schedule"], json!({"every": 3600}));
    assert_eq!(t["last_run"], iso(DAY));
    drop(c);
    stop(d).await;

    // No longer declared: gone, and the removal is on the record.
    let d = env.start_with(Demo::default()).await;
    let mut c = Client::connect(&env.sock).await;
    assert!(triggers(&mut c).await.is_empty());
    assert!(!env.home.path().join("data/scheduler/triggers/demo-nightly.json").exists());
    assert_eq!(logged(&env, "scheduler.trigger.removed").len(), 1);
    stop(d).await;
}

#[tokio::test]
async fn user_triggers_survive_a_module_reshuffle() {
    let env = Env::fake_clock();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    let id = add(&mut c, json!({"every": DAY}), "demo.echo", "skip").await;
    drop(c);
    stop(d).await;
    let d = env.start_with(Demo { triggers: vec![nightly(Schedule::Every(Duration::from_secs(DAY)))] }).await;
    let mut c = Client::connect(&env.sock).await;
    let ids: Vec<_> = triggers(&mut c).await.iter().map(|t| t["id"].as_str().unwrap().to_owned()).collect();
    assert!(ids.contains(&id) && ids.contains(&"demo-nightly".to_owned()), "{ids:?}");
    stop(d).await;
}

#[tokio::test]
async fn module_triggers_with_an_invalid_fallback_are_not_registered() {
    let env = Env::fake_clock();
    let with = |id: &str, op: &str, fallback_op: &str| TriggerSpec {
        id: TriggerId::new(id),
        op: op.into(),
        fallback: serde_json::from_value(fb(fallback_op)).unwrap(),
        ..nightly(Schedule::Every(Duration::from_secs(DAY)))
    };
    let declared = Demo {
        triggers: vec![
            with("good", "demo.echo", "notify.send"),
            with("wrong-namespace", "demo.echo", "demo.echo"),
            with("nested", "notify.send", "notify.send"),
        ],
    };
    let d = env.start_with(declared).await;
    let mut c = Client::connect(&env.sock).await;
    let ids: Vec<_> = triggers(&mut c).await.iter().map(|t| t["id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(ids, ["good"], "a bad declaration degrades like any other, and never stops the daemon");
    stop(d).await;
}

#[tokio::test]
async fn a_local_time_trigger_follows_the_timezone_after_a_restart() {
    // ADR 0031 §2: a `cron_local` schedule is read in `[general] local_timezone`; that is read at
    // start, so after a change of zone the next run moves to the same local time in the new zone.
    let env = Env::fake_clock();
    let config = env.home.path().join("config.toml");
    std::fs::write(&config, "[general]\nlocal_timezone = \"America/New_York\"\n").unwrap();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    let id = add(&mut c, json!({"cron_local": "0 9 * * *"}), "demo.echo", "skip").await;
    let ny = trigger(&mut c, &id).await.unwrap()["next_due"].as_str().unwrap().to_owned();
    assert!(ny.ends_with("13:00:00Z") || ny.ends_with("14:00:00Z"), "9am in New York is 13:00 or 14:00 UTC: {ny}");
    assert_eq!(trigger(&mut c, &id).await.unwrap()["schedule"], json!({"cron_local": "0 9 * * *"}));
    drop(c);
    stop(d).await;

    std::fs::write(&config, "[general]\nlocal_timezone = \"America/Los_Angeles\"\n").unwrap();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    let la = trigger(&mut c, &id).await.unwrap()["next_due"].as_str().unwrap().to_owned();
    let hours = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap();
    assert_eq!((hours(&la) - hours(&ny)).num_hours(), 3, "9am Los Angeles is 3 hours after 9am New York: {ny} → {la}");
    stop(d).await;
}

/// Wait until `topic` has been logged `n` times.
async fn until_logged(env: &Env, topic: &str, n: usize) {
    for _ in 0..300 {
        if logged(env, topic).len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{topic} logged {} times, not {n}", logged(env, topic).len());
}

#[tokio::test]
async fn a_local_time_trigger_missed_while_down_still_catches_up_after_a_restart() {
    // ADR 0031 §2 (#175's review): catch-up decides from the stored next run, so a restart must
    // never move it past runs the daemon missed, or run-once and backfill would act like skip.
    let env = Env::fake_clock();
    let config = env.home.path().join("config.toml");
    std::fs::write(&config, "[general]\nlocal_timezone = \"America/New_York\"\n").unwrap();
    let d = env.start().await;
    let mut c = Client::connect(&env.sock).await;
    let id = add(&mut c, json!({"cron_local": "0 9 * * *"}), "demo.echo", "run_once").await;
    drop(c);
    stop(d).await;

    // Off for three days, same zone: one run on waking, for the runs it missed.
    advance(&env, 3 * DAY);
    let d = env.start().await;
    until_logged(&env, "scheduler.trigger.fired", 1).await;
    let mut c = Client::connect(&env.sock).await;
    settle(&mut c).await;
    assert_eq!(logged(&env, "scheduler.trigger.fired").len(), 1, "same zone: caught up once");
    assert_eq!(logged(&env, "scheduler.trigger.missed").len(), 1);
    let ran = trigger(&mut c, &id).await.unwrap()["last_run"].clone();
    drop(c);
    stop(d).await;

    // Off for three more days, and now in Los Angeles: still caught up, never before last_run.
    advance(&env, 3 * DAY);
    std::fs::write(&config, "[general]\nlocal_timezone = \"America/Los_Angeles\"\n").unwrap();
    let d = env.start().await;
    until_logged(&env, "scheduler.trigger.fired", 2).await;
    let mut c = Client::connect(&env.sock).await;
    settle(&mut c).await;
    assert_eq!(logged(&env, "scheduler.trigger.fired").len(), 2, "zone changed: caught up once more");
    let t = trigger(&mut c, &id).await.unwrap();
    assert!(t["last_run"].as_str() > ran.as_str(), "{t}");
    let file = env.home.path().join(format!("data/scheduler/triggers/{id}.json"));
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    assert_eq!(on_disk["zone"], "America/Los_Angeles", "the zone its next run is in");
    stop(d).await;
}
