//! Notify end to end (ADR 0032), through the real binary and a real daemon: a message sent as a
//! queued task lands in the inbox file, goes to the desktop, and the inbox ops work over the
//! socket. Every test home has `[desktop] show = false`, so the real backend logs each note
//! instead of popping it up.

mod common;

use std::time::{Duration, Instant};

use common::{stderr, Home};
use serde_json::{json, Value};

fn call(home: &Home, op: &str, params: Value) -> Value {
    let o = home.shimmer(&["call", op, &params.to_string(), "--json"]);
    assert!(o.status.success(), "{op} failed: {}", stderr(&o));
    serde_json::from_slice(&o.stdout).unwrap()
}

/// `notify.list` once the queued sends have run: `n` messages, none still `pending`, for up to
/// 10 s. A message is written as `pending` when it lands, then again once delivery was tried:
/// looking in between would see `pending` (it did, on CI).
fn inbox(home: &Home, n: usize) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let list = call(home, "notify.list", json!({}));
        let items = list["items"].as_array().cloned().unwrap_or_default();
        if items.len() == n && items.iter().all(|m| m["state"] != "pending") {
            return list;
        }
        assert!(Instant::now() < deadline, "the inbox never had {n} messages: {list}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_message_sent_through_the_queue_lands_in_the_inbox() {
    let home = Home::new();
    let queued = call(&home, "notify.send", json!({"title": "Build finished", "body": "v1.4", "priority": "high"}));
    assert!(queued["task_id"].is_string(), "notify.send is queued: {queued}");

    let list = inbox(&home, 1);
    let m = &list["items"][0];
    assert_eq!(
        (m["title"].as_str(), m["priority"].as_str(), m["state"].as_str()),
        (Some("Build finished"), Some("high"), Some("delivered"))
    );
    let id = m["id"].as_str().unwrap();
    let file = std::fs::read_to_string(home.dir.path().join(format!("home/data/notify/inbox/{id}.toml"))).unwrap();
    assert!(file.contains("title = \"Build finished\""), "files are the truth: {file}");

    // The real desktop backend took it and, with `show = false`, logged it instead of showing it.
    let logs = std::fs::read_dir(home.dir.path().join("home/logs")).unwrap();
    let logged: String = logs.filter_map(|e| std::fs::read_to_string(e.unwrap().path()).ok()).collect();
    assert!(logged.contains("desktop note") && logged.contains("Build finished"), "{logged}");

    assert_eq!(call(&home, "notify.mark_read", json!({"ids": [id]}))["marked"], 1);
    assert_eq!(call(&home, "notify.count", json!({"unread": true}))["count"], 0);

    // A bulk remove asks first, over the socket too.
    let o = home.shimmer(&["call", "notify.remove", r#"{"all":true}"#, "--json"]);
    assert!(!o.status.success() && stderr(&o).contains("confirmation_required"), "{}", stderr(&o));
    assert_eq!(call(&home, "notify.remove", json!({"all": true, "confirm": true}))["removed"], 1);
    assert_eq!(inbox(&home, 0)["counts"]["total"], 0);
}

#[test]
fn settings_change_at_once_with_no_restart() {
    let home = Home::new();
    call(&home, "notify.pause", json!({}));
    call(&home, "notify.send", json!({"title": "later"}));
    assert_eq!(inbox(&home, 1)["items"][0]["state"], "held");
    assert!(call(&home, "notify.status", json!({}))["paused"].is_object());
    let settings = std::fs::read_to_string(home.dir.path().join("home/data/notify/settings.toml")).unwrap();
    assert!(settings.contains("[pause]"), "{settings}");

    call(&home, "notify.resume", json!({}));
    assert!(call(&home, "notify.status", json!({}))["paused"].is_null());
}

/// `shimmer notify …` as a person types it (ADR 0032 §7).
#[test]
fn the_notify_commands() {
    let home = Home::new();
    let sent = common::ok(&home, &["notify", "send", "Build finished", "--body", "v1.4 is live", "--priority", "high"]);
    assert!(sent.starts_with("✓ sent \"Build finished\""), "{sent}");
    inbox(&home, 1);

    let list = common::ok(&home, &["notify", "list"]);
    assert!(list.contains("Build finished — v1.4 is live") && list.contains("1 unread"), "{list}");
    assert_eq!(common::ok(&home, &["notify", "count"]).trim(), "1");
    let id = inbox(&home, 1)["items"][0]["id"].as_str().unwrap().to_owned();
    let short = &id[id.len() - 6..];
    let shown = common::ok(&home, &["notify", "show", short]);
    assert!(shown.starts_with("Build finished\nv1.4 is live") && shown.contains("priority  high"), "{shown}");
    assert_eq!(common::ok(&home, &["notify", "count"]).trim(), "0", "show marks it read");
    assert!(common::ok(&home, &["notify", "mark-unread", "--all"]).starts_with("✓ marked 1 message unread"));

    // A bulk remove asks first; with no one to ask, it says to add --yes.
    let refused = common::fails(&home, &["notify", "remove", "--all"]);
    assert!(refused.contains("this removes 1 message (1 unread); add --yes"), "{refused}");
    assert!(common::ok(&home, &["notify", "remove", "--all", "--yes"]).starts_with("✓ removed 1 message"));
    assert!(common::ok(&home, &["notify", "list"]).starts_with("no messages yet"));

    assert!(common::ok(&home, &["notify", "pause", "2h"]).starts_with("✓ paused until"));
    assert!(common::ok(&home, &["notify", "status"]).contains("paused     yes, until"));
    assert!(common::ok(&home, &["notify", "resume"]).starts_with("✓ resumed"));
    let quiet = common::ok(&home, &["notify", "quiet", "22:00-08:00", "--weekends", "00:00-10:00"]);
    assert!(quiet.contains("every day  22:00-08:00") && quiet.contains("weekends   00:00-10:00"), "{quiet}");
    // The alerts are automations' built-in rules now (ADR 0035 §10).
    assert!(common::ok(&home, &["automations", "disable", "workspace-dirty"]).contains("✓ workspace-dirty is off"));
    assert!(common::fails(&home, &["notify", "builtin-alerts"]).contains("unknown notify command"));
    assert!(common::ok(&home, &["notify", "cap", "500"]).starts_with("✓ the inbox keeps at most 500"));
    let test = common::ok(&home, &["notify", "test"]);
    assert!(test.starts_with("Sent \"Shimmer can reach you\" to:\n  desktop ✓"), "{test}");
    assert!(common::fails(&home, &["notify", "cap", "5"]).contains("at least 100"));
}

/// `notify send` can be scheduled (ADR 0032 §7), and reads back as typed.
#[test]
fn a_reminder_can_be_scheduled() {
    let home = Home::new();
    let out = common::ok(&home, &["schedule", "every", "1h", "--catch-up", "skip", "notify", "send", "Stretch break"]);
    assert!(out.starts_with("✓ scheduled notify send 'Stretch break'"), "{out}");
    let list = common::ok(&home, &["scheduler", "list"]);
    assert!(list.contains("notify send 'Stretch break'"), "{list}");
    let refused = common::fails(&home, &["schedule", "every", "1h", "--catch-up", "skip", "notify", "list"]);
    assert!(refused.contains("only notify send can"), "{refused}");
}
