//! Shared harness for the end-to-end tests: the real `shimmer` binary in a throwaway `$SHIMMER_HOME`.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// The LeetCode-style collection the records tests use (crates/modules/records/tests/fixtures).
pub const LEETCODE: &str = include_str!("../../../modules/records/tests/fixtures/leetcode.toml");

pub struct Home {
    pub dir: TempDir,
}

impl Home {
    pub fn new() -> Self {
        Self { dir: TempDir::new().unwrap() }
    }

    /// A home with a LeetCode-style collection already in it. A fresh install has no
    /// collections (ADR 0023), so tests that use one write it in themselves.
    pub fn with_leetcode() -> Self {
        let home = Self::new();
        let dir = home.dir.path().join("home/data/records/collections");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("leetcode.toml"), LEETCODE).unwrap();
        home
    }

    pub fn socket(&self) -> PathBuf {
        self.dir.path().join("d.sock")
    }

    pub fn shimmer(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_shimmer"))
            .args(args)
            .env("SHIMMER_HOME", self.dir.path().join("home"))
            .env("SHIMMER_SOCKET", self.socket())
            // The CLI's own settings (the active command pack, ADR 0013): never the developer's.
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .output()
            .unwrap()
    }
}

/// Never leave a daemon running behind a failed assertion.
impl Drop for Home {
    fn drop(&mut self) {
        if self.socket().exists() {
            let _ = self.shimmer(&["shutdown"]);
        }
    }
}

pub fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

pub fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// `shimmer args…`'s output; fails the test, with what it printed, if the command failed.
pub fn ok(home: &Home, args: &[&str]) -> String {
    let o = home.shimmer(args);
    assert!(o.status.success(), "shimmer {args:?} failed: {}{}", stdout(&o), stderr(&o));
    stdout(&o)
}

/// `shimmer args…`'s error output; fails the test if the command succeeded.
pub fn fails(home: &Home, args: &[&str]) -> String {
    let o = home.shimmer(args);
    assert!(!o.status.success(), "shimmer {args:?} should have failed: {}", stdout(&o));
    stderr(&o)
}

pub fn wait_gone(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!path.exists(), "daemon did not remove its socket after shutdown");
}
