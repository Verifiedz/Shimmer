//! The notify module against an in-memory `Ctx` (CLAUDE.md §12 rule 13): the inbox and its ops,
//! delivery to sinks, quiet hours, pause and the summary after them, settings, and the alerts.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use shimmer_core::testing::TestEnv;
use shimmer_core::{Ctx, Error, ErrorCode, Event, Module, ModuleId, Result};
use shimmer_notify::{Message, Notify, Sink};

/// A sink that records what it was given, and fails when told to.
struct Fake {
    id: &'static str,
    fallback: Option<&'static str>,
    fails: Mutex<bool>,
    got: Mutex<Vec<String>>,
}

impl Fake {
    fn new(id: &'static str) -> Arc<Self> {
        Arc::new(Self { id, fallback: None, fails: Mutex::new(false), got: Mutex::new(Vec::new()) })
    }

    fn failing(id: &'static str, fallback: Option<&'static str>) -> Arc<Self> {
        Arc::new(Self { id, fallback, fails: Mutex::new(true), got: Mutex::new(Vec::new()) })
    }

    fn got(&self) -> Vec<String> {
        self.got.lock().unwrap().clone()
    }
}

#[async_trait]
impl Sink for Fake {
    fn id(&self) -> &str {
        self.id
    }

    fn fallback(&self) -> Option<&str> {
        self.fallback
    }

    async fn deliver(&self, message: &Message, _ctx: &Ctx) -> Result<()> {
        self.got.lock().unwrap().push(format!("{}|{}", message.title, message.body));
        if *self.fails.lock().unwrap() {
            Err(Error::unavailable(format!("{} is off", self.id)))
        } else {
            Ok(())
        }
    }
}

/// The test clock starts at midnight UTC on a Friday (2024-10-04).
struct T {
    env: TestEnv,
    notify: Notify,
    desktop: Arc<Fake>,
}

fn setup() -> T {
    let desktop = Fake::new("desktop");
    T { env: TestEnv::new("notify"), notify: Notify::with_sinks(vec![desktop.clone()]), desktop }
}

impl T {
    async fn call(&self, op: &str, params: Value) -> Value {
        self.notify.handle(op, params, &self.env.ctx).await.unwrap_or_else(|e| panic!("{op}: {e}"))
    }

    async fn fails(&self, op: &str, params: Value) -> Error {
        self.notify.handle(op, params, &self.env.ctx).await.expect_err(op)
    }

    async fn send(&self, title: &str) -> String {
        self.call("notify.send", json!({"title": title})).await["id"].as_str().unwrap().to_owned()
    }

    fn topics(&self) -> Vec<String> {
        self.env.backend.events().into_iter().map(|e| e.topic).collect()
    }

    fn files(&self, prefix: &str) -> Vec<String> {
        self.env.ctx.store.list(prefix).unwrap()
    }

    fn enqueued(&self, op: &str) -> usize {
        self.env.queue.requests.lock().unwrap().iter().filter(|r| r.op == op).count()
    }

    fn later(&self, secs: u64) {
        self.env.clock.advance(Duration::from_secs(secs));
    }
}

fn titles(v: &Value) -> Vec<String> {
    v["items"].as_array().unwrap().iter().map(|m| m["title"].as_str().unwrap().to_owned()).collect()
}

#[tokio::test]
async fn a_message_lands_in_the_inbox_then_reaches_every_sink() {
    let t = setup();
    let m = t.call("notify.send", json!({"title": "Standup", "body": "in 15"})).await;
    assert_eq!(
        (m["state"].as_str(), m["priority"].as_str(), m["source"].as_str()),
        (Some("delivered"), Some("normal"), Some("send"))
    );
    assert_eq!(m["delivery"][0]["medium"], "desktop");
    assert_eq!(t.desktop.got(), ["Standup|in 15"]);
    assert_eq!(t.topics(), ["notify.message.received", "notify.message.sent"]);
    let file = t.env.ctx.store.read_string(&format!("inbox/{}.toml", m["id"].as_str().unwrap())).unwrap().unwrap();
    assert!(file.contains("title = \"Standup\"") && file.contains("state = \"delivered\""), "{file}");

    let scheduled = t.call("notify.send", json!({"title": "Stretch", "from": "schedule"})).await;
    assert_eq!(scheduled["source"], "schedule");
}

#[tokio::test]
async fn nothing_is_dropped_when_every_sink_fails() {
    let t = setup();
    let phone = Fake::failing("phone", Some("desktop"));
    let backup = Fake::failing("desktop", None);
    let notify = Notify::with_sinks(vec![phone.clone(), backup.clone()]);
    let m = notify.handle("notify.send", json!({"title": "x"}), &t.env.ctx).await.unwrap();
    assert_eq!(m["state"], "failed", "the task still succeeds: the inbox has it");
    assert_eq!((phone.got().len(), backup.got().len()), (1, 1), "the fallback tried once, never on its own");
    assert_eq!(t.topics(), ["notify.message.received", "notify.message.exhausted"]);

    // No sink at all (before the desktop one): kept as failed too.
    let m = Notify::default().handle("notify.send", json!({"title": "y"}), &t.env.ctx).await.unwrap();
    assert_eq!(m["state"], "failed");
}

#[tokio::test]
async fn a_fallback_is_sent_whatever_it_carries() {
    let t = setup();
    let params = json!({"message": "your 2pm session", "priority": "high", "failure_code": "workspace_dirty",
                        "failure_reason": "site is dirty", "failure_detail": null});
    let m = t.call("notify.send", params).await;
    assert_eq!((m["source"].as_str(), m["priority"].as_str()), (Some("fallback"), Some("high")));
    assert_eq!(t.desktop.got(), ["Scheduled task failed: site is dirty|your 2pm session"]);
    assert!(t.fails("notify.send", json!({"body": "no title"})).await.message.contains("needs a title"));
}

#[tokio::test]
async fn quiet_hours_hold_what_isnt_urgent_then_send_one_summary() {
    let t = setup();
    let q = t.call("notify.quiet", json!({"every_day": "23:00-07:00"})).await;
    assert_eq!((q["quiet"]["every_day"].as_str(), q["quiet_now"].as_bool()), (Some("23:00-07:00"), Some(true)));
    for title in ["one", "two", "three", "four"] {
        assert_eq!(t.call("notify.send", json!({"title": title})).await["state"], "held");
    }
    assert_eq!(t.call("notify.send", json!({"title": "fire", "priority": "high"})).await["state"], "delivered");
    assert_eq!(t.desktop.got(), ["fire|"], "high always goes through quiet hours");
    assert_eq!(t.files("held").len(), 4);
    assert_eq!(t.call("notify.flush", json!({})).await["delivered"], 0, "still quiet");

    t.later(8 * 3600);
    let other = Event::new(ModuleId::new("records"), "records.item.added", json!({}), t.env.clock.now());
    t.notify.on_event(&other, &t.env.ctx).await.unwrap();
    t.notify.on_event(&other, &t.env.ctx).await.unwrap();
    assert_eq!(t.enqueued("notify.flush"), 1, "one flush, however many events");

    assert_eq!(t.call("notify.flush", json!({})).await["delivered"], 4);
    assert_eq!(t.desktop.got()[1], "4 messages while you were away|one; two; three and 1 more");
    assert!(t.files("held").is_empty());
    let before = t.env.backend.events().len();
    assert_eq!(t.call("notify.flush", json!({})).await["delivered"], 0);
    assert_eq!(t.env.backend.events().len(), before, "nothing held: a flush writes nothing");

    let off = t.call("notify.quiet", json!({"off": true})).await;
    assert_eq!(off["quiet"], json!({}));
}

#[tokio::test]
async fn quiet_hours_per_day() {
    let t = setup();
    // Friday 00:00: weekdays' range covers it, weekends' doesn't start until Saturday.
    t.call("notify.quiet", json!({"weekdays": "22:00-07:00", "weekends": "00:00-10:00"})).await;
    assert_eq!(t.call("notify.send", json!({"title": "a"})).await["state"], "held");
    t.later(24 * 3600 + 8 * 3600); // Saturday 08:00
    assert_eq!(t.call("notify.send", json!({"title": "b"})).await["state"], "held", "weekends: until 10");
    t.call("notify.quiet", json!({"weekends": null})).await;
    assert_eq!(t.call("notify.send", json!({"title": "c"})).await["state"], "delivered", "weekends cleared");
    assert!(t.fails("notify.quiet", json!({"someday": "22:00-07:00"})).await.message.contains("day word"));
    assert!(t.fails("notify.quiet", json!({"fri": "late"})).await.message.contains("22:00-08:00"));
}

#[tokio::test]
async fn pause_holds_until_resume_and_high_still_comes_through() {
    let t = setup();
    t.call("notify.pause", json!({})).await;
    assert_eq!(t.call("notify.send", json!({"title": "later"})).await["state"], "held");
    assert_eq!(t.call("notify.send", json!({"title": "urgent", "priority": "high"})).await["state"], "delivered");

    let r = t.call("notify.resume", json!({})).await;
    assert_eq!((r["was_paused"].as_bool(), r["held"].as_u64()), (Some(true), Some(1)));
    assert_eq!(t.enqueued("notify.flush"), 1, "resume sends what waited");
    assert_eq!(t.call("notify.flush", json!({})).await["delivered"], 1);
    assert_eq!(t.desktop.got(), ["urgent|", "later|"]);

    t.call("notify.pause", json!({"all": true})).await;
    assert_eq!(t.call("notify.send", json!({"title": "fire", "priority": "high"})).await["state"], "held", "--all");
    let topics = t.topics();
    assert!(topics.contains(&"notify.pause.started".into()) && topics.contains(&"notify.pause.ended".into()));
}

#[tokio::test]
async fn a_timed_pause_ends_by_itself() {
    let t = setup();
    let until = (t.env.clock.now() + chrono::Duration::hours(2)).to_rfc3339();
    t.call("notify.pause", json!({"until": until})).await;
    assert_eq!(t.call("notify.send", json!({"title": "x"})).await["state"], "held");
    assert!(t.call("notify.status", json!({})).await["paused"].is_object());
    t.later(3 * 3600);
    assert!(t.call("notify.status", json!({})).await["paused"].is_null(), "over");
    assert_eq!(t.call("notify.flush", json!({})).await["delivered"], 1);
    assert!(!t.env.ctx.store.read_string("settings.toml").unwrap().unwrap_or_default().contains("[pause]"), "cleared");
    let past = (t.env.clock.now() - chrono::Duration::hours(1)).to_rfc3339();
    assert!(t.fails("notify.pause", json!({"until": past})).await.message.contains("already past"));
}

#[tokio::test]
async fn the_inbox_lists_newest_first_with_filters_and_counts() {
    let t = setup();
    t.send("standup notes").await;
    t.later(86_400);
    let alert = t
        .call("notify.send", json!({"title": "site couldn't launch", "priority": "high", "alert": "workspace_dirty"}))
        .await;
    t.later(1);
    t.send("Acme interview").await;

    let all = t.call("notify.list", json!({})).await;
    assert_eq!(titles(&all), ["Acme interview", "site couldn't launch", "standup notes"]);
    assert_eq!(all["counts"], json!({"total": 3, "unread": 3, "high": 1, "held": 0, "failed": 0}));
    assert_eq!(titles(&t.call("notify.list", json!({"limit": 1})).await), ["Acme interview"]);
    assert_eq!(titles(&t.call("notify.list", json!({"from": "alerts"})).await), ["site couldn't launch"]);
    assert_eq!(titles(&t.call("notify.list", json!({"text": "ACME"})).await), ["Acme interview"]);
    assert_eq!(titles(&t.call("notify.list", json!({"since": "2024-10-05"})).await).len(), 2);
    assert_eq!(titles(&t.call("notify.list", json!({"before": "2024-10-05"})).await), ["standup notes"]);

    let c = t.call("notify.count", json!({"priority": "high"})).await;
    assert_eq!((c["count"].as_u64(), c["unread"].as_u64()), (Some(1), Some(3)));
    let one = t.call("notify.get", json!({"id": alert["id"]})).await;
    assert_eq!(one["source"], "alert:workspace_dirty");
    assert!(t.fails("notify.get", json!({"id": "01JD2T0000000000000000000A"})).await.message.contains("no message"));
    assert_eq!(
        t.fails("notify.get", json!({"id": "../x"})).await.code,
        ErrorCode::NotFound,
        "only inbox ids are paths"
    );
    assert!(t.fails("notify.list", json!({"from": "friends"})).await.message.contains("schedules"));
}

#[tokio::test]
async fn marking_read_and_unread_by_ids_all_or_filters() {
    let t = setup();
    let a = t.send("a").await;
    t.send("b").await;
    t.call("notify.send", json!({"title": "c", "priority": "low"})).await;

    assert_eq!(t.call("notify.mark_read", json!({"ids": [a]})).await["marked"], 1);
    assert_eq!(t.call("notify.mark_read", json!({"ids": [a]})).await["marked"], 0, "already read");
    assert_eq!(t.call("notify.mark_read", json!({"priority": "low"})).await["marked"], 1);
    assert_eq!(titles(&t.call("notify.list", json!({"unread": true})).await), ["b"]);
    assert_eq!(t.call("notify.mark_read", json!({"all": true})).await["marked"], 1);
    assert_eq!(t.call("notify.mark_unread", json!({"text": "a"})).await["marked"], 1);
    assert_eq!(titles(&t.call("notify.list", json!({"unread": true})).await), ["a"]);
    assert!(t.topics().contains(&"notify.message.unread".into()));

    assert!(t.fails("notify.mark_read", json!({})).await.message.contains("say which messages"));
    assert!(t.fails("notify.mark_read", json!({"all": true, "read": true})).await.message.contains("not several"));
    assert!(t
        .fails("notify.mark_read", json!({"ids": ["01JD2T0000000000000000000A"]}))
        .await
        .message
        .contains("no message"));
}

#[tokio::test]
async fn removing_by_ids_is_at_once_and_in_bulk_asks_first() {
    let t = setup();
    let a = t.send("a").await;
    t.send("b").await;
    t.send("c").await;
    t.call("notify.mark_read", json!({"ids": [a]})).await;

    assert_eq!(t.call("notify.remove", json!({"ids": [a]})).await["removed"], 1);
    let e = t.fails("notify.remove", json!({"all": true})).await;
    assert_eq!(e.code, ErrorCode::ConfirmationRequired);
    assert_eq!(e.detail.unwrap(), json!({"count": 2, "unread": 2}));
    assert_eq!(t.files("inbox").len(), 2, "nothing removed without confirm");
    assert_eq!(t.call("notify.remove", json!({"text": "b", "confirm": true})).await["removed"], 1);
    assert_eq!(titles(&t.call("notify.list", json!({})).await), ["c"]);
    assert!(t.topics().contains(&"notify.message.removed".into()));
}

#[tokio::test]
async fn an_id_can_be_the_ending_list_shows_but_never_under_four_characters() {
    let t = setup();
    let ids = [t.send("a").await, t.send("b").await, t.send("c").await];

    // `short` is worked out against the whole inbox, so even a one-message page gives one that
    // finds its message (#246's review).
    let page = t.call("notify.list", json!({"limit": 1})).await;
    let short = page["items"][0]["short"].as_str().unwrap().to_owned();
    assert!(short.len() >= 6 && ids[2].ends_with(&short), "{short}");
    assert_eq!(t.call("notify.get", json!({"id": short.to_lowercase()})).await["title"], "c");
    let list = t.call("notify.list", json!({})).await;
    for item in list["items"].as_array().unwrap() {
        assert_eq!(t.call("notify.get", json!({"id": item["short"]})).await["id"], item["id"]);
    }

    // `shimmer notify remove 5`: one character never picks a message, even when only one ends so.
    for written in [&ids[0][25..], &ids[0][23..]] {
        let e = t.fails("notify.remove", json!({"ids": [written]})).await;
        assert_eq!(e.code, ErrorCode::InvalidParams, "{written}");
        assert!(e.message.contains("at least 4"), "{}", e.message);
    }
    assert_eq!(t.files("inbox").len(), 3, "nothing removed");
    assert_eq!(t.call("notify.mark_read", json!({"ids": [&ids[0][20..]]})).await["marked"], 1);
    assert_eq!(t.call("notify.remove", json!({"ids": [&ids[1][20..], &ids[2]]})).await["removed"], 2);
    assert_eq!(titles(&t.call("notify.list", json!({})).await), ["a"]);
}

#[tokio::test]
async fn the_cap_removes_the_oldest_read_first() {
    let t = setup();
    assert_eq!(t.call("notify.cap", json!({})).await["max"], 2000);
    assert!(t.fails("notify.cap", json!({"max": 5})).await.message.contains("at least 100"));
    t.call("notify.cap", json!({"max": 100})).await;
    let oldest_unread = t.send("oldest").await;
    let read = t.send("read one").await;
    t.call("notify.mark_read", json!({"ids": [read]})).await;
    for i in 0..98 {
        t.send(&format!("m{i}")).await;
    }
    assert_eq!(t.files("inbox").len(), 100);

    t.send("newest").await;
    let inbox = t.files("inbox");
    assert_eq!(inbox.len(), 100);
    assert!(!inbox.contains(&format!("inbox/{read}.toml")), "the read one went");
    assert!(inbox.contains(&format!("inbox/{oldest_unread}.toml")), "the older unread one stayed");

    t.send("one more").await;
    assert!(!t.files("inbox").contains(&format!("inbox/{oldest_unread}.toml")), "no read left: oldest unread");
    let list = t.call("notify.list", json!({"limit": 1})).await;
    assert_eq!(list["removed_unread"], 1);
}

#[tokio::test]
async fn test_and_resend_try_the_sinks_again() {
    let t = setup();
    let r = t.call("notify.test", json!({})).await;
    assert_eq!(
        (r["deliveries"][0]["medium"].as_str(), r["deliveries"][0]["ok"].as_bool()),
        (Some("desktop"), Some(true))
    );
    assert!(t.files("inbox").is_empty(), "a test isn't kept");

    *t.desktop.fails.lock().unwrap() = true;
    let failed = t.send("missed").await;
    *t.desktop.fails.lock().unwrap() = false;
    let r = t.call("notify.resend", json!({"state": "failed"})).await;
    assert_eq!(r["items"][0]["state"], "delivered");
    assert_eq!(r["items"][0]["delivery"].as_array().unwrap().len(), 2, "both attempts on the one message");
    assert_eq!(t.call("notify.get", json!({"id": failed})).await["state"], "delivered");
    assert!(t.fails("notify.resend", json!({})).await.message.contains("say which messages"));
}

#[tokio::test]
async fn alerts_are_enqueued_never_sent_inline_and_can_be_switched_off() {
    let t = setup();
    let failed = Event::new(
        ModuleId::new("queue"),
        "queue.task.failed",
        json!({"op": "records.complete", "origin": {"type": "scheduler", "trigger_id": "usr-1"},
               "error": "boom", "code": "module_error", "fallback": false}),
        t.env.clock.now(),
    );
    t.notify.on_event(&failed, &t.env.ctx).await.unwrap();
    let sent: Vec<(String, Value)> =
        t.env.queue.requests.lock().unwrap().iter().map(|r| (r.op.clone(), r.params["alert"].clone())).collect();
    assert_eq!(sent, [("notify.send".to_owned(), json!("scheduled_task_failed"))]);
    assert!(t.desktop.got().is_empty(), "the queue sends it, as a task");

    let own = Event::new(ModuleId::new("notify"), "notify.message.sent", json!({}), t.env.clock.now());
    t.notify.on_event(&own, &t.env.ctx).await.unwrap();
    assert_eq!(t.enqueued("notify.send"), 1, "never reacts to its own topics");

    let a = t.call("notify.builtin_alerts", json!({"name": "scheduled-task-failed", "on": false})).await;
    assert_eq!(a["alerts"], json!({"workspace_dirty": true, "scheduled_task_failed": false}));
    t.notify.on_event(&failed, &t.env.ctx).await.unwrap();
    assert_eq!(t.enqueued("notify.send"), 1, "switched off");
    assert!(t
        .fails("notify.builtin_alerts", json!({"name": "disk-full", "on": true}))
        .await
        .message
        .contains("workspace-dirty"));
}

#[tokio::test]
async fn a_broken_settings_file_never_silences_and_is_never_overwritten() {
    let t = setup();
    t.env.ctx.store.write("settings.toml", "cap = [\n").unwrap();
    assert_eq!(t.call("notify.send", json!({"title": "still here"})).await["state"], "delivered");
    let status = t.call("notify.status", json!({})).await;
    assert!(status["settings_problem"].as_str().unwrap().contains("settings.toml"), "{status}");
    assert!(t.fails("notify.quiet", json!({"every_day": "22:00-07:00"})).await.message.contains("fix it or remove it"));
    assert_eq!(t.env.ctx.store.read_string("settings.toml").unwrap().unwrap(), "cap = [\n", "left as it was");
}

#[tokio::test]
async fn status_says_it_all() {
    let t = setup();
    t.call("notify.quiet", json!({"every_day": "23:00-07:00"})).await;
    t.send("held").await;
    let s = t.call("notify.status", json!({})).await;
    assert_eq!(s["quiet_now"], true);
    assert_eq!(s["quiet"], json!({"every_day": "23:00-07:00"}));
    assert_eq!((s["cap"].as_u64(), s["counts"]["held"].as_u64()), (Some(2000), Some(1)));
    assert_eq!(s["sinks"], json!(["desktop"]));
    assert!(s["paused"].is_null() && s["settings_problem"].is_null());
    assert_eq!(s["builtin_alerts"]["workspace_dirty"], true);
}

#[tokio::test]
async fn the_manifest_declares_its_lane_trigger_and_ops() {
    let notify = Notify::default();
    assert_eq!(notify.lanes()[0].max_concurrent, 2);
    let t = &notify.triggers()[0];
    assert_eq!((t.id.as_str(), t.op.as_str()), ("notify.flush", "notify.flush"));
    assert_eq!(notify.commands().len(), 16);
    for topic in notify.manifest().topics {
        assert!(topic.starts_with("notify."), "{topic}");
    }
}
