//! Capability-scoped `ctx.desktop` wiring (ADR 0032 §1–§2): a module gets the real desktop
//! backend only if its manifest declares `"desktop"`; every other keeps `Desktop::unavailable`.
//! End to end through the real `Daemon::start`. `[desktop] show = false`, so nothing ever pops
//! up on a developer's screen.

mod common;

use std::sync::Arc;

use serde_json::json;

use common::{stop, Client, DesktopUser, Env};

fn quiet_desktop(env: &Env) {
    std::fs::write(env.home.path().join("config.toml"), "[desktop]\nshow = false\n").unwrap();
}

#[tokio::test]
async fn without_the_capability_ctx_desktop_fails_closed() {
    let env = Env::new();
    quiet_desktop(&env);
    let d = env.try_start(vec![Arc::new(DesktopUser { declares_capability: false })]).await.unwrap();
    let mut c = Client::connect(&env.sock).await;
    let err = c.call("deskuser.show", json!({})).await.unwrap_err();
    assert_eq!(err.code, shimmer_core::ErrorCode::Unavailable);
    assert!(err.message.contains("\"desktop\" capability"), "{}", err.message);
    stop(d).await;
}

#[tokio::test]
async fn with_the_capability_ctx_desktop_is_the_real_backend() {
    let env = Env::new();
    quiet_desktop(&env);
    let d = env.try_start(vec![Arc::new(DesktopUser { declares_capability: true })]).await.unwrap();
    let mut c = Client::connect(&env.sock).await;
    // The real backend with show = false logs the note and succeeds; the stub would fail.
    assert_eq!(c.call("deskuser.show", json!({})).await.unwrap(), json!({"shown": true}));
    stop(d).await;
}
