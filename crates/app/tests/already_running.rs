//! `shimmer daemon` when a daemon already runs for the folder (ADR 0034 §5): a distinct exit
//! status, so a login service treats it as done instead of restarting it every few seconds.

mod common;

use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{ok, stderr, Home};

/// `shimmer daemon` with `extra` env, killed if it's still running after a few seconds: a daemon
/// that starts when it shouldn't fails the test instead of hanging it.
fn daemon(home: &Home, extra: &[(&str, &str)]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_shimmer"))
        .arg("daemon")
        .env("SHIMMER_HOME", home.dir.path().join("home"))
        .env("SHIMMER_SOCKET", home.socket())
        .env_remove("XPC_SERVICE_NAME")
        .envs(extra.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("`shimmer daemon` kept running: {:?}", child.wait_with_output().unwrap());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn a_second_daemon_exits_already_running() {
    let home = Home::new();
    ok(&home, &["ping"]);

    let o = daemon(&home, &[]);
    assert_eq!(o.status.code(), Some(3), "{}", stderr(&o));
    assert!(stderr(&o).contains("already running"), "{}", stderr(&o));

    // launchd can't be told 3 means success, so under it that's 0; a macOS terminal's `0` isn't it.
    let o = daemon(&home, &[("XPC_SERVICE_NAME", "io.github.verifiedz.shimmer")]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let o = daemon(&home, &[("XPC_SERVICE_NAME", "0")]);
    assert_eq!(o.status.code(), Some(3), "{}", stderr(&o));

    // The first daemon is untouched.
    ok(&home, &["ping"]);
}

#[test]
fn a_lock_nobody_answers_for_is_still_a_failure() {
    let home = Home::new();
    let dir = home.dir.path().join("home");
    std::fs::create_dir_all(&dir).unwrap();
    // Held here, by something that isn't a daemon: no socket answers for it.
    let lock = std::fs::File::create(dir.join(".daemon.lock")).unwrap();
    lock.try_lock().unwrap();

    let o = daemon(&home, &[]);
    assert_eq!(o.status.code(), Some(1), "retried by a login service: {}", stderr(&o));
    assert!(stderr(&o).contains("another daemon already owns"), "{}", stderr(&o));
}
