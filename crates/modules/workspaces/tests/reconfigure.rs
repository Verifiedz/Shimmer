//! `workspaces.answers` and `workspaces.reconfigure` (ADR 0025 §9) through `handle`, against the
//! in-memory `Ctx` (CLAUDE.md §12 rule 13): a workspace's answers asked again, only its `[env]`
//! rewritten, only while nothing of it runs.

use serde_json::{json, Value};
use shimmer_core::testing::TestEnv;
use shimmer_core::{ErrorCode, Module, Result, StoreBackend};
use shimmer_workspaces::Workspaces;

async fn call(w: &Workspaces, env: &TestEnv, op: &str, params: Value) -> Result<Value> {
    w.handle(op, params, &env.ctx).await
}

fn file(env: &TestEnv, path: &str) -> String {
    env.backend.read("workspaces", path).unwrap().map(|b| String::from_utf8(b).unwrap()).unwrap_or_default()
}

/// A web-project workspace "site", answered.
async fn site(w: &Workspaces, env: &TestEnv) {
    let values = json!({"PROJECT_DIR": "/home/me/code/site", "CODE_EDITOR": "vscode", "LINKS": "https://a.dev"});
    call(w, env, "workspaces.new", json!({"id": "site", "template": "web-project", "values": values})).await.unwrap();
}

#[tokio::test]
async fn answers_shows_the_template_its_questions_and_the_current_answers() {
    let (env, w) = (TestEnv::new("workspaces"), Workspaces::default());
    site(&w, &env).await;
    assert_eq!(file(&env, "site/.template"), "web-project\n", "create records the template");
    let a = call(&w, &env, "workspaces.answers", json!({"id": "site"})).await.unwrap();
    assert_eq!(a["template"], "web-project");
    assert_eq!(a["questions"][0]["name"], "PROJECT_DIR");
    assert_eq!(a["answers"]["CODE_EDITOR"], "vscode");
    assert_eq!(a["answers"]["LINKS"], "https://a.dev");
}

#[tokio::test]
async fn reconfigure_rewrites_only_env_and_keeps_hand_edits() {
    let (env, w) = (TestEnv::new("workspaces"), Workspaces::default());
    site(&w, &env).await;
    // Hand edits: a step's description, and an [env] value no question asks for.
    let text = file(&env, "site/workspace.toml")
        .replace("description = \"Pull the latest code", "description = \"MY OWN: pull the latest code")
        .replace("[env]\n", "[env]\nMY_TOKEN_NAME = \"keychain-entry\"\n");
    env.ctx.store.write("site/workspace.toml", text.as_str()).unwrap();
    let events_before = env.backend.events().len();

    let r = call(
        &w,
        &env,
        "workspaces.reconfigure",
        json!({"id": "site", "values": {"CODE_EDITOR": "cursor", "LINKS": ""}}),
    )
    .await
    .unwrap();
    assert_eq!(
        r["changed"],
        json!([{"name": "CODE_EDITOR", "from": "vscode", "to": "cursor"}, {"name": "LINKS", "from": "https://a.dev", "to": ""}])
    );
    let after = file(&env, "site/workspace.toml");
    assert!(after.contains("CODE_EDITOR = \"cursor\"\n") && after.contains("LINKS = \"\"\n"), "{after}");
    assert!(after.contains("MY OWN: pull the latest code"), "hand edits outside [env] are kept: {after}");
    assert!(after.contains("MY_TOKEN_NAME = \"keychain-entry\""), "values no question asks for are kept");
    assert!(after.starts_with(&text[..text.find("[env]").unwrap() - 60]), "everything before [env] is as it was");
    assert_eq!(after.lines().filter(|l| *l == "[env]").count(), 1);
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(env.backend.events().len(), events_before + 1);
    assert_eq!(ev.topic, "workspaces.workspace.reconfigured");
    assert_eq!(ev.payload, json!({"workspace": "site", "changed": ["CODE_EDITOR", "LINKS"]}), "names, never values");

    // The same answers again: nothing changes, nothing is written or logged.
    let r = call(&w, &env, "workspaces.reconfigure", json!({"id": "site", "values": {"CODE_EDITOR": "cursor"}}))
        .await
        .unwrap();
    assert_eq!(r["changed"], json!([]));
    assert_eq!(env.backend.events().len(), events_before + 1);
    // And it still launches: the status reads the rewritten file.
    assert_eq!(call(&w, &env, "workspaces.show", json!({"id": "site"})).await.unwrap()["state"], "ready");
}

#[tokio::test]
async fn new_answers_are_checked_as_create_checks_them() {
    let (env, w) = (TestEnv::new("workspaces"), Workspaces::default());
    site(&w, &env).await;
    let before = file(&env, "site/workspace.toml");
    for (values, says) in [
        (json!({"CODE_EDITOR": "notepad"}), "CODE_EDITOR (Editor): must be one of"),
        (json!({"PROJECT_DIR": ""}), "PROJECT_DIR (Project folder) is required"),
        (json!({"PROJECT_DIR": "~/site"}), "must be an absolute path"),
        (json!({"NOPE": "x"}), "has no question 'NOPE'"),
        (json!({"CODE_EDITOR": 3}), "answers are text"),
    ] {
        let e = call(&w, &env, "workspaces.reconfigure", json!({"id": "site", "values": values})).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams);
        assert!(e.message.contains(says), "{}: {}", says, e.message);
    }
    assert_eq!(file(&env, "site/workspace.toml"), before, "nothing written");
}

#[tokio::test]
async fn only_while_nothing_of_it_runs() {
    let (env, w) = (TestEnv::new("workspaces"), Workspaces::default());
    site(&w, &env).await;
    env.ctx.store.write("site/state.toml", "state = \"active\"\n").unwrap();
    let e = call(&w, &env, "workspaces.reconfigure", json!({"id": "site", "values": {"CODE_EDITOR": "cursor"}}))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
    assert!(e.message.contains("stop it first"), "{}", e.message);
    assert!(file(&env, "site/workspace.toml").contains("CODE_EDITOR = \"vscode\""));
}

#[tokio::test]
async fn an_older_workspace_is_known_by_its_header_and_a_hand_made_one_is_refused() {
    let (env, w) = (TestEnv::new("workspaces"), Workspaces::default());
    site(&w, &env).await;
    // Made before workspaces recorded their template: the header comment names it.
    env.ctx.store.delete("site/.template").unwrap();
    let a = call(&w, &env, "workspaces.answers", json!({"id": "site"})).await.unwrap();
    assert_eq!(a["template"], "web-project");

    env.ctx.store.write("mine/workspace.toml", "[workspace]\nlabel = \"Mine\"\n").unwrap();
    let e = call(&w, &env, "workspaces.answers", json!({"id": "mine"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    assert!(
        e.message.contains("wasn't made from a template") && e.message.contains("workspaces edit mine"),
        "{}",
        e.message
    );
    let e = call(&w, &env, "workspaces.answers", json!({"id": "nope"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}
