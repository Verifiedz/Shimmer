//! Start the daemon when nobody is listening, then connect (CLAUDE.md §2: like `tmux` or
//! `ssh-agent`, the user never starts it by hand). The daemon is this same binary run as
//! `shimmer daemon`; the CLI never links the daemon crate (§3 rule 2).

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use shimmer_core::{Error, Result};

use crate::client::Client;

/// Opening the store and recovering staged transactions can take a moment on a big home.
const START_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(50);

/// Launch `<exe> daemon` detached, bound to `socket`, and wait until it completes a handshake.
pub async fn start_and_connect(exe: &Path, socket: &Path) -> Result<Client> {
    // The daemon runs in `/`, never in the folder this command happens to be in (a USB drive's
    // folder couldn't be unmounted while it runs, #266 finding 3). So the folder and socket it's
    // given are made absolute first: a relative `SHIMMER_HOME=play` means this command's `play`.
    let absolute = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let home = absolute(&shimmer_proto::paths::shimmer_home());
    // A second earlier, so a log line written in the same second as the start still counts.
    let started = chrono::Utc::now() - chrono::Duration::seconds(1);
    let mut child = Command::new(exe)
        .arg("daemon")
        // `socket` may itself have come from $SHIMMER_SOCKET or $XDG_RUNTIME_DIR; pinning it makes
        // the daemon bind exactly where this client is about to look.
        .env("SHIMMER_SOCKET", absolute(socket))
        .env("SHIMMER_HOME", &home)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Own process group, so Ctrl-C on this command does not reach the daemon.
        .process_group(0)
        .spawn()
        .map_err(|e| Error::unavailable(format!("cannot start daemon ({}): {e}", exe.display())))?;

    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match Client::connect(socket).await {
            Ok(client) => return Ok(client),
            Err(e) if !e.nobody_listening() => return Err(e.into_error(socket)),
            Err(_) => {}
        }
        // Exited early: usually another daemon already owns this home (a second `shimmer` raced us
        // and won), or the home is unusable. One last look covers the race.
        if let Ok(Some(status)) = child.try_wait() {
            return Client::connect(socket).await.map_err(|_| {
                Error::unavailable(match start_failure(&home.join("logs"), started) {
                    Some(why) => format!("the daemon exited during startup ({status}): {why}"),
                    None => format!("the daemon exited during startup ({status}); run 'shimmer daemon' to see why"),
                })
            });
        }
        if Instant::now() >= deadline {
            return Err(Error::unavailable(format!(
                "started the daemon but it was not listening on {} after {}s; run 'shimmer daemon' to see why",
                socket.display(),
                START_TIMEOUT.as_secs()
            )));
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Why the daemon just failed to start, from its log: its stderr goes nowhere when auto-started,
/// so `run_daemon` also logs the failure (#266 finding 5). The newest `daemon.log.<date>` file's
/// last "cannot start daemon: " record, if it was written since `since`; an older one is about an
/// earlier start, not this one. The whole record: a `config.toml` syntax error is several lines
/// (where, the line with a caret, the cause), and its first line alone drops the cause (#307's
/// review). A record ends where the next one starts, at a line beginning with its timestamp.
fn start_failure(logs: &Path, since: chrono::DateTime<chrono::Utc>) -> Option<String> {
    const MARK: &str = "cannot start daemon: ";
    let newest = std::fs::read_dir(logs)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("daemon.log"))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())?;
    let text = std::fs::read_to_string(newest.path()).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.iter().rposition(|l| l.contains(MARK) && stamped(l))?;
    let at = chrono::DateTime::parse_from_rfc3339(lines[start].split_whitespace().next()?).ok()?;
    if at < since {
        return None;
    }
    let (_, first) = lines[start].split_once(MARK)?;
    let rest = lines[start + 1..].iter().take_while(|l| !stamped(l)).filter(|l| !l.trim().is_empty());
    let mut why = first.trim().to_owned();
    for line in rest {
        why.push_str("\n  ");
        why.push_str(line.trim_end());
    }
    Some(why)
}

/// Whether a log line starts a record: it begins with an RFC 3339 timestamp.
fn stamped(line: &str) -> bool {
    line.split_whitespace().next().is_some_and(|w| chrono::DateTime::parse_from_rfc3339(w).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(dir: &Path, name: &str, text: &str) {
        std::fs::write(dir.join(name), text).unwrap();
    }

    #[test]
    fn the_reason_is_the_newest_logs_last_start_failure_since_this_start() {
        let dir = tempfile::tempdir().unwrap();
        let since = chrono::DateTime::parse_from_rfc3339("2026-10-19T12:00:00Z").unwrap().to_utc();
        assert_eq!(start_failure(dir.path(), since), None, "no log at all");

        log(
            dir.path(),
            "daemon.log.2026-10-19",
            "2026-10-19T11:00:00.000001Z ERROR shimmer::start: cannot start daemon: an old one\n\
             2026-10-19T12:00:01.000001Z  INFO shimmer_daemon: starting\n\
             2026-10-19T12:00:02.000001Z ERROR shimmer::start: cannot start daemon: config.toml: bad lanes\n\
             2026-10-19T12:00:02.000002Z  INFO shimmer_daemon: something after it\n",
        );
        assert_eq!(start_failure(dir.path(), since).as_deref(), Some("config.toml: bad lanes"));

        // A multi-line record: the cause on its last line is kept, and the next record isn't.
        log(
            dir.path(),
            "daemon.log.2026-10-19",
            "2026-10-19T12:00:02.000001Z ERROR shimmer::start: cannot start daemon: invalid_params: config.toml: TOML parse error at line 1, column 7\n\
             \x20 |\n\
             1 | [lanes\n\
             \x20 |       ^\n\
             unclosed table, expected `]`\n\
             2026-10-19T12:00:02.000002Z  INFO shimmer_daemon: something after it\n",
        );
        let why = start_failure(dir.path(), since).unwrap();
        assert!(why.starts_with("invalid_params: config.toml: TOML parse error at line 1, column 7\n"), "{why}");
        assert!(why.contains("1 | [lanes") && why.ends_with("unclosed table, expected `]`"), "{why}");
        assert!(!why.contains("something after it"), "{why}");

        // Only an earlier start's failure: not this one's reason.
        let later = chrono::DateTime::parse_from_rfc3339("2026-10-19T13:00:00Z").unwrap().to_utc();
        assert_eq!(start_failure(dir.path(), later), None);
    }
}
