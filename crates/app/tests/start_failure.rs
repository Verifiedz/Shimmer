//! A daemon that fails to start says why, even auto-started (#266 finding 5): its stderr goes
//! nowhere then, so the reason is written to its log and the command quotes it from there.

mod common;

use common::Home;

#[test]
fn an_auto_started_daemon_that_cant_start_says_why() {
    let home = Home::new();
    std::fs::write(home.dir.path().join("home/config.toml"), "[lanes]\nfetchers = \"many\"\n").unwrap();

    let out = home.shimmer(&["ping"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("the daemon exited during startup"), "{stderr}");
    assert!(stderr.contains("config.toml"), "the reason, not just 'run shimmer daemon': {stderr}");
    assert!(!stderr.contains("to see why"), "{stderr}");

    // And it's in the daemon's log, for a failure no one was watching.
    let logs = home.dir.path().join("home/logs");
    let log: String = std::fs::read_dir(&logs)
        .unwrap()
        .flatten()
        .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
        .collect();
    assert!(log.contains("cannot start daemon: ") && log.contains("config.toml"), "{log}");
}

#[test]
fn a_daemon_started_by_hand_prints_the_reason_once() {
    let home = Home::new();
    std::fs::write(home.dir.path().join("home/config.toml"), "[lanes]\nfetchers = \"many\"\n").unwrap();

    let out = home.shimmer(&["daemon"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert_eq!(stderr.matches("cannot start daemon").count(), 1, "once, not again from the log: {stderr}");
}

/// #307's review: a config.toml *syntax* error is several lines (where, a caret, the cause). The
/// whole of it is quoted, cause included, not just its first line.
#[test]
fn a_syntax_error_in_config_toml_is_quoted_whole() {
    let home = Home::new();
    std::fs::write(home.dir.path().join("home/config.toml"), "[lanes\nfetchers = 2\n").unwrap();

    let out = home.shimmer(&["ping"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("TOML parse error at line 1"), "{stderr}");
    assert!(stderr.contains("unclosed table"), "the cause, not just where: {stderr}");
}

/// #307's review: a RUST_LOG that leaves out shimmer::start still gets the reason written.
#[test]
fn the_reason_is_logged_whatever_rust_log_says() {
    let home = Home::new();
    std::fs::write(home.dir.path().join("home/config.toml"), "[lanes]\nfetchers = \"many\"\n").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_shimmer"))
        .arg("ping")
        .env("SHIMMER_HOME", home.dir.path().join("home"))
        .env("SHIMMER_SOCKET", home.socket())
        .env("XDG_CONFIG_HOME", home.dir.path().join("config"))
        .env("RUST_LOG", "shimmer_daemon=debug")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("lanes.fetchers"), "quoted from the log: {stderr}");
}
