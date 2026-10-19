//! `shimmer startup` (ADR 0034) through the real binary. Only what never reaches the operating
//! system's service manager: `on` itself is tested against a fake one in `crates/cli/src/startup.rs`.

mod common;

use std::process::{Command, Output};

use common::{stderr, stdout, Home};

/// `shimmer startup args…` with HOME and the config folder inside the test's temp folder, so a
/// developer's own login service is never read or touched.
fn startup(home: &Home, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_shimmer"))
        .arg("startup")
        .args(args)
        .env("HOME", home.dir.path().join("me"))
        .env("XDG_CONFIG_HOME", home.dir.path().join("config"))
        .env("SHIMMER_HOME", home.dir.path().join("home"))
        .env("SHIMMER_SOCKET", home.socket())
        .output()
        .unwrap()
}

#[test]
fn off_until_turned_on() {
    let home = Home::new();
    let o = startup(&home, &["status"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).starts_with("off: Shimmer's daemon starts when a command needs it."), "{}", stdout(&o));

    let o = startup(&home, &["off"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "startup is already off");
    assert!(!home.socket().exists(), "never starts the daemon");
}

#[test]
fn its_help_and_its_mistakes() {
    let home = Home::new();
    let o = startup(&home, &[]);
    assert!(o.status.success());
    assert!(stdout(&o).contains("usage: shimmer startup <command>"), "{}", stdout(&o));
    let o = startup(&home, &["--help"]);
    assert!(stdout(&o).contains("usage: shimmer startup <command>"), "{}", stdout(&o));

    let o = startup(&home, &["now"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("it's on, off or status"), "{}", stderr(&o));
    assert!(stderr(&o).contains("usage: shimmer startup"), "the startup help, not the top one: {}", stderr(&o));
}
