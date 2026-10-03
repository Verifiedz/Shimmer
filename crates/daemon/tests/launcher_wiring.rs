//! Capability-scoped `ctx.launcher` wiring (ADR 0010 §4): a module gets a real `Launcher`
//! only if its manifest declares `"process"`; every other module keeps the default
//! `unavailable` stub. End-to-end through the real `Daemon::start`, not a unit test of
//! `Core::new` directly, so it also proves `$SHIMMER_HOME`/`$SHIMMER_SOCKET` reach the real
//! backend correctly.

mod common;

use std::sync::Arc;

use serde_json::json;

use common::{stop, Client, Env, ProcessUser};

#[tokio::test]
async fn without_the_capability_ctx_launcher_stays_the_default_unavailable_stub() {
    let env = Env::new();
    let d = env.try_start(vec![Arc::new(ProcessUser { declares_capability: false })]).await.unwrap();
    let mut c = Client::connect(&env.sock).await;

    let err = c.call("procuser.launch", json!({"workspace_dir": "deep-work"})).await.unwrap_err();
    assert_eq!(err.code, shimmer_core::ErrorCode::Unavailable);
    assert!(err.message.contains("no launch backend registered"), "{}", err.message);

    stop(d).await;
}

#[tokio::test]
async fn with_the_capability_ctx_launcher_is_the_real_backend() {
    let env = Env::new();
    let d = env.try_start(vec![Arc::new(ProcessUser { declares_capability: true })]).await.unwrap();
    let mut c = Client::connect(&env.sock).await;

    // No cleanup.sh exists in this empty $SHIMMER_HOME, so the real backend's own
    // "script not found" path fires — a different message than the default stub's,
    // proving a different `Launcher` actually ran.
    let err = c.call("procuser.launch", json!({"workspace_dir": "deep-work"})).await.unwrap_err();
    assert_eq!(err.code, shimmer_core::ErrorCode::Unavailable);
    assert!(err.message.contains("launch script not found"), "{}", err.message);

    stop(d).await;
}

#[tokio::test]
async fn a_module_with_the_capability_can_actually_launch() {
    let env = Env::new();
    let workspace_dir = env.home.path().join("data/workspaces/deep-work");
    std::fs::create_dir_all(&workspace_dir).unwrap();
    std::fs::write(workspace_dir.join("cleanup.sh"), "true\n").unwrap();

    let d = env.try_start(vec![Arc::new(ProcessUser { declares_capability: true })]).await.unwrap();
    let mut c = Client::connect(&env.sock).await;

    let data = c.call("procuser.launch", json!({"workspace_dir": "deep-work"})).await.unwrap();
    assert_eq!(data["session_id_is_empty"], false, "a real backend must mint a non-empty session id");

    stop(d).await;
}
