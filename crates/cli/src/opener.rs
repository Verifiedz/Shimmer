//! Opening a `role = "path"` field's value with whatever the OS already associates with its
//! file type, for `records open` (ADR 0044). The same posture `editor.rs` already takes for
//! `$EDITOR`: the client acting on a value it already has, not a decision about data — this
//! can't live behind `Ctx`/the daemon, since `Ctx.launcher` is deliberately scoped to a
//! workspace's own namespace-relative scripts and never takes an arbitrary path (ADR 0010).

use shimmer_core::{Error, Result};

/// Open `path` and wait for the opener program to return (not for whatever it opens —
/// `xdg-open`/`open` exit as soon as the target application has started). Any failure's
/// message includes `path` itself, so it can be copied straight from the error (ADR 0044 §3).
pub fn open(path: &str) -> Result<()> {
    let status = command_for(path)
        .status()
        .map_err(|e| Error::unavailable(format!("couldn't start an opener for '{path}': {e}")))?;
    if !status.success() {
        return Err(Error::unavailable(format!("opening '{path}' failed ({status}); here's the path to copy: {path}")));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn command_for(path: &str) -> std::process::Command {
    let mut c = std::process::Command::new("open");
    c.arg(path);
    c
}

#[cfg(target_os = "windows")]
fn command_for(path: &str) -> std::process::Command {
    let mut c = std::process::Command::new("cmd");
    // The empty "" is the window title `start` wants before the target when it's quoted.
    c.args(["/C", "start", "", path]);
    c
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn command_for(path: &str) -> std::process::Command {
    let mut c = std::process::Command::new("xdg-open");
    c.arg(path);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_right_opener_for_this_platform() {
        let c = command_for("/tmp/resume.pdf");
        #[cfg(target_os = "macos")]
        assert_eq!(c.get_program(), "open");
        #[cfg(target_os = "windows")]
        assert_eq!(c.get_program(), "cmd");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(c.get_program(), "xdg-open");
        assert!(c.get_args().any(|a| a == "/tmp/resume.pdf"));
    }
}
