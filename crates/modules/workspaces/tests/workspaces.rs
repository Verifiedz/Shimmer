//! The workspaces module through its entry points (`init`, `handle`) against the in-memory
//! `Ctx` (CLAUDE.md §12 rule 13). Files and events are inspected on the fake backend.

use serde_json::{json, Value};
use shimmer_core::testing::TestEnv;
use shimmer_core::{ErrorCode, Execution, Module, Result, StoreBackend};
use shimmer_workspaces::Workspaces;

const DEEP_WORK: &str = r#"
[workspace]
label = "Deep Work"
description = "Shimmer on Hyprland"

[[step]]
name = "setup"
mode = "supervised"
timeout_s = 60

[[step]]
name = "editor"
mode = "detached"

[cleanup]
timeout_s = 30
"#;

const DIRTY_STATE: &str = r#"state = "dirty"

[dirty]
reason = "exit code 1"
step = { index = 1, count = 2, name = "setup" }
failed_at = "2026-09-16T09:12:44Z"
log = "logs/deep-work-01JD2T.log"
"#;

fn put(env: &TestEnv, path: &str, text: &str) {
    env.ctx.store.write(path, text).unwrap();
}

fn file(env: &TestEnv, path: &str) -> Option<String> {
    env.backend.read("workspaces", path).unwrap().map(|b| String::from_utf8(b).unwrap())
}

fn topics(env: &TestEnv) -> Vec<String> {
    env.backend.events().into_iter().map(|e| e.topic).collect()
}

/// `deep-work`: valid, two steps and a cleanup script.
fn add_deep_work(env: &TestEnv) {
    put(env, "deep-work/workspace.toml", DEEP_WORK);
    put(env, "deep-work/steps/01-setup.sh", "git pull\n");
    put(env, "deep-work/steps/02-editor.sh", "code .\n");
    put(env, "deep-work/cleanup.sh", "docker stop db\n");
}

async fn setup() -> (Workspaces, TestEnv) {
    let env = TestEnv::new("workspaces");
    add_deep_work(&env);
    let w = Workspaces::default();
    w.init(&env.ctx).await.unwrap();
    (w, env)
}

async fn call(w: &Workspaces, env: &TestEnv, op: &str, params: Value) -> Result<Value> {
    w.handle(op, params, &env.ctx).await
}

// ---------------------------------------------------------------- manifest

#[test]
fn it_declares_its_lane_the_process_capability_and_the_protocol_ops() {
    let w = Workspaces::default();
    assert_eq!(w.manifest().capabilities, ["process"]);
    let lanes = w.lanes();
    assert_eq!((lanes[0].id.as_str(), lanes[0].max_concurrent), ("workspaces", 1));
    // docs/protocol.md "Workspace ops": what runs scripts is queued on the workspaces lane.
    let queued = || Execution::Queued { lane: "workspaces".into() };
    let ops: Vec<_> = w.commands().into_iter().map(|c| (c.op, c.execution)).collect();
    assert_eq!(
        ops,
        [
            ("workspaces.list".to_owned(), Execution::Inline),
            ("workspaces.status".to_owned(), Execution::Inline),
            ("workspaces.activate".to_owned(), queued()),
            ("workspaces.cleanup".to_owned(), queued()),
            ("workspaces.force_relaunch".to_owned(), queued()),
            ("workspaces.stop".to_owned(), queued()),
            ("workspaces.reset".to_owned(), Execution::Inline),
            ("workspaces.remove".to_owned(), Execution::Inline),
            ("workspaces.restore".to_owned(), Execution::Inline),
            ("workspaces.copy".to_owned(), Execution::Inline),
            ("workspaces.rename".to_owned(), Execution::Inline),
            ("workspaces.templates".to_owned(), Execution::Inline),
            ("workspaces.create".to_owned(), Execution::Inline),
            ("workspaces.answers".to_owned(), Execution::Inline),
            ("workspaces.reconfigure".to_owned(), Execution::Inline),
            ("workspaces.health".to_owned(), Execution::Inline),
        ]
    );
}

// ---------------------------------------------------------------- health

#[tokio::test]
async fn health_is_one_ok_line_when_nothing_is_wrong() {
    let (w, env) = setup().await;
    let before = topics(&env).len();
    let data = call(&w, &env, "workspaces.health", json!({})).await.unwrap();
    assert_eq!(data, json!({"checks": [{"status": "ok", "message": "1 workspace, all fine"}]}));
    assert_eq!(topics(&env).len(), before, "nothing written");
}

#[tokio::test]
async fn health_reports_dirty_broken_and_unstoppable_workspaces_with_a_fix() {
    let (w, env) = setup().await;
    put(&env, "deep-work/state.toml", DIRTY_STATE);
    // No workspace.toml at all.
    put(&env, "broken/steps/01-a.sh", "true\n");
    // A detached step and no cleanup script.
    put(
        &env,
        "loose/workspace.toml",
        "[workspace]\nlabel = \"Loose\"\n\n[[step]]\nname = \"editor\"\nmode = \"detached\"\n",
    );
    put(&env, "loose/steps/01-editor.sh", "code .\n");

    let data = call(&w, &env, "workspaces.health", json!({})).await.unwrap();
    let checks = data["checks"].as_array().unwrap();
    let by = |id: &str| checks.iter().filter(|c| c["subject"] == id).collect::<Vec<_>>();

    let broken = by("broken");
    assert_eq!((broken.len(), &broken[0]["status"]), (1, &json!("fail")), "{data}");
    assert!(
        broken[0]["message"].as_str().unwrap().contains("broken can't be used: there is no workspace.toml"),
        "{data}"
    );

    let dirty = by("deep-work");
    assert_eq!(dirty[0]["status"], "fail");
    let message = dirty[0]["message"].as_str().unwrap();
    assert!(
        message.contains("1/2 setup")
            && message.contains("exit code 1")
            && message.contains("logs/deep-work-01JD2T.log")
    );
    // It has a cleanup script, so that's the fix.
    assert_eq!(dirty[0]["fix"]["op"], "workspaces.cleanup");
    assert_eq!(dirty[0]["fix"]["params"], json!({"id": "deep-work"}));

    let loose = by("loose");
    assert_eq!(loose[0]["status"], "warn", "{data}");
    assert!(loose[0]["message"].as_str().unwrap().contains("no cleanup script"));
    assert_eq!(checks.len(), 3, "no ok line when something is wrong: {data}");
}

#[tokio::test]
async fn health_suggests_reset_for_a_dirty_workspace_with_no_cleanup_script() {
    let (w, env) = setup().await;
    env.ctx.store.delete("deep-work/cleanup.sh").unwrap();
    put(&env, "deep-work/workspace.toml", &DEEP_WORK.replace("[cleanup]\ntimeout_s = 30\n", ""));
    put(&env, "deep-work/state.toml", DIRTY_STATE);
    let data = call(&w, &env, "workspaces.health", json!({})).await.unwrap();
    let fixes: Vec<&Value> = data["checks"].as_array().unwrap().iter().map(|c| &c["fix"]).collect();
    assert_eq!(fixes[0]["op"], "workspaces.reset", "{data}");
}

// ---------------------------------------------------------------- list

#[tokio::test]
async fn list_shows_a_new_workspace_as_ready() {
    let (w, env) = setup().await;
    let data = call(&w, &env, "workspaces.list", Value::Null).await.unwrap();
    assert_eq!(data, json!({"workspaces": [{"id": "deep-work", "label": "Deep Work", "state": "ready"}]}));
    assert!(file(&env, "deep-work/state.toml").is_none(), "listing writes nothing");
}

#[tokio::test]
async fn a_broken_workspace_is_listed_as_invalid_and_hides_nothing() {
    let (w, env) = setup().await;
    put(&env, "broken/workspace.toml", "[workspace]\nlabel = \"x\"\n"); // no steps
    put(&env, "no-toml/steps/01-a.sh", "true\n"); // no workspace.toml at all
    put(&env, "README.md", "loose files at the top are ignored\n");

    let data = call(&w, &env, "workspaces.list", json!({})).await.unwrap();
    let list = data["workspaces"].as_array().unwrap();
    let ids: Vec<_> = list.iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["broken", "deep-work", "no-toml"], "sorted by id, none hidden");

    assert_eq!(list[0]["state"], "invalid");
    assert_eq!(list[0]["error"], "broken/workspace.toml: there must be at least one [[step]]");
    assert_eq!(list[1]["state"], "ready");
    assert_eq!(list[2]["state"], "invalid");
    assert_eq!(list[2]["error"], "no-toml: there is no workspace.toml");
}

#[tokio::test]
async fn list_shows_why_a_workspace_is_dirty() {
    let (w, env) = setup().await;
    put(&env, "deep-work/state.toml", DIRTY_STATE);
    let data = call(&w, &env, "workspaces.list", json!({})).await.unwrap();
    assert_eq!(data["workspaces"][0]["state"], "dirty");
    assert_eq!(data["workspaces"][0]["dirty_reason"], "step 1/2 setup: exit code 1");
}

#[tokio::test]
async fn an_unreadable_state_file_makes_the_workspace_invalid() {
    let (w, env) = setup().await;
    put(&env, "deep-work/state.toml", "state = \"sleeping\"\n");
    let data = call(&w, &env, "workspaces.list", json!({})).await.unwrap();
    assert_eq!(data["workspaces"][0]["state"], "invalid");
    assert_eq!(data["workspaces"][0]["error"], "deep-work/state.toml: unknown state \"sleeping\"");
}

#[tokio::test]
async fn an_invalid_workspace_shows_its_error_not_a_stale_dirty_reason() {
    // Review on #57: dirty, then workspace.toml hand-edited into something unparseable.
    let (w, env) = setup().await;
    put(&env, "deep-work/state.toml", DIRTY_STATE);
    put(&env, "deep-work/workspace.toml", "[workspace\n");

    let list = call(&w, &env, "workspaces.list", json!({})).await.unwrap();
    let entry = &list["workspaces"][0];
    assert_eq!(entry["state"], "invalid");
    assert!(entry["error"].as_str().unwrap().starts_with("deep-work/workspace.toml: "), "{entry}");
    assert!(entry.get("dirty_reason").is_none(), "no stale reason: {entry}");

    let status = call(&w, &env, "workspaces.status", json!({"id": "deep-work"})).await.unwrap();
    assert_eq!(status["state"], "invalid");
    for stale in ["dirty_reason", "dirty", "log", "running_step"] {
        assert!(status.get(stale).is_none(), "no stale {stale}: {status}");
    }
}

#[tokio::test]
async fn when_both_files_are_broken_both_errors_are_shown() {
    // Review on #56: fixing one file shouldn't be the only way to discover the other.
    let (w, env) = setup().await;
    put(&env, "deep-work/workspace.toml", "[workspace\n");
    put(&env, "deep-work/state.toml", "state = \"sleeping\"\n");
    for data in [
        call(&w, &env, "workspaces.list", json!({})).await.unwrap()["workspaces"][0].clone(),
        call(&w, &env, "workspaces.status", json!({"id": "deep-work"})).await.unwrap(),
    ] {
        let error = data["error"].as_str().unwrap();
        assert!(error.contains("deep-work/workspace.toml: "), "{error}");
        assert!(error.contains("deep-work/state.toml: unknown state \"sleeping\""), "{error}");
    }
}

// ---------------------------------------------------------------- status

#[tokio::test]
async fn status_describes_the_workspace_in_full() {
    let (w, env) = setup().await;
    let data = call(&w, &env, "workspaces.status", json!({"id": "deep-work"})).await.unwrap();
    assert_eq!(
        data,
        json!({
            "id": "deep-work", "label": "Deep Work", "description": "Shimmer on Hyprland",
            "state": "ready", "has_cleanup_script": true, "cleanup_timeout_s": 30, "last_session": null,
            "steps": [
                {"index": 1, "name": "setup", "mode": "supervised", "timeout_s": 60},
                {"index": 2, "name": "editor", "mode": "detached"},
            ],
        })
    );
}

#[tokio::test]
async fn status_of_a_dirty_workspace_includes_the_failure() {
    let (w, env) = setup().await;
    put(&env, "deep-work/state.toml", DIRTY_STATE);
    let data = call(&w, &env, "workspaces.status", json!({"id": "deep-work"})).await.unwrap();
    assert_eq!(data["state"], "dirty");
    // The fields crates/mockd/fixtures/workspace-dirty.json gives clients.
    assert_eq!(data["dirty_reason"], "step 1/2 setup: exit code 1");
    assert_eq!(data["log"], "logs/deep-work-01JD2T.log");
    assert_eq!(data["has_cleanup_script"], true);
    assert_eq!(data["last_session"], Value::Null);
    assert_eq!(
        data["dirty"],
        json!({"reason": "exit code 1", "failed_step": "1/2 setup",
               "failed_at": "2026-09-16T09:12:44Z", "log": "logs/deep-work-01JD2T.log"})
    );
}

#[tokio::test]
async fn status_of_an_invalid_workspace_says_why() {
    let (w, env) = setup().await;
    put(
        &env,
        "deep-work/workspace.toml",
        "[workspace]\nlabel = \"\"\n[[step]]\nname = \"setup\"\nmode = \"detached\"\n",
    );
    let data = call(&w, &env, "workspaces.status", json!({"id": "deep-work"})).await.unwrap();
    assert_eq!(data["state"], "invalid");
    assert_eq!(data["error"], "deep-work/workspace.toml: [workspace] label must not be empty");
}

#[tokio::test]
async fn an_unknown_id_is_not_found_whatever_it_contains() {
    let (w, env) = setup().await;
    for id in ["nope", "../records", "deep-work/steps", ""] {
        for op in ["workspaces.status", "workspaces.reset"] {
            let e = call(&w, &env, op, json!({"id": id})).await.unwrap_err();
            assert_eq!(e.code, ErrorCode::NotFound, "{op} {id:?}: {}", e.message);
        }
    }
    let e = call(&w, &env, "workspaces.status", json!({})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams, "missing id");
}

// ---------------------------------------------------------------- reset

#[tokio::test]
async fn reset_clears_dirty_and_records_what_it_cleared() {
    let (w, env) = setup().await;
    put(&env, "deep-work/state.toml", DIRTY_STATE);
    let data = call(&w, &env, "workspaces.reset", json!({"id": "deep-work"})).await.unwrap();
    assert_eq!(data, json!({"id": "deep-work", "state": "ready"}));
    assert_eq!(file(&env, "deep-work/state.toml").unwrap(), "state = \"ready\"\n");

    let ev = env.backend.events().pop().unwrap();
    assert_eq!(ev.topic, "workspaces.workspace.reset");
    assert_eq!(
        ev.payload,
        json!({"workspace": "deep-work", "prior": {
            "workspace": "deep-work", "reason": "exit code 1", "failed_step": "1/2 setup",
            "failed_at": "2026-09-16T09:12:44Z", "log": "logs/deep-work-01JD2T.log"}})
    );
}

#[tokio::test]
async fn reset_is_refused_unless_the_workspace_is_dirty() {
    let (w, env) = setup().await;
    let e = call(&w, &env, "workspaces.reset", json!({"id": "deep-work"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert!(e.message.contains("not dirty"), "{}", e.message);
    assert!(file(&env, "deep-work/state.toml").is_none(), "nothing written");
    assert!(topics(&env).is_empty(), "no event");
}

#[tokio::test]
async fn reset_also_clears_a_state_file_shimmer_cannot_read() {
    let (w, env) = setup().await;
    put(&env, "deep-work/state.toml", "this is not [toml");
    call(&w, &env, "workspaces.reset", json!({"id": "deep-work"})).await.unwrap();
    assert_eq!(file(&env, "deep-work/state.toml").unwrap(), "state = \"ready\"\n");
    let ev = env.backend.events().pop().unwrap();
    assert!(ev.payload["prior"]["unreadable_state"].as_str().unwrap().starts_with("deep-work/state.toml: "));
}

// ---------------------------------------------------------------- init: a launch the daemon never finished

#[tokio::test]
async fn a_workspace_left_launching_comes_back_dirty_after_a_restart() {
    let env = TestEnv::new("workspaces");
    add_deep_work(&env);
    put(&env, "deep-work/state.toml", "state = \"launching\"\nstep = { index = 2, count = 2, name = \"editor\" }\n");

    Workspaces::default().init(&env.ctx).await.unwrap();

    let state = file(&env, "deep-work/state.toml").unwrap();
    assert!(state.starts_with("state = \"dirty\""), "{state}");
    assert!(state.contains("the daemon stopped during step 2/2 editor"), "{state}");
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(ev.topic, "workspaces.session.dirty");
    assert_eq!(ev.payload["failed_step"], "2/2 editor");
    assert_eq!(ev.payload["recovered"], true, "no task failed to report it (ADR 0032 §4)");

    // A dirty workspace blocks activation until cleanup or reset (§10.3); reset works here.
    let w = Workspaces::default();
    call(&w, &env, "workspaces.reset", json!({"id": "deep-work"})).await.unwrap();
}

#[tokio::test]
async fn init_leaves_every_other_state_alone() {
    let env = TestEnv::new("workspaces");
    add_deep_work(&env);
    for (id, state) in [("a", "state = \"active\"\n"), ("b", DIRTY_STATE), ("c", "unreadable [")] {
        put(&env, &format!("{id}/workspace.toml"), DEEP_WORK);
        put(&env, &format!("{id}/state.toml"), state);
    }
    Workspaces::default().init(&env.ctx).await.unwrap();
    assert!(topics(&env).is_empty(), "nothing to recover");
    assert_eq!(file(&env, "a/state.toml").unwrap(), "state = \"active\"\n");
}

#[tokio::test]
async fn unknown_ops_are_refused() {
    let (w, env) = setup().await;
    let e = call(&w, &env, "workspaces.explode", json!({})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::UnknownOp);
}
