//! Notify end to end (ADR 0032), through the real binary and a real daemon: a message sent as a
//! queued task lands in the inbox file, and the inbox ops work over the socket. There's no sink
//! yet (the desktop one comes with `Ctx.desktop`), so a message is kept as `failed`.

mod common;

use std::time::{Duration, Instant};

use common::{stderr, Home};
use serde_json::{json, Value};

fn call(home: &Home, op: &str, params: Value) -> Value {
    let o = home.shimmer(&["call", op, &params.to_string(), "--json"]);
    assert!(o.status.success(), "{op} failed: {}", stderr(&o));
    serde_json::from_slice(&o.stdout).unwrap()
}

/// `notify.list` once the queued sends have run: `n` messages, for up to 10 s.
fn inbox(home: &Home, n: usize) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let list = call(home, "notify.list", json!({}));
        if list["items"].as_array().is_some_and(|i| i.len() == n) {
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
        (Some("Build finished"), Some("high"), Some("failed"))
    );
    let id = m["id"].as_str().unwrap();
    let file = std::fs::read_to_string(home.dir.path().join(format!("home/data/notify/inbox/{id}.toml"))).unwrap();
    assert!(file.contains("title = \"Build finished\""), "files are the truth: {file}");

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
