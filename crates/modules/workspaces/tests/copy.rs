//! `workspaces.copy` (ADR 0026 §2b) through `handle`, against the in-memory `Ctx` (CLAUDE.md §12
//! rule 13): every file but the state, new answers checked as reconfigure checks them, the
//! original only read.

use serde_json::{json, Value};
use shimmer_core::testing::TestEnv;
use shimmer_core::{ErrorCode, Module, Result};
use shimmer_workspaces::Workspaces;

async fn call(w: &Workspaces, env: &TestEnv, op: &str, params: Value) -> Result<Value> {
    w.handle(op, params, &env.ctx).await
}

fn file(env: &TestEnv, path: &str) -> String {
    env.ctx.store.read_string(path).unwrap().unwrap_or_default()
}

async fn site(w: &Workspaces, env: &TestEnv) {
    let values = json!({"PROJECT_DIR": "/home/me/code/site", "LOCAL_URL": "http://localhost:3000"});
    call(w, env, "workspaces.new", json!({"id": "site", "template": "web-project", "values": values})).await.unwrap();
}

#[tokio::test]
async fn copy_makes_a_fresh_workspace_with_the_same_files_and_new_answers() {
    let (env, w) = (TestEnv::new("workspaces"), Workspaces::default());
    site(&w, &env).await;
    // The original is running, with a history: copying only reads it.
    env.ctx.store.write("site/state.toml", "state = \"active\"\n").unwrap();
    let edited = file(&env, "site/steps/01-check.sh") + "# my own edit\n";
    env.ctx.store.write("site/steps/01-check.sh", edited.as_str()).unwrap();
    let original = file(&env, "site/workspace.toml");

    let r = call(
        &w,
        &env,
        "workspaces.copy",
        json!({"id": "site", "to": "blog", "label": "My blog",
               "values": {"PROJECT_DIR": "/home/me/code/blog", "LOCAL_URL": "http://localhost:3001"}}),
    )
    .await
    .unwrap();
    assert_eq!(r["from"], "site");
    assert_eq!(
        r["changed"][0],
        json!({"name": "PROJECT_DIR", "from": "/home/me/code/site", "to": "/home/me/code/blog"})
    );

    let mut files = env.ctx.store.list("blog").unwrap();
    let mut expected: Vec<String> =
        env.ctx.store.list("site").unwrap().iter().map(|p| p.replacen("site/", "blog/", 1)).collect();
    expected.retain(|p| p != "blog/state.toml");
    files.sort();
    expected.sort();
    assert_eq!(files, expected, "every file but the state");
    assert_eq!(file(&env, "blog/steps/01-check.sh"), edited, "hand edits come along");
    assert_eq!(file(&env, "blog/.template"), "web-project\n");
    let toml = file(&env, "blog/workspace.toml");
    assert!(toml.contains("label = \"My blog\"") && toml.contains("PROJECT_DIR = \"/home/me/code/blog\""), "{toml}");
    assert_eq!(file(&env, "site/workspace.toml"), original, "the original is untouched");

    let status = call(&w, &env, "workspaces.show", json!({"id": "blog"})).await.unwrap();
    assert_eq!((status["state"].as_str(), status["last_session"].is_null()), (Some("ready"), true));
    let ev = env.backend.events().pop().unwrap();
    assert_eq!(ev.topic, "workspaces.workspace.copied");
    assert_eq!(ev.payload, json!({"from": "site", "to": "blog", "changed": ["PROJECT_DIR", "LOCAL_URL"]}));
}

#[tokio::test]
async fn copy_refuses_what_create_and_reconfigure_refuse_and_writes_nothing() {
    let (env, w) = (TestEnv::new("workspaces"), Workspaces::default());
    site(&w, &env).await;
    call(&w, &env, "workspaces.new", json!({"id": "taken", "template": "smoke-test"})).await.unwrap();
    for (params, code, says) in [
        (json!({"id": "site", "to": "Bad Name"}), ErrorCode::InvalidParams, "not a valid workspace id"),
        (json!({"id": "site", "to": "taken"}), ErrorCode::Conflict, "already a workspace 'taken'"),
        (json!({"id": "site", "to": "site"}), ErrorCode::Conflict, "already a workspace 'site'"),
        (
            json!({"id": "site", "to": "b", "values": {"CODE_EDITOR": "notepad"}}),
            ErrorCode::InvalidParams,
            "must be one of",
        ),
        (json!({"id": "site", "to": "b", "label": " "}), ErrorCode::InvalidParams, "label must be one line"),
        (json!({"id": "nope", "to": "b"}), ErrorCode::NotFound, "no workspace 'nope'"),
    ] {
        let e = call(&w, &env, "workspaces.copy", params.clone()).await.unwrap_err();
        assert_eq!(e.code, code, "{params}");
        assert!(e.message.contains(says), "{params}: {}", e.message);
    }
    assert!(env.ctx.store.list("b").unwrap().is_empty());

    // A hand-made workspace copies as it is, but has no questions to answer differently.
    let mine = "[workspace]\nlabel = \"Mine\"\n\n[[step]]\nname = \"hi\"\nmode = \"detached\"\n";
    env.ctx.store.write("mine/workspace.toml", mine).unwrap();
    env.ctx.store.write("mine/steps/01-hi.sh", "echo hi\n").unwrap();
    call(&w, &env, "workspaces.copy", json!({"id": "mine", "to": "mine-2"})).await.unwrap();
    assert_eq!(file(&env, "mine-2/steps/01-hi.sh"), "echo hi\n");
    // A broken one isn't copied: the copy couldn't launch either.
    env.ctx.store.write("broken/workspace.toml", "[workspace]\nlabel = \"B\"\n").unwrap();
    let e = call(&w, &env, "workspaces.copy", json!({"id": "broken", "to": "broken-2"})).await.unwrap_err();
    assert!(e.message.contains("[[step]]"), "{}", e.message);
    let e = call(&w, &env, "workspaces.copy", json!({"id": "mine", "to": "mine-3", "values": {"A": "b"}}))
        .await
        .unwrap_err();
    assert!(e.message.contains("wasn't made from a template"), "{}", e.message);
}
