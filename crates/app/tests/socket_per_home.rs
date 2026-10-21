//! ADR 0039: each Shimmer folder gets its own daemon on its own default socket, a daemon names the
//! folder it serves, and nothing is ever answered by another folder's daemon. Runs the real binary
//! with the *default* socket (no `SHIMMER_SOCKET`), which no other e2e test does.

mod common;

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{stderr, stdout, Home};
use tempfile::TempDir;

/// `shimmer args…` for `home`, with the default socket: `XDG_RUNTIME_DIR` is `runtime` (Linux
/// reads it; macOS keeps the socket in the folder), and `SHIMMER_SOCKET` is only set if given.
fn run(home: &Home, runtime: &Path, socket: Option<&Path>, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_shimmer"));
    cmd.args(args)
        .env("SHIMMER_HOME", home.dir.path().join("home"))
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_CONFIG_HOME", home.dir.path().join("config"))
        .env_remove("SHIMMER_SOCKET");
    if let Some(socket) = socket {
        cmd.env("SHIMMER_SOCKET", socket);
    }
    cmd.output().unwrap()
}

fn ok(home: &Home, runtime: &Path, args: &[&str]) -> String {
    let o = run(home, runtime, None, args);
    assert!(o.status.success(), "shimmer {args:?} failed: {}", stderr(&o));
    stdout(&o)
}

/// The socket `home`'s daemon listens on by default, as the binary resolves it.
fn socket_of(home: &Home, runtime: &Path) -> std::path::PathBuf {
    let home_dir = home.dir.path().join("home");
    if cfg!(target_os = "linux") {
        shimmer_proto::paths::default_socket_in(&home_dir, Some(runtime), shimmer_proto::paths::owner_uid())
    } else {
        shimmer_proto::paths::default_socket_in(&home_dir, None, shimmer_proto::paths::owner_uid())
    }
}

/// Stop `home`'s daemon and wait for its socket to go.
fn stop(home: &Home, runtime: &Path) {
    let socket = socket_of(home, runtime);
    let _ = run(home, runtime, None, &["shutdown"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn two_folders_get_two_daemons_and_each_command_reaches_its_own() {
    let runtime = TempDir::new().unwrap();
    let rt = runtime.path();
    let (a, b) = (Home::with_leetcode(), Home::new());

    // Both start, side by side, on their own sockets.
    assert!(ok(&a, rt, &["records", "collections"]).contains("leetcode"));
    let collections_b = ok(&b, rt, &["records", "collections"]);
    assert!(!collections_b.contains("leetcode"), "B is answered from B's data, not A's: {collections_b}");
    let (sa, sb) = (socket_of(&a, rt), socket_of(&b, rt));
    assert_ne!(sa, sb);
    assert!(sa.exists() && sb.exists(), "{sa:?} {sb:?}");

    // A command for B pointed at A's daemon is refused, never answered from A's data.
    let o = run(&b, rt, Some(&sa), &["records", "collections"]);
    assert!(!o.status.success(), "{}", stdout(&o));
    assert!(stderr(&o).contains("serves") && stderr(&o).contains("SHIMMER_SOCKET"), "{}", stderr(&o));

    stop(&a, rt);
    stop(&b, rt);
}

#[test]
fn another_folders_daemon_is_not_already_running() {
    let runtime = TempDir::new().unwrap();
    let rt = runtime.path();
    let (a, b) = (Home::new(), Home::new());
    ok(&a, rt, &["ping"]);

    // A daemon for B told to use A's socket: A's daemon answers there, but for another folder, so
    // it's a real failure (exit 1), not "already running" (3) (ADR 0039 §4).
    let mut child = Command::new(env!("CARGO_BIN_EXE_shimmer"))
        .arg("daemon")
        .env("SHIMMER_HOME", b.dir.path().join("home"))
        .env("XDG_RUNTIME_DIR", rt)
        .env("SHIMMER_SOCKET", socket_of(&a, rt))
        .env_remove("XPC_SERVICE_NAME")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("`shimmer daemon` kept running: {:?}", child.wait_with_output().unwrap());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(stderr(&o).contains("serves"), "{}", stderr(&o));

    // A's daemon is untouched.
    ok(&a, rt, &["ping"]);
    stop(&a, rt);
}

/// ADR 0039 §5: an explicit `SHIMMER_SOCKET` too long for a socket is never shortened, and the
/// command says exactly that instead of "cannot connect".
#[test]
fn a_shimmer_socket_too_long_says_so() {
    let runtime = TempDir::new().unwrap();
    let home = Home::new();
    let long = home.dir.path().join(format!("{}.sock", "x".repeat(200)));
    let o = run(&home, runtime.path(), Some(&long), &["ping"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("a socket path can be at most"), "{}", stderr(&o));
}
