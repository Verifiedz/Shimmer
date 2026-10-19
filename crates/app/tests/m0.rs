//! M0's definition of done (CLAUDE.md §14): a `shimmer` command reaches the daemon and back.
//! Runs the real binary as a user would, in a throwaway `$SHIMMER_HOME`, starting from no daemon.

mod common;

use common::{stderr, stdout, wait_gone, Home};

#[test]
fn a_command_reaches_the_daemon_and_back() {
    let home = Home::new();
    assert!(!home.socket().exists());

    // No daemon yet: the CLI starts one and answers.
    let o = home.shimmer(&["ping"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    assert!(stdout(&o).starts_with("pong (daemon up "), "{}", stdout(&o));
    assert!(home.socket().exists());

    // The same daemon answers the next command.
    let o = home.shimmer(&["manifest", "--json"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let m: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(m["protocol"], shimmer_proto::PROTOCOL_VERSION);
    assert!(m["lanes"].as_array().unwrap().iter().any(|l| l["id"] == "default"));

    // Daemon errors come back with their stable code and exit 1.
    let o = home.shimmer(&["call", "nope.nothing"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).starts_with("shimmer: unknown_op: "), "{}", stderr(&o));

    let o = home.shimmer(&["shutdown"]);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "daemon stopped");
    wait_gone(&home.socket());

    // Stopping a stopped daemon does not start one.
    let o = home.shimmer(&["shutdown"]);
    assert!(o.status.success());
    assert!(stderr(&o).contains("not running"));
    assert!(!home.socket().exists());
}

#[test]
fn an_explicit_socket_never_auto_starts() {
    let home = Home::new();
    let elsewhere = home.dir.path().join("elsewhere.sock");
    let o = home.shimmer(&["ping", "--socket", elsewhere.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("unavailable: cannot connect"), "{}", stderr(&o));
    assert!(!elsewhere.exists());
    assert!(!home.socket().exists());
}

#[test]
fn bad_usage_exits_2_with_help() {
    let home = Home::new();
    let o = home.shimmer(&["frobnicate"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("unknown command 'frobnicate'"));
    assert!(stderr(&o).contains("usage: shimmer"));
    assert!(!home.socket().exists(), "bad usage must not start a daemon");
}
