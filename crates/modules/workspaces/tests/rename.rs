//! `workspaces.rename` (ADR 0026 §2a) through `handle`, against the in-memory `Ctx` (CLAUDE.md
//! §12 rule 13): every file moves in one transaction, only while nothing of it runs.

use serde_json::{json, Value};
use shimmer_core::testing::TestEnv;
use shimmer_core::{ErrorCode, Module, Result};
use shimmer_workspaces::Workspaces;

async fn call(w: &Workspaces, env: &TestEnv, op: &str, params: Value) -> Result<Value> {
    w.handle(op, params, &env.ctx).await
}

async fn site(w: &Workspaces, env: &TestEnv, values: Value) {
    call(w, env, "workspaces.new", json!({"id": "site", "template": "web-project", "values": values})).await.unwrap();
}

#[tokio::test]
async fn rename_moves_every_file_and_the_workspace_keeps_working() {
    let (env, w) = (TestEnv::new("workspaces"), Workspaces::default());
    site(&w, &env, json!({"PROJECT_DIR": "/home/me/code/site"})).await;
    env.ctx.store.write("site/state.toml", "state = \"ready\"\n").unwrap();
    let before: Vec<String> = env.ctx.store.list("site").unwrap();

    let r = call(&w, &env, "workspaces.rename", json!({"id": "site", "to": "portfolio"})).await.unwrap();
    assert_eq!(r, json!({"from": "site", "to": "portfolio", "notes": []}));
    let after: Vec<String> = env.ctx.store.list("portfolio").unwrap();
    let moved: Vec<String> = before.iter().map(|p| p.replacen("site/", "portfolio/", 1)).collect();
    assert_eq!(after, moved, "every file, .template and state.toml included");
    assert!(env.ctx.store.list("site").unwrap().is_empty());
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(
        (ev.topic.as_str(), ev.payload),
        ("workspaces.workspace.renamed", json!({"from": "site", "to": "portfolio"}))
    );

    assert_eq!(call(&w, &env, "workspaces.show", json!({"id": "portfolio"})).await.unwrap()["state"], "ready");
    let a = call(&w, &env, "workspaces.answers", json!({"id": "portfolio"})).await.unwrap();
    assert_eq!(a["template"], "web-project", "still knows its template");
    let e = call(&w, &env, "workspaces.show", json!({"id": "site"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn rename_says_what_stays_under_the_old_name() {
    let (env, w) = (TestEnv::new("workspaces"), Workspaces::default());
    site(&w, &env, json!({"PROJECT_DIR": "/home/me/code/site", "BROWSER_WINDOW": "separate"})).await;
    let r = call(&w, &env, "workspaces.rename", json!({"id": "site", "to": "portfolio"})).await.unwrap();
    let note = r["notes"][0].as_str().unwrap();
    assert!(note.contains("browser-profiles/site ") && note.ends_with("browser-profiles/portfolio"), "{note}");
}

#[tokio::test]
async fn rename_refuses_anything_unsafe_and_changes_nothing() {
    let (env, w) = (TestEnv::new("workspaces"), Workspaces::default());
    site(&w, &env, json!({"PROJECT_DIR": "/home/me/code/site"})).await;
    call(&w, &env, "workspaces.new", json!({"id": "taken", "template": "smoke-test"})).await.unwrap();
    let files = env.ctx.store.list("site").unwrap();
    for (to, code, says) in [
        ("Bad Name", ErrorCode::InvalidParams, "not a valid workspace id"),
        ("site", ErrorCode::InvalidParams, "already called that"),
        ("taken", ErrorCode::Conflict, "already a workspace 'taken'"),
    ] {
        let e = call(&w, &env, "workspaces.rename", json!({"id": "site", "to": to})).await.unwrap_err();
        assert_eq!(e.code, code, "{to}");
        assert!(e.message.contains(says), "{to}: {}", e.message);
    }
    env.ctx.store.write("site/state.toml", "state = \"active\"\n").unwrap();
    let e = call(&w, &env, "workspaces.rename", json!({"id": "site", "to": "portfolio"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
    assert!(e.message.contains("stop it first"), "{}", e.message);
    assert_eq!(env.ctx.store.list("site").unwrap().len(), files.len() + 1, "all still there (plus the state file)");
    let e = call(&w, &env, "workspaces.rename", json!({"id": "nope", "to": "x"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn rename_refuses_when_the_state_file_cant_be_read() {
    // Fail closed (ADR 0026 §2a): an unreadable state may hide a running workspace.
    let (env, w) = (TestEnv::new("workspaces"), Workspaces::default());
    site(&w, &env, json!({"PROJECT_DIR": "/home/me/code/site"})).await;
    env.ctx.store.write("site/state.toml", "this is not toml [").unwrap();
    let e = call(&w, &env, "workspaces.rename", json!({"id": "site", "to": "portfolio"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
    assert!(e.message.contains("state file can't be read"), "{}", e.message);
    assert!(env.ctx.store.list("portfolio").unwrap().is_empty(), "nothing moved");
    // remove still works on a broken workspace: restore can undo it (ADR 0026 §2).
    call(&w, &env, "workspaces.remove", json!({"id": "site"})).await.unwrap();
}
