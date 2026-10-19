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
