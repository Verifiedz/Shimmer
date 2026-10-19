//! #266 finding 3: the daemon runs in `/`, never in the folder of whatever command started it (so
//! that folder, a USB drive's say, can be unmounted), and a relative `SHIMMER_HOME` still means
//! the folder the starting command named. Linux reads a process's working folder from `/proc`.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// The pid of the `shimmer daemon` listening on `socket`, from `/proc`.
fn daemon_pid(socket: &Path) -> Option<u32> {
    let want = format!("SHIMMER_SOCKET={}", socket.display());
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
        let cmdline = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
        let environ = std::fs::read(entry.path().join("environ")).unwrap_or_default();
        let is_daemon = cmdline.split(|b| *b == 0).any(|arg| arg == b"daemon");
        let ours = environ.split(|b| *b == 0).any(|var| var == want.as_bytes());
        if is_daemon && ours {
            return Some(pid);
        }
    }
    None
}

fn shimmer(cwd: &Path, home: &Path, socket: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_shimmer"))
        .args(args)
        .current_dir(cwd)
        .env("SHIMMER_HOME", home)
        .env("SHIMMER_SOCKET", socket)
        .env("XDG_CONFIG_HOME", cwd.join("config"))
        .output()
        .unwrap()
}

#[test]
fn an_auto_started_daemon_runs_in_root_and_a_relative_home_still_means_that_folder() {
    let tmp = TempDir::new().unwrap();
    let drive = tmp.path().join("drive");
    let home = drive.join("play");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.toml"), "[desktop]\nshow = false\n").unwrap();
    let socket = tmp.path().join("d.sock");

    // Started from the "drive", with the folder given relative to it.
    let o = shimmer(&drive, Path::new("play"), &socket, &["ping"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let deadline = Instant::now() + Duration::from_secs(10);
    let pid = loop {
        if let Some(pid) = daemon_pid(&socket) {
            break pid;
        }
        assert!(Instant::now() < deadline, "no daemon found for {}", socket.display());
        std::thread::sleep(Duration::from_millis(50));
    };
    let cwd: PathBuf = std::fs::read_link(format!("/proc/{pid}/cwd")).unwrap();
    assert_eq!(cwd, Path::new("/"), "the daemon must not hold the drive's folder");

    // It serves the folder the relative path named: its files land there, not under `/`.
    assert!(home.join(".daemon.lock").exists(), "the daemon locked drive/play");

    let o = shimmer(tmp.path(), &home, &socket, &["shutdown"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let deadline = Instant::now() + Duration::from_secs(10);
    while socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}
