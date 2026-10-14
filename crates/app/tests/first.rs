//! `--first` (#222) through the real binary and daemon: a launch asked to run first is put ahead
//! of what's waiting in the workspaces lane only once it's confirmed, never ahead of the running
//! one, and the promotion is in the event log (CLAUDE.md §6.2).

mod common;

use common::{fails, ok, Home};
use serde_json::Value;

const ONE_STEP: &str =
    "[workspace]\nlabel = \"W\"\n\n[[step]]\nname = \"run\"\nmode = \"supervised\"\ntimeout_s = 30\n";

fn workspace(home: &Home, id: &str, script: &str) {
    let dir = home.dir.path().join("home/data/workspaces").join(id);
    std::fs::create_dir_all(dir.join("steps")).unwrap();
    std::fs::write(dir.join("workspace.toml"), ONE_STEP).unwrap();
    std::fs::write(dir.join("steps/01-run.sh"), script).unwrap();
}

/// The ids of the tasks waiting in the workspaces lane, in order.
fn waiting(home: &Home) -> Vec<String> {
    let list: Value = serde_json::from_str(&ok(home, &["--json", "queue", "list"])).unwrap();
    let lane = list["lanes"].as_array().unwrap().iter().find(|l| l["id"] == "workspaces").unwrap().clone();
    lane["queued"].as_array().unwrap().iter().map(|t| t["task_id"].as_str().unwrap().to_owned()).collect()
}

fn task_id(out: &str) -> String {
    let start = out.find("(task ").unwrap() + "(task ".len();
    out[start..].split(')').next().unwrap().to_owned()
}

#[test]
fn first_runs_ahead_of_whats_waiting_once_confirmed() {
    let home = Home::new();
    // Holds the lane until the test lets it go, so nothing races the checks below.
    let go = home.dir.path().join("go");
    workspace(&home, "slow", &format!("while [ ! -e '{}' ]; do sleep 0.05; done\n", go.display()));
    workspace(&home, "b", "true\n");
    workspace(&home, "c", "true\n");

    ok(&home, &["workspaces", "activate", "slow"]);
    let b = task_id(&ok(&home, &["workspaces", "activate", "b"]));

    // No terminal here: without --yes it refuses, and nothing is queued.
    let e = fails(&home, &["workspaces", "activate", "c", "--first"]);
    assert!(e.contains("pass --yes"), "{e}");
    assert_eq!(waiting(&home), [b.as_str()]);

    let out = ok(&home, &["workspaces", "activate", "c", "--first", "--yes"]);
    assert!(out.contains("first in its lane"), "{out}");
    let c = task_id(&out);
    assert_eq!(waiting(&home), [c.as_str(), b.as_str()], "c ahead of b; slow keeps running");

    std::fs::write(&go, "").unwrap();
    let events = std::fs::read_dir(home.dir.path().join("home/events"))
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
        .collect::<String>();
    let promoted: Vec<Value> = events
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|e| e["topic"] == "queue.task.promoted")
        .collect();
    assert_eq!(promoted.len(), 1, "one audited promotion");
    assert_eq!(promoted[0]["payload"]["task_id"], c.as_str());
    assert_eq!(promoted[0]["payload"]["displaced"], serde_json::json!([b]));
}

#[test]
fn first_is_refused_on_an_op_that_answers_at_once() {
    let home = Home::new();
    let e = fails(&home, &["call", "workspaces.list", "--first"]);
    assert!(e.contains("--first is for queued ops"), "{e}");
}
