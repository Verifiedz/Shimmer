//! Capability-scoped `ctx.http` wiring (ADR 0027 §4): a module gets the real `HttpBackend`
//! only if its manifest declares `"network"`; every other module keeps the default
//! `DisabledHttp` stub. End-to-end through the real `Daemon::start`, not a unit test of
//! `Core::new` directly.

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

use serde_json::json;

use common::{stop, Client, Env, NetworkUser};

#[tokio::test]
async fn without_the_capability_ctx_http_stays_the_default_disabled_stub() {
    let env = Env::new();
    let d = env.try_start(vec![Arc::new(NetworkUser { declares_capability: false })]).await.unwrap();
    let mut c = Client::connect(&env.sock).await;

    let err = c.call("netuser.fetch", json!({"url": "http://127.0.0.1:1/"})).await.unwrap_err();
    assert_eq!(err.code, shimmer_core::ErrorCode::Unavailable);
    assert!(err.message.contains("http gateway not enabled"), "{}", err.message);

    stop(d).await;
}

#[tokio::test]
async fn with_the_capability_ctx_http_is_the_real_backend() {
    let env = Env::new();
    let d = env.try_start(vec![Arc::new(NetworkUser { declares_capability: true })]).await.unwrap();
    let mut c = Client::connect(&env.sock).await;

    // Nothing listens on port 1 -- the real backend's own connection-refused path fires, a
    // different message than the disabled stub's, proving a different HttpBackend actually
    // ran.
    let err = c.call("netuser.fetch", json!({"url": "http://127.0.0.1:1/"})).await.unwrap_err();
    assert_eq!(err.code, shimmer_core::ErrorCode::Unavailable);
    assert!(!err.message.contains("http gateway not enabled"), "{}", err.message);

    stop(d).await;
}

#[tokio::test]
async fn a_module_with_the_capability_can_actually_fetch() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            // One response, then the connection closes: say so, as the gateway's own TestServer
            // does (#134).
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nok");
        }
    });

    let env = Env::new();
    let d = env.try_start(vec![Arc::new(NetworkUser { declares_capability: true })]).await.unwrap();
    let mut c = Client::connect(&env.sock).await;

    let data = c.call("netuser.fetch", json!({"url": format!("http://{addr}/")})).await.unwrap();
    assert_eq!(data["status"], 200);
    assert_eq!(data["body"], "ok");

    stop(d).await;
}
