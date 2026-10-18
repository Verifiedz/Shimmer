//! `workspaces.stop`, `remove`, `restore` (ADR 0026) and `purge` (ADR 0038) against `FakeLauncher` and the
//! in-memory `Ctx` (CLAUDE.md §12 rule 13).

use serde_json::{json, Value};
use shimmer_core::testing::TestEnv;
use shimmer_core::{ErrorCode, Module, Result, SpawnMode, Step, StepOutcome, StoreBackend};
use shimmer_workspaces::Workspaces;

const SITE: &str = r#"
[workspace]
label = "Site"

[[step]]
name = "server"
mode = "detached"

[cleanup]
timeout_s = 30

[env]
PROJECT_DIR = "/home/me/site"
"#;

const ACTIVE: &str = "state = \"active\"\n";

fn env_with_site(state: Option<&str>, cleanup: bool) -> TestEnv {
    let env = TestEnv::new("workspaces");
    let toml = if cleanup { SITE.to_owned() } else { SITE.replace("[cleanup]\ntimeout_s = 30\n", "") };
    put(&env, "site/workspace.toml", &toml);
    put(&env, "site/steps/01-server.sh", "true\n");
    if cleanup {
        put(&env, "site/cleanup.sh", "true\n");
    }
    if let Some(state) = state {
        put(&env, "site/state.toml", state);
    }
    env
}

fn put(env: &TestEnv, path: &str, text: &str) {
    env.ctx.store.write(path, text).unwrap();
}

fn file(env: &TestEnv, path: &str) -> Option<String> {
    env.backend.read("workspaces", path).unwrap().map(|b| String::from_utf8(b).unwrap())
}

async fn call(w: &Workspaces, env: &TestEnv, op: &str) -> Result<Value> {
    w.handle(op, json!({"id": "site"}), &env.ctx).await
}

fn outcome(exit_code: i32) -> Result<StepOutcome> {
    Ok(StepOutcome {
        session_id: "01S".into(),
        exit_code: Some(exit_code),
        timed_out: false,
        log_path: "logs/site-cleanup.log".into(),
    })
}

fn state_line(env: &TestEnv) -> String {
    file(env, "site/state.toml").unwrap_or_default().lines().next().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn stop_runs_the_cleanup_script_and_goes_ready() {
    let env = env_with_site(Some(ACTIVE), true);
    env.launcher.outcomes.lock().unwrap().push_back(outcome(0));
    let w = Workspaces::default();
    assert_eq!(
        call(&w, &env, "workspaces.stop").await.unwrap(),
        json!({"id": "site", "state": "ready", "log": "logs/site-cleanup.log"}),
        "the log, so a client can show what was closed"
    );
    let ran = env.launcher.received.lock().unwrap().clone();
    assert_eq!(ran.len(), 1);
    assert_eq!(ran[0].step, Step::Cleanup);
    assert!(matches!(ran[0].mode, SpawnMode::Supervised { .. }));
    assert_eq!(ran[0].user_env, [("PROJECT_DIR".to_owned(), "/home/me/site".to_owned())], "same variables");
    assert_eq!(state_line(&env), "state = \"ready\"");
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(ev.topic, "workspaces.session.stopped");
    assert_eq!(ev.payload, json!({"workspace": "site", "ran_cleanup": true, "log": "logs/site-cleanup.log"}));
}

#[tokio::test]
async fn without_a_cleanup_script_stop_just_goes_ready() {
    let env = env_with_site(Some(ACTIVE), false);
    let w = Workspaces::default();
    call(&w, &env, "workspaces.stop").await.unwrap();
    assert!(env.launcher.received.lock().unwrap().is_empty(), "nothing to run");
    assert_eq!(state_line(&env), "state = \"ready\"");
    assert_eq!(env.backend.events().pop().unwrap().payload["ran_cleanup"], false);
}

#[tokio::test]
async fn a_failed_stop_stays_active_and_says_where_the_log_is() {
    let env = env_with_site(Some(ACTIVE), true);
    env.launcher.outcomes.lock().unwrap().push_back(outcome(1));
    let w = Workspaces::default();
    let e = call(&w, &env, "workspaces.stop").await.unwrap_err();
    assert!(e.message.contains("cleanup failed (exit code 1); still active"), "{}", e.message);
    assert_eq!(e.detail.unwrap()["log"], "logs/site-cleanup.log");
    assert_eq!(state_line(&env), "state = \"active\"");
    assert!(env.backend.events().is_empty());
}

#[tokio::test]
async fn only_an_active_workspace_can_be_stopped() {
    let w = Workspaces::default();
    let ready = env_with_site(None, true);
    assert_eq!(call(&w, &ready, "workspaces.stop").await.unwrap_err().code, ErrorCode::InvalidParams);
    let dirty = env_with_site(
        Some("state = \"dirty\"\n\n[dirty]\nreason = \"exit code 1\"\nstep = { index = 1, count = 1, name = \"server\" }\nfailed_at = \"2026-09-16T09:12:44Z\"\nlog = \"logs/x.log\"\n"),
        true,
    );
    let e = call(&w, &dirty, "workspaces.stop").await.unwrap_err();
    assert!(e.message.contains("cleanup"), "{}", e.message);
    assert!(ready.launcher.received.lock().unwrap().is_empty() && dirty.launcher.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn remove_and_restore_bring_it_back_exactly() {
    let env = env_with_site(None, true);
    let w = Workspaces::default();
    assert_eq!(call(&w, &env, "workspaces.remove").await.unwrap(), json!({"removed": true}));
    assert!(file(&env, "site/workspace.toml").is_none());
    assert!(file(&env, ".removed/site/workspace.toml").is_some());
    let list = w.handle("workspaces.list", json!({}), &env.ctx).await.unwrap();
    assert_eq!(list["workspaces"], json!([]), "a removed workspace isn't listed");
    assert_eq!(call(&w, &env, "workspaces.status").await.unwrap_err().code, ErrorCode::NotFound);

    let back = call(&w, &env, "workspaces.restore").await.unwrap();
    assert_eq!(back["state"], "ready");
    assert_eq!(file(&env, "site/steps/01-server.sh").as_deref(), Some("true\n"));
    assert!(file(&env, ".removed/site/workspace.toml").is_none());
    let topics: Vec<String> = env.backend.events().into_iter().map(|e| e.topic).collect();
    assert_eq!(topics, ["workspaces.workspace.removed", "workspaces.workspace.restored"]);
    assert_eq!(call(&w, &env, "workspaces.restore").await.unwrap_err().code, ErrorCode::NotFound);
}

#[tokio::test]
async fn a_running_workspace_is_never_removed() {
    let env = env_with_site(Some(ACTIVE), true);
    let w = Workspaces::default();
    let e = call(&w, &env, "workspaces.remove").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
    assert!(e.message.contains("stop it first"), "{}", e.message);
    assert!(file(&env, "site/workspace.toml").is_some());
}

#[tokio::test]
async fn restore_never_overwrites_a_new_workspace_with_the_same_id() {
    let env = env_with_site(None, true);
    let w = Workspaces::default();
    call(&w, &env, "workspaces.remove").await.unwrap();
    put(&env, "site/workspace.toml", SITE);
    let e = call(&w, &env, "workspaces.restore").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
    assert!(file(&env, ".removed/site/workspace.toml").is_some(), "still there to restore later");
}

#[tokio::test]
async fn purge_asks_first_then_deletes_one_removed_workspace_for_good() {
    let env = env_with_site(None, true);
    let w = Workspaces::default();
    call(&w, &env, "workspaces.remove").await.unwrap();

    let e = w.handle("workspaces.purge", json!({"id": "site"}), &env.ctx).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::ConfirmationRequired);
    assert_eq!(e.detail, Some(json!({"workspaces": 1})));
    let stale = json!({"id": "site", "confirm": {"workspaces": 2}});
    assert_eq!(w.handle("workspaces.purge", stale, &env.ctx).await.unwrap_err().code, ErrorCode::ConfirmationRequired);
    assert!(file(&env, ".removed/site/workspace.toml").is_some(), "nothing deleted without a matching confirm");

    let ok = json!({"id": "site", "confirm": {"workspaces": 1}});
    assert_eq!(w.handle("workspaces.purge", ok, &env.ctx).await.unwrap(), json!({"purged": 1}));
    assert!(env.ctx.store.list(".removed").unwrap().is_empty());
    let purged = env.backend.events().into_iter().last().unwrap();
    assert_eq!(
        (purged.topic.as_str(), purged.payload),
        ("workspaces.workspace.purged", json!({"workspace": "site", "workspaces": 1}))
    );
    assert_eq!(call(&w, &env, "workspaces.restore").await.unwrap_err().code, ErrorCode::NotFound);
    let again = w.handle("workspaces.purge", json!({"id": "site"}), &env.ctx).await.unwrap_err();
    assert_eq!(again.code, ErrorCode::NotFound, "only a removed workspace can be purged");
}

#[tokio::test]
async fn purge_without_an_id_deletes_every_removed_workspace_and_nothing_else() {
    let env = env_with_site(None, true);
    let w = Workspaces::default();
    put(&env, "blog/workspace.toml", &SITE.replace("Site", "Blog"));
    put(&env, "blog/steps/01-server.sh", "true\n");
    put(&env, "blog/cleanup.sh", "true\n");
    put(&env, "keep/workspace.toml", &SITE.replace("Site", "Keep"));
    put(&env, "keep/steps/01-server.sh", "true\n");
    put(&env, "keep/cleanup.sh", "true\n");
    call(&w, &env, "workspaces.remove").await.unwrap();
    w.handle("workspaces.remove", json!({"id": "blog"}), &env.ctx).await.unwrap();

    let e = w.handle("workspaces.purge", json!({}), &env.ctx).await.unwrap_err();
    assert_eq!(e.detail, Some(json!({"workspaces": 2})));
    let done = w.handle("workspaces.purge", json!({"confirm": {"workspaces": 2}}), &env.ctx).await.unwrap();
    assert_eq!(done, json!({"purged": 2}));
    assert!(env.ctx.store.list(".removed").unwrap().is_empty());
    assert!(file(&env, "keep/workspace.toml").is_some(), "a workspace that wasn't removed is untouched");
    assert_eq!(
        w.handle("workspaces.purge", json!({}), &env.ctx).await.unwrap(),
        json!({"purged": 0}),
        "nothing to ask about"
    );
}
