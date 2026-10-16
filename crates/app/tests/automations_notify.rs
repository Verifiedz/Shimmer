//! Automations and notify together, on the real binary and daemon (ADR 0035 §9–§10): every
//! message template and the built-in rules fire for real, and their messages land in the inbox
//! marked as coming from automations. Test homes have `[desktop] show = false`.

mod common;

use std::time::{Duration, Instant};

use common::{ok, Home};
use serde_json::{json, Value};

const JOBS: &str = r#"
[collection]
id = "jobs"
label = "Jobs"
title = "{title}"

[[field]]
name = "title"
type = "string"
required = true

[[field]]
name = "stage"
type = "enum"
values = ["applied", "interviewing", "offer", "rejected"]

[[field]]
name = "attempts"
type = "int"
"#;

fn home() -> Home {
    let home = Home::new();
    let dir = home.dir.path().join("home/data/records/collections");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("jobs.toml"), JOBS).unwrap();
    home
}

fn call(home: &Home, op: &str, params: Value) -> Value {
    let out = ok(home, &["call", op, &params.to_string(), "--json"]);
    serde_json::from_str(&out).unwrap()
}

/// From `template` with these answers, as `RULE`.
fn make(home: &Home, rule: &str, template: &str, set: &[&str]) {
    let mut args = vec!["automations", "new", rule, "--from", template, "--yes"];
    for s in set {
        args.extend(["--set", s]);
    }
    ok(home, &args);
}

/// The inbox message titled `title`, once it's there.
fn message(home: &Home, title: &str) -> Value {
    let start = Instant::now();
    loop {
        let list = call(home, "notify.list", json!({"limit": 200}));
        if let Some(m) = list["items"].as_array().unwrap().iter().find(|m| m["title"] == title) {
            return m.clone();
        }
        let titles: Vec<&Value> = list["items"].as_array().unwrap().iter().map(|m| &m["title"]).collect();
        assert!(start.elapsed() < Duration::from_secs(30), "no message '{title}' (there are: {titles:?})");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn each_message_template_sends_its_message_from_automations() {
    let home = home();
    make(&home, "added", "notify-on-add", &["COLLECTION=jobs"]);
    make(&home, "done", "notify-on-complete", &["COLLECTION=jobs"]);
    make(&home, "offer", "stage-message", &["COLLECTION=jobs", "FIELD=stage", "VALUE=offer"]);
    make(&home, "tries", "field-threshold", &["COLLECTION=jobs", "FIELD=attempts", "OVER=3"]);
    make(&home, "failures", "notify-on-failure", &["LANE=workspaces"]);
    make(&home, "follow", "follow-up-reminder", &["COLLECTION=jobs", "DAYS=2"]);
    make(&home, "redo", "spaced-review", &["COLLECTION=jobs"]);

    ok(&home, &["records", "add", "jobs", "--title", "Acme", "--stage", "applied"]);
    let m = message(&home, "New in jobs: Acme");
    assert_eq!(m["source"], "automation:added", "{m}");
    ok(&home, &["records", "update", "jobs/acme", "--stage", "offer"]);
    assert_eq!(message(&home, "Acme: stage is now offer")["priority"], "high");
    ok(&home, &["records", "update", "jobs/acme", "--attempts", "2"]);
    ok(&home, &["records", "update", "jobs/acme", "--attempts", "5"]);
    message(&home, "Acme: attempts is 5");
    ok(&home, &["records", "complete", "jobs/acme"]);
    message(&home, "Done: Acme");

    // A task that fails in the workspaces lane: there's no such workspace.
    call(&home, "workspaces.activate", json!({"id": "nope"}));
    message(&home, "Failed: workspaces.activate");

    // The two reminders wait as schedules, labelled with their rules.
    let start = Instant::now();
    let labels = loop {
        let list = call(&home, "scheduler.list", json!({}));
        let labels: Vec<String> = list["triggers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["source"]["label"].as_str().map(String::from))
            .collect();
        if labels.len() == 2 || start.elapsed() > Duration::from_secs(20) {
            break labels;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        labels.iter().any(|l| l.starts_with("rule follow:")) && labels.iter().any(|l| l.starts_with("rule redo:")),
        "{labels:?}"
    );

    // Only the messages these rules sent, nothing twice.
    let from = call(&home, "notify.list", json!({"from": "automations", "limit": 200}));
    assert_eq!(from["items"].as_array().unwrap().len(), 5, "{from}");
}

#[test]
fn the_built_in_rules_say_what_went_wrong_once() {
    let home = home();
    // A workspace that fails at its first step on purpose: the built-in rule says so, with the fix.
    let _state = StateDir::of(&home, "site");
    ok(&home, &["workspaces", "new", "site", "--from", "smoke-test", "--set", "FAIL_AT=check"]);
    call(&home, "workspaces.activate", json!({"id": "site"}));
    let m = message(&home, "site couldn't launch");
    assert_eq!(m["source"], "automation:workspace-dirty");
    let full = call(&home, "notify.get", json!({"id": m["id"]}));
    assert!(full["body"].as_str().unwrap().ends_with("Then: shimmer workspaces cleanup site"), "{full}");

    // A rule that sets itself off: stopped, and loop-stopped says so.
    ok(&home, &["records", "add", "jobs", "--title", "Acme"]);
    let rule = "[rule]\non = \"records.item.updated\"\n[[do]]\nop = \"records.update\"\nparams = { collection = \"jobs\", id = \"{id}\", fields = { attempts = 1 } }\n";
    call(&home, "automations.create", json!({"id": "again", "text": rule}));
    ok(&home, &["records", "update", "jobs/acme", "--stage", "applied"]);
    assert_eq!(message(&home, "Automation again was stopped")["source"], "automation:loop-stopped");

    // A scheduled task that fails: scheduled-task-failed says so.
    let soon = (shimmer_core::Clock::system().now() + Duration::from_secs(1)).to_rfc3339();
    call(
        &home,
        "scheduler.add",
        json!({"schedule": {"once": soon}, "op": "workspaces.activate",
                                        "params": {"id": "nope"}, "catch_up": "run_once"}),
    );
    assert_eq!(message(&home, "Scheduled workspaces.activate failed")["source"], "automation:scheduled-task-failed");

    // One message for each: the dirty launch wasn't also reported as a scheduled failure.
    std::thread::sleep(Duration::from_millis(500));
    let all = call(&home, "notify.list", json!({"limit": 200}));
    let titles: Vec<&str> = all["items"].as_array().unwrap().iter().filter_map(|m| m["title"].as_str()).collect();
    assert_eq!(titles.len(), 3, "{titles:?}");
}

/// A workspace's state folder under the temporary folder, removed when the test ends:
/// `shimmer-<uid>-<home tag>-<workspace>`, as smoke-test's lib/shimmer-open.sh names it.
struct StateDir(std::path::PathBuf);

impl StateDir {
    fn of(home: &Home, workspace: &str) -> Self {
        let shimmer_home = home.dir.path().join("home");
        let tag = shimmer_home.to_string_lossy().bytes().fold(0u64, |h, b| (h * 31 + u64::from(b)) % 2_147_483_647);
        let uid = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| s.lines().find(|l| l.starts_with("Uid:"))?.split_whitespace().nth(1).map(String::from))
            .unwrap_or_default();
        let tmp = std::env::var_os("TMPDIR").map_or_else(|| "/tmp".into(), std::path::PathBuf::from);
        Self(tmp.join(format!("shimmer-{uid}-{tag:08x}-{workspace}")))
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
