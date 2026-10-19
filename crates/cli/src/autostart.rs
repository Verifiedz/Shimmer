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
    let mut child = Command::new(exe)
        .arg("daemon")
        // `socket` may itself have come from $SHIMMER_SOCKET or $XDG_RUNTIME_DIR; pinning it makes
        // the daemon bind exactly where this client is about to look.
        .env("SHIMMER_SOCKET", absolute(socket))
        .env("SHIMMER_HOME", absolute(&shimmer_proto::paths::shimmer_home()))
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
                Error::unavailable(format!(
                    "the daemon exited during startup ({status}); run 'shimmer daemon' to see why"
                ))
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
