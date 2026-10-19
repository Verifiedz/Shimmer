//! The real `ctx.desktop` backend (ADR 0032 §2), shared by every module that declares the
//! `"desktop"` capability: `notify-send` on Linux, `osascript` on macOS.
//!
//! The text is always passed as program arguments, never put into a script or a shell, so
//! nothing in a message can run as code. `[desktop] show = false` in `config.toml` logs each
//! note instead of showing it: for machines with no screen, and for tests, which must never pop
//! anything up on a developer's screen.

use std::time::Duration;

use async_trait::async_trait;
use shimmer_core::{DesktopBackend, DesktopNote, Error, Result, Urgency};
use tokio::process::Command;

/// A desktop that doesn't answer in this long is treated as unavailable.
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Linux,
    Mac,
    Other,
}

impl Os {
    pub fn this() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::Mac
        } else {
            Self::Other
        }
    }
}

pub struct RealDesktopBackend {
    /// `false`: log each note instead of showing it (`[desktop] show = false`).
    show: bool,
    os: Os,
}

impl RealDesktopBackend {
    pub fn new(show: bool) -> Self {
        Self { show, os: Os::this() }
    }
}

/// The program and its arguments that show `note` on `os`. Pure, so tests can check exactly
/// what would run without running it.
pub fn command_for(os: Os, note: &DesktopNote) -> Option<(&'static str, Vec<String>)> {
    match os {
        Os::Linux => {
            let urgency = match note.urgency {
                Urgency::Low => "low",
                Urgency::Normal => "normal",
                Urgency::Critical => "critical",
            };
            Some((
                "notify-send",
                vec![
                    "--app-name=Shimmer".into(),
                    format!("--urgency={urgency}"),
                    "--".into(),
                    note.title.clone(),
                    note.body.clone(),
                ],
            ))
        }
        // `on run argv` reads the text from the arguments: it is never part of the script.
        // `--` ends osascript's options, so a title like `-e` is text, not another script line.
        Os::Mac => Some((
            "osascript",
            vec![
                "-e".into(),
                "on run argv".into(),
                "-e".into(),
                "display notification (item 2 of argv) with title (item 1 of argv)".into(),
                "-e".into(),
                "end run".into(),
                "--".into(),
                note.title.clone(),
                note.body.clone(),
            ],
        )),
        Os::Other => None,
    }
}

#[async_trait]
impl DesktopBackend for RealDesktopBackend {
    async fn show(&self, note: &DesktopNote) -> Result<()> {
        if !self.show {
            tracing::info!(title = %note.title, body = %note.body, urgency = ?note.urgency, "desktop note (not shown: [desktop] show = false)");
            return Ok(());
        }
        let Some((program, args)) = command_for(self.os, note) else {
            return Err(Error::unavailable("desktop notifications work on Linux and macOS for now"));
        };
        let child = Command::new(program).args(&args).kill_on_drop(true).output();
        let output = match tokio::time::timeout(TIMEOUT, child).await {
            Err(_) => return Err(Error::unavailable(format!("{program} didn't answer within {}s", TIMEOUT.as_secs()))),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::unavailable(match self.os {
                    Os::Linux => "notify-send isn't installed (it's in the libnotify-bin or libnotify package)",
                    _ => "osascript isn't available",
                }))
            }
            Ok(Err(e)) => return Err(Error::unavailable(format!("can't run {program}: {e}"))),
            Ok(Ok(output)) => output,
        };
        if output.status.success() {
            return Ok(());
        }
        let said = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        // Most often a daemon started with no desktop session (over SSH, or at login before
        // the desktop exported its environment): say so plainly.
        let hint = if self.os == Os::Linux && std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            " (no desktop session: the daemon has no DBUS_SESSION_BUS_ADDRESS)"
        } else {
            ""
        };
        Err(Error::unavailable(format!("{program} failed{hint}: {said}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(title: &str, body: &str, urgency: Urgency) -> DesktopNote {
        DesktopNote { title: title.into(), body: body.into(), urgency }
    }

    #[test]
    fn linux_passes_the_text_as_arguments_after_the_options() {
        let (program, args) = command_for(Os::Linux, &note("--help", "x; rm -rf ~", Urgency::Critical)).unwrap();
        assert_eq!(program, "notify-send");
        assert_eq!(args, ["--app-name=Shimmer", "--urgency=critical", "--", "--help", "x; rm -rf ~"]);
    }

    #[test]
    fn macos_never_puts_the_text_into_the_script() {
        let title = "\" & (do shell script \"touch /tmp/pwned\") & \"";
        let (program, args) = command_for(Os::Mac, &note(title, "body", Urgency::Low)).unwrap();
        assert_eq!(program, "osascript");
        let script: Vec<&String> = args.iter().take(6).collect();
        assert!(script.iter().all(|a| !a.contains("pwned")), "{script:?}");
        assert_eq!(&args[6..], ["--", title, "body"]);
    }

    /// Runs the real osascript with the arguments `command_for` builds, the display line
    /// swapped for one that hands back what the script received: a title that looks like an
    /// osascript option must arrive as the text, not be read as a flag.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_osascript_reads_a_dash_title_as_text() {
        for title in ["-e", "-s", "-l", "-i", "--", "-e do shell script \"touch /tmp/pwned\""] {
            let (program, mut args) = command_for(Os::Mac, &note(title, "-e", Urgency::Normal)).unwrap();
            args[3] = r#"return (item 1 of argv) & "|" & (item 2 of argv) & "|" & (count of argv)"#.into();
            let output = std::process::Command::new(program).args(&args).output().unwrap();
            assert!(output.status.success(), "{title}: {}", String::from_utf8_lossy(&output.stderr));
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), format!("{title}|-e|2"));
        }
    }

    #[test]
    fn other_systems_have_no_command() {
        assert_eq!(command_for(Os::Other, &note("t", "b", Urgency::Normal)), None);
    }

    #[tokio::test]
    async fn show_false_shows_nothing_and_succeeds() {
        let backend = RealDesktopBackend { show: false, os: Os::Other };
        backend.show(&note("t", "b", Urgency::Normal)).await.unwrap();
    }
}
