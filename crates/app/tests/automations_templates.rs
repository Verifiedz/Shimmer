//! Every rule template on the real binary and daemon (ADR 0035 §9; the template safety rules of
//! ADR 0025 §6a). The two workspace templates run for real here; the message templates fire for
//! real in automations_notify.rs.

mod common;

use std::time::{Duration, Instant};

use common::{fails, ok, Home};
use serde_json::Value;

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
name = "fail"
type = "string"
"#;

fn home() -> Home {
    let home = Home::new();
    let dir = home.dir.path().join("home/data/records/collections");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("jobs.toml"), JOBS).unwrap();
    home
}

fn json(home: &Home, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    serde_json::from_str(&ok(home, &all)).unwrap()
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(30), "never: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn templates_list_and_peek_show_what_they_make() {
    let home = home();
    let out = ok(&home, &["automations", "templates"]);
    for id in ["notify-on-add", "follow-up-reminder", "stage-creates-workspace"] {
        assert!(out.contains(id), "{id}: {out}");
    }
    let peek = ok(&home, &["automations", "peek", "stage-creates-workspace"]);
    assert!(peek.contains("── RULE-make ──") && peek.contains("── RULE-stop ──"), "{peek}");
    assert!(peek.contains("runs: workspaces.activate, workspaces.create, workspaces.stop"), "{peek}");
}

#[test]
fn stage_opens_workspace_runs_the_workspace_when_the_field_changes() {
    let home = home();
    // A workspace of our own whose one step leaves a mark.
    let ws = home.dir.path().join("home/data/workspaces/prep");
    std::fs::create_dir_all(ws.join("steps")).unwrap();
    std::fs::write(
        ws.join("workspace.toml"),
        "[workspace]\nlabel = \"Prep\"\n\n[[step]]\nname = \"mark\"\nmode = \"supervised\"\ntimeout_s = 10\n",
    )
    .unwrap();
    std::fs::write(ws.join("steps/01-mark.sh"), "echo opened >> \"$SHIMMER_WORKSPACE_DIR/opened\"\n").unwrap();

    let set = ["COLLECTION=jobs", "FIELD=stage", "VALUE=interviewing", "WORKSPACE=prep"];
    // From a script without --yes: refused, nothing made (#254's review).
    let mut unconfirmed = vec!["automations", "new", "open-prep", "--from", "stage-opens-workspace"];
    for s in &set {
        unconfirmed.extend(["--set", s]);
    }
    let refused = fails(&home, &unconfirmed);
    assert!(refused.contains("without confirmation; pass --yes"), "{refused}");
    assert!(!ok(&home, &["automations", "list"]).contains("open-prep"), "nothing made");
    let mut args = vec!["automations", "new", "open-prep", "--from", "stage-opens-workspace", "--yes"];
    for s in &set {
        args.extend(["--set", s]);
    }
    assert!(ok(&home, &args).contains("✓ made from stage-opens-workspace, and on:"));

    ok(&home, &["records", "add", "jobs", "--title", "Acme", "--stage", "applied"]);
    ok(&home, &["records", "update", "jobs/acme", "--stage", "interviewing"]);
    until("the workspace opened", || ws.join("opened").exists());
    // Editing something else doesn't open it again: only a change of stage does.
    ok(&home, &["records", "update", "jobs/acme", "--title", "Acme Inc"]);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(std::fs::read_to_string(ws.join("opened")).unwrap(), "opened\n");
    let status = json(&home, &["workspaces", "status", "prep"]);
    assert_eq!(status["state"], "active", "{status}");
}

/// The three-rule template, for real: make (from a workspace template, an answer from the
/// record), open (a chain of 2: its launch runs, including a background step), and stop when the
/// record closes (its cleanup ends that step: nothing is left running).
#[test]
fn stage_creates_workspace_makes_opens_and_stops_one_per_record() {
    let home = home();
    let set = [
        "COLLECTION=jobs",
        "FIELD=stage",
        "VALUE=interviewing",
        "TEMPLATE=smoke-test",
        "CLOSED=offer, rejected",
        "ANSWERS=FAIL_AT=item.fail",
    ];
    let mut args = vec!["automations", "new", "prep", "--from", "stage-creates-workspace", "--yes"];
    for s in &set {
        args.extend(["--set", s]);
    }
    let out = ok(&home, &args);
    for id in ["prep-make", "prep-open", "prep-stop"] {
        assert!(out.contains(id), "{id}: {out}");
    }

    // No `fail` on the record: FAIL_AT is empty, so smoke-test takes its default and launches.
    ok(&home, &["records", "add", "jobs", "--title", "Acme", "--stage", "applied"]);
    ok(&home, &["records", "update", "jobs/acme", "--stage", "interviewing"]);
    let state = || json(&home, &["workspaces", "status", "acme-smoke-test"])["state"].as_str().unwrap_or("").to_owned();
    let made = || {
        json(&home, &["workspaces", "list"])["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["id"] == "acme-smoke-test")
    };
    until("acme's workspace was made", made);
    until("it was opened by the second rule", || state() == "active");
    let history = ok(&home, &["automations", "history", "prep-open"]);
    assert!(history.contains("prep-open on workspaces.workspace.created: queued workspaces.activate"), "{history}");
    // A detached step is started, not waited on: `active` can come before it has written its pid.
    until("smoke-test's background step saved its pid", || background_pid().is_some());
    let (pid, dir) = background_pid().expect("smoke-test's background step saved its pid");
    let _cleanup = Cleanup { pid, dir };
    assert!(alive(pid), "the background step runs while it's open");

    ok(&home, &["records", "update", "jobs/acme", "--stage", "offer"]);
    until("the third rule stopped it", || state() == "ready");
    until("its background step ended", || !alive(pid));
}

/// Even when an assertion fails: no background step left running, no state folder left behind.
struct Cleanup {
    pid: u32,
    dir: std::path::PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if alive(self.pid) {
            let _ = std::process::Command::new("kill").arg(self.pid.to_string()).status();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// smoke-test's background step writes its pid to `background.pid` in its state folder, under
/// the temporary folder: `shimmer-<uid>-<home tag>-<workspace>` (its lib/shimmer-open.sh).
fn state_dir() -> Option<std::path::PathBuf> {
    let tmp = std::env::var_os("TMPDIR").map_or_else(|| "/tmp".into(), std::path::PathBuf::from);
    std::fs::read_dir(tmp)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().ends_with("-acme-smoke-test")))
        .filter(|p| p.join("background.pid").exists())
        .max_by_key(|p| std::fs::metadata(p.join("background.pid")).and_then(|m| m.modified()).ok())
}

fn background_pid() -> Option<(u32, std::path::PathBuf)> {
    let dir = state_dir()?;
    let pid = std::fs::read_to_string(dir.join("background.pid")).ok()?.trim().parse().ok()?;
    Some((pid, dir))
}

/// Running, not a zombie waiting to be reaped. `ps`, not `/proc`: macOS has no `/proc`.
fn alive(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().starts_with('Z'))
}
