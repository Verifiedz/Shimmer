//! The workspaces module in the real daemon, through the real binary (issue #32), with the
//! real `LaunchBackend` wired in (ADR 0010, Dev A): the module is registered with its lane,
//! `list`/`status` read real folders, and `activate` actually runs a workspace's scripts. The
//! last tests drive the same through `shimmer workspaces …`, the CLI commands (issue #33).

mod common;

use std::time::{Duration, Instant};

use common::{stderr, stdout, wait_gone, Home};
use serde_json::{json, Value};

fn call(home: &Home, op: &str, params: Value) -> Value {
    let o = home.shimmer(&["call", op, &params.to_string(), "--json"]);
    assert!(o.status.success(), "{op} failed: {}", stderr(&o));
    serde_json::from_slice(&o.stdout).unwrap()
}

/// `data/workspaces/<id>/` with `files` as (relative path, contents).
fn workspace(home: &Home, id: &str, files: &[(&str, &str)]) {
    let dir = home.dir.path().join("home/data/workspaces").join(id);
    for (path, text) in files {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

const DEEP_WORK: &str =
    "[workspace]\nlabel = \"Deep Work\"\n\n[[step]]\nname = \"setup\"\nmode = \"supervised\"\ntimeout_s = 30\n";

/// Poll `queue.task` until the task has finished.
fn finished_task(home: &Home, task_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let t = call(home, "queue.task", json!({"task_id": task_id}));
        if !matches!(t["status"].as_str(), Some("queued" | "running")) {
            return t;
        }
        assert!(Instant::now() < deadline, "task never finished: {t}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn workspaces_are_registered_with_their_lane() {
    let home = Home::new();
    let m = call(&home, "core.manifest", json!({}));
    let module = m["modules"].as_array().unwrap().iter().find(|m| m["id"] == "workspaces").expect("registered");
    let ops: Vec<_> = module["commands"].as_array().unwrap().iter().map(|c| c["op"].as_str().unwrap()).collect();
    assert_eq!(
        ops,
        [
            "workspaces.list",
            "workspaces.status",
            "workspaces.activate",
            "workspaces.cleanup",
            "workspaces.force_relaunch",
            "workspaces.stop",
            "workspaces.reset",
            "workspaces.remove",
            "workspaces.restore",
            "workspaces.purge",
            "workspaces.copy",
            "workspaces.rename",
            "workspaces.templates",
            "workspaces.create",
            "workspaces.answers",
            "workspaces.reconfigure",
            "workspaces.health"
        ]
    );
    let lanes = m["lanes"].as_array().unwrap();
    assert!(lanes.iter().any(|l| l["id"] == "workspaces" && l["max_concurrent"] == 1), "{m}");
}

#[test]
fn list_and_status_read_real_folders_and_a_broken_one_hides_nothing() {
    let home = Home::new();
    call(&home, "core.ping", json!({})); // starts the daemon, which creates the data dir
    workspace(&home, "deep-work", &[("workspace.toml", DEEP_WORK), ("steps/01-setup.sh", "echo hi\n")]);
    workspace(&home, "broken", &[("workspace.toml", "[workspace\n")]);

    let list = call(&home, "workspaces.list", json!({}));
    let list = list["workspaces"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!((list[0]["id"].as_str(), list[0]["state"].as_str()), (Some("broken"), Some("invalid")));
    assert!(list[0]["error"].as_str().unwrap().starts_with("broken/workspace.toml: "));
    assert_eq!(list[1], json!({"id": "deep-work", "label": "Deep Work", "state": "ready"}));

    let status = call(&home, "workspaces.status", json!({"id": "deep-work"}));
    assert_eq!(status["steps"][0], json!({"index": 1, "name": "setup", "mode": "supervised", "timeout_s": 30}));
    assert_eq!(status["last_session"], Value::Null);
}

#[test]
fn activate_runs_the_real_script_and_the_result_survives_a_restart() {
    let home = Home::new();
    call(&home, "core.ping", json!({}));
    workspace(&home, "deep-work", &[("workspace.toml", DEEP_WORK), ("steps/01-setup.sh", "echo hi\n")]);

    // Queued: the request returns a task handle; the outcome arrives on the task.
    let handle = call(&home, "workspaces.activate", json!({"id": "deep-work"}));
    assert_eq!(handle["lane"], "workspaces");
    let task = finished_task(&home, handle["task_id"].as_str().unwrap());
    assert_eq!(task["status"], "succeeded", "{task}");

    // The script really ran: active, and that survives a restart.
    let state = |home: &Home| call(home, "workspaces.status", json!({"id": "deep-work"}))["state"].clone();
    assert_eq!(state(&home), "active");
    assert!(home.shimmer(&["shutdown"]).status.success());
    wait_gone(&home.socket());
    assert_eq!(state(&home), "active");
}

// ---------------------------------------------------------------- shimmer workspaces … (#33)

const DIRTY_STATE: &str = "state = \"dirty\"\n\n[dirty]\nreason = \"exit code 1\"\n\
                           step = { index = 1, count = 1, name = \"setup\" }\n\
                           failed_at = \"2026-10-02T09:12:44Z\"\nlog = \"logs/deep-work-01.log\"\n";

/// Run `shimmer args…` (stdin is not a terminal here, as in a script) and return
/// (exit code, stdout, stderr).
fn shimmer(home: &Home, args: &[&str]) -> (i32, String, String) {
    let o = home.shimmer(args);
    (o.status.code().unwrap_or(-1), stdout(&o), stderr(&o))
}

#[test]
fn the_cli_lists_and_shows_workspaces() {
    let home = Home::new();
    call(&home, "core.ping", json!({}));
    workspace(&home, "deep-work", &[("workspace.toml", DEEP_WORK), ("steps/01-setup.sh", "echo hi\n")]);
    workspace(&home, "broken", &[("workspace.toml", "[workspace\n")]);

    let (code, out, _) = shimmer(&home, &["workspaces", "list"]);
    assert_eq!(code, 0);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "ID         STATE    LABEL      WHY");
    assert!(lines[1].starts_with("broken     invalid  -          broken/workspace.toml: "), "{out}");
    assert_eq!(lines[2].trim_end(), "deep-work  ready    Deep Work");

    let (code, out, _) = shimmer(&home, &["workspaces", "status", "deep-work"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("deep-work  (ready)  Deep Work\n  steps:\n    1  setup  supervised, timeout 30s"), "{out}");
    assert!(out.contains("last session: none yet"), "{out}");

    let (code, _, err) = shimmer(&home, &["workspaces", "status", "nope"]);
    assert_eq!(code, 1);
    assert!(err.contains("not_found: no workspace 'nope'"), "{err}");

    let (_, help, _) = shimmer(&home, &["--help"]);
    assert!(help.contains("workspaces …"), "{help}");
}

#[test]
fn the_cli_queues_and_follows_a_launch() {
    let home = Home::new();
    call(&home, "core.ping", json!({}));
    workspace(&home, "deep-work", &[("workspace.toml", DEEP_WORK), ("steps/01-setup.sh", "echo hi\n")]);

    // Without --wait: queued, and a hint to check status (never "run it again").
    let (code, out, _) = shimmer(&home, &["workspaces", "activate", "deep-work"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("queued the launch of deep-work (task "), "{out}");
    assert!(out.ends_with("check on it with: shimmer workspaces status deep-work\n"), "{out}");

    // With --wait: followed to the end. The real backend runs the script, which succeeds.
    let (code, out, err) = shimmer(&home, &["workspaces", "activate", "deep-work", "--wait"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("step 1/1 setup"), "{err}");
    // The last step is supervised, so its output follows (ADR 0025, update-everything's summary).
    assert_eq!(out.trim_end(), "✓ deep-work is active\n  hi");
    assert_eq!(call(&home, "workspaces.status", json!({"id": "deep-work"}))["state"], "active");
}

#[test]
fn the_cli_explains_a_dirty_workspace_and_guards_the_overrides() {
    let home = Home::new();
    call(&home, "core.ping", json!({}));
    workspace(
        &home,
        "deep-work",
        &[("workspace.toml", DEEP_WORK), ("steps/01-setup.sh", "echo hi\n"), ("state.toml", DIRTY_STATE)],
    );

    // The refusal comes back on the task; --wait shows it the readable way.
    let (code, _, err) = shimmer(&home, &["workspaces", "activate", "deep-work", "--wait"]);
    assert_eq!(code, 1);
    assert!(err.contains("workspace_dirty: workspace 'deep-work' failed at step 1/1 setup (exit code 1)"), "{err}");
    assert!(err.contains("log: logs/deep-work-01.log"), "{err}");
    assert!(err.contains("shimmer workspaces force-relaunch deep-work"), "{err}");
    assert!(!err.contains("workspaces cleanup deep-work"), "no cleanup script, so no cleanup suggestion: {err}");

    // Nobody at the keyboard and no --yes: a clear refusal, never a hang.
    for args in [&["workspaces", "reset", "deep-work"][..], &["workspaces", "force-relaunch", "deep-work"]] {
        let (code, _, err) = shimmer(&home, args);
        assert_eq!(code, 1, "{args:?}");
        assert!(err.contains("without confirmation; pass --yes"), "{args:?}: {err}");
    }
    assert_eq!(call(&home, "workspaces.status", json!({"id": "deep-work"}))["state"], "dirty", "nothing changed");

    let (code, out, _) = shimmer(&home, &["workspaces", "reset", "deep-work", "--yes"]);
    assert_eq!((code, out.trim()), (0, "✓ deep-work reset to ready"));
    assert_eq!(call(&home, "workspaces.status", json!({"id": "deep-work"}))["state"], "ready");
}
