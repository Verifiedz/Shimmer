//! The one binary. `shimmer daemon` and `shimmer mockd` run servers; every other command belongs to the
//! CLI. TUI dispatch arrives with its crate. Clients hold no business logic (§2), so nothing
//! else belongs here.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use shimmer_core::{ErrorCode, Module};
use shimmer_daemon::{Daemon, DaemonConfig};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

/// Daily rotation, oldest files pruned once there are more than this many (§7 of CLAUDE.md).
/// `tracing-appender` can dip below its cap by one on prune, so this is set one higher than
/// the week of history we actually want to guarantee.
const LOG_RETAIN_FILES: usize = 8;

/// `shimmer daemon`'s exit status when another daemon already runs for this folder and answers on
/// its socket (ADR 0034 §5). Not a failure: a login service (`shimmer startup on`) lists it as
/// success, so it isn't restarted every few seconds while a daemon a command started is running.
const ALREADY_RUNNING: u8 = 3;

fn main() -> ExitCode {
    // The vault first (ADR 0041 §3): `--vault NAME`, or the one `shimmer vaults use` chose, runs
    // this binary again with that folder's SHIMMER_HOME and SHIMMER_SOCKET, before any thread exists.
    if let Some(code) = shimmer_cli::vaults::enter() {
        return code;
    }
    match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime.block_on(run()),
        Err(e) => {
            eprintln!("shimmer: couldn't start: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> ExitCode {
    // Started through a command-pack link (`ikuzo deep-work`, ADR 0013 §10): the whole line is
    // the alias's, even `ikuzo daemon`. Started as `shimmer`, or anything else, nothing changes.
    if let Some(alias) = std::env::args().next().as_deref().and_then(shimmer_cli::invoked_alias) {
        init_stderr_logging();
        return shimmer_cli::run_as_alias(alias, std::env::args().skip(1).collect()).await;
    }
    match std::env::args().nth(1).as_deref() {
        Some("daemon") => {
            // `--quiet`: a vault copy's first start (ADR 0041 §5a). Nothing else is taken.
            let quiet = match std::env::args().skip(2).collect::<Vec<_>>().as_slice() {
                [] => false,
                [flag] if flag == "--quiet" => true,
                _ => {
                    eprintln!("usage: shimmer daemon [--quiet]");
                    return ExitCode::from(2);
                }
            };
            // Autostart (crates/cli/src/autostart.rs) nulls the child's stderr, so this is the
            // only trace of a cold-start crash unless it also lands in a file (ADR 0007).
            let _guard = init_daemon_logging(&shimmer_proto::paths::shimmer_home());
            run_daemon(quiet).await
        }
        Some("mockd") => {
            init_stderr_logging();
            run_mockd(std::env::args().skip(2).collect()).await
        }
        _ => {
            init_stderr_logging();
            shimmer_cli::run(std::env::args().skip(1).collect()).await
        }
    }
}

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

fn init_stderr_logging() {
    tracing_subscriber::fmt().with_env_filter(env_filter()).with_writer(std::io::stderr).init();
}

/// Logs to stderr (visible when run in the foreground) and, best-effort, to
/// `$SHIMMER_HOME/logs/daemon.log.<date>` (visible when autostarted, whose stderr is discarded).
/// Rotates at UTC midnight and keeps at most `LOG_RETAIN_FILES` files; older ones are deleted,
/// not archived elsewhere. The returned guard must stay alive for the process lifetime or
/// buffered lines are dropped.
fn init_daemon_logging(home: &Path) -> Option<WorkerGuard> {
    // A start failure is printed to stderr as a plain line already (`run_daemon`); the log file
    // gets it through tracing, and stderr would otherwise show it twice.
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(env_filter())
        .with_filter(tracing_subscriber::filter::filter_fn(|m| m.target() != START_FAILED));
    // Absolute: the daemon moves to `/` once it starts (#266 finding 3), and the appender opens
    // each day's new file by this path.
    let logs_dir = std::path::absolute(home).unwrap_or_else(|_| home.to_path_buf()).join("logs");
    // Pre-create the directory: on a fresh `$SHIMMER_HOME`, the appender would otherwise try (and
    // fail, noisily) to list it for pruning before the first file creates it.
    let appender = std::fs::create_dir_all(&logs_dir).map_err(|e| e.to_string()).and_then(|_| {
        RollingFileAppender::builder()
            .rotation(Rotation::DAILY)
            .filename_prefix("daemon.log")
            .max_log_files(LOG_RETAIN_FILES)
            .build(&logs_dir)
            .map_err(|e| e.to_string())
    });
    match appender {
        Ok(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            // A failed start is always written, whatever RUST_LOG says: auto-start reads the reason
            // from here (#307's review).
            let always = format!("{START_FAILED}=error")
                .parse()
                .map(|d| env_filter().add_directive(d))
                .unwrap_or_else(|_| env_filter());
            let file_layer = tracing_subscriber::fmt::layer().with_ansi(false).with_writer(writer).with_filter(always);
            tracing_subscriber::registry().with(stderr_layer).with(file_layer).init();
            Some(guard)
        }
        Err(e) => {
            tracing_subscriber::registry().with(stderr_layer).init();
            eprintln!(
                "shimmer: cannot open daemon log files under {} ({e}); daemon logs will only go to stderr",
                logs_dir.display()
            );
            None
        }
    }
}

async fn run_daemon(quiet: bool) -> ExitCode {
    // Every module the daemon runs. A new module is one more line here.
    let modules: Vec<Arc<dyn Module>> = vec![
        Arc::new(shimmer_records::Records::default()),
        Arc::new(shimmer_workspaces::Workspaces::default()),
        Arc::new(shimmer_fetchers::Fetchers::default()),
        Arc::new(shimmer_automations::Automations::default()),
        Arc::new(shimmer_notify::Notify::default()),
    ];
    let mut config = DaemonConfig::from_env();
    config.quiet = quiet;
    // Run in `/`, never in the folder whatever started it was in (#266 finding 3): a drive the
    // daemon sits in can't be unmounted. Its folder and socket are made absolute first, so a
    // relative `SHIMMER_HOME` still means the folder it named.
    let absolute = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    config.home = absolute(&config.home);
    config.socket = absolute(&config.socket);
    if let Err(e) = std::env::set_current_dir("/") {
        tracing::warn!(error = %e, "couldn't change the daemon's working folder to /");
    }
    let (socket, home) = (config.socket.clone(), config.home.clone());
    let daemon = match Daemon::start(config, modules).await {
        Ok(d) => d,
        // Refused because one already runs. A lock held by a daemon that doesn't answer is still a
        // real failure (below), so a login service retries it.
        Err(e) if e.code == ErrorCode::Conflict => match answers_soon(&socket, &home).await {
            // Only this folder's daemon counts as "already running" (ADR 0039 §4).
            Answer::ThisFolder => {
                eprintln!("shimmer: already running: a daemon answers on {} ({e})", socket.display());
                return already_running();
            }
            // Another folder's daemon on this socket is a real conflict: fail, so it's retried and seen.
            Answer::Other(why) => {
                eprintln!("shimmer: cannot start daemon: {why}");
                return ExitCode::FAILURE;
            }
            Answer::Nobody => {
                eprintln!("shimmer: cannot start daemon: {e}");
                return ExitCode::FAILURE;
            }
        },
        Err(e) => {
            // Into the log file too: an auto-started daemon's stderr goes nowhere, and the CLI
            // quotes this line when it reports the failure (#266 finding 5).
            tracing::error!(target: START_FAILED, "cannot start daemon: {e}");
            eprintln!("shimmer: cannot start daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    let handle = daemon.shutdown_handle();
    tokio::spawn(async move {
        wait_for_signal().await;
        handle.shutdown();
    });
    // Also returns when a `core.shutdown` request stops the daemon.
    daemon.wait().await;
    ExitCode::SUCCESS
}

/// The log target of a failed start. `crates/cli/src/autostart.rs` finds the line by its words,
/// "cannot start daemon: ", so they stay the same.
const START_FAILED: &str = "shimmer::start";

/// How long a daemon that holds the lock may take to start answering. It takes the lock first,
/// then recovers transactions, opens the index and starts modules, and only then binds the
/// socket: one starting up at login while a command starts another must not look dead (#241's
/// review), or the login service restarts it in a loop.
const STARTING_UP: std::time::Duration = std::time::Duration::from_secs(5);

/// Who answers on a socket a starting daemon couldn't take.
enum Answer {
    /// The daemon for this Shimmer folder.
    ThisFolder,
    /// Something else: another folder's daemon (ADR 0039 §4), or one we can't talk to. Why.
    Other(String),
    /// Nothing within [`STARTING_UP`].
    Nobody,
}

/// Who answers on `socket` within [`STARTING_UP`]: a full handshake, so another folder's daemon
/// (which names its folder in `welcome`) is never taken for this one's.
async fn answers_soon(socket: &Path, home: &Path) -> Answer {
    use shimmer_cli::{Client, ConnectError};
    let deadline = tokio::time::Instant::now() + STARTING_UP;
    loop {
        match Client::connect_for(socket, home).await {
            Ok(_) => return Answer::ThisFolder,
            Err(e @ ConnectError::OtherHome { .. }) => return Answer::Other(e.into_error(socket).message),
            Err(e) if !e.nobody_listening() && tokio::time::Instant::now() >= deadline => {
                return Answer::Other(e.into_error(socket).message)
            }
            Err(_) if tokio::time::Instant::now() >= deadline => return Answer::Nobody,
            Err(_) => {}
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// [`ALREADY_RUNNING`], or success when launchd started this daemon: launchd can't be told that
/// an exit status means success, so any other would be restarted (ADR 0034 §3, §5). launchd names
/// the job in `XPC_SERVICE_NAME`; only our own LaunchAgent's label counts, since terminals set it
/// too (Terminal.app to `0`, iTerm2 and VS Code to their own, #241's review).
fn already_running() -> ExitCode {
    match std::env::var("XPC_SERVICE_NAME") {
        Ok(job) if job == LAUNCH_AGENT => ExitCode::SUCCESS,
        _ => ExitCode::from(ALREADY_RUNNING),
    }
}

/// The LaunchAgent's label, as `shimmer startup on` writes it (ADR 0034 §3).
const LAUNCH_AGENT: &str = "io.github.verifiedz.shimmer";

async fn run_mockd(args: Vec<String>) -> ExitCode {
    let options = match shimmer_mockd::Command::parse(args) {
        Ok(shimmer_mockd::Command::Run(options)) => options,
        Ok(shimmer_mockd::Command::Help) => {
            println!("{}", shimmer_mockd::USAGE);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("shimmer mockd: {e}\n\n{}", shimmer_mockd::USAGE);
            return ExitCode::from(2);
        }
    };
    let mockd = match shimmer_mockd::Mockd::start(options).await {
        Ok(m) => m,
        Err(e) => {
            eprintln!("shimmer mockd: {e}");
            return ExitCode::FAILURE;
        }
    };
    let handle = mockd.shutdown_handle();
    tokio::spawn(async move {
        wait_for_signal().await;
        handle.shutdown();
    });
    // Also returns when a `core.shutdown` request stops it.
    mockd.wait().await;
    ExitCode::SUCCESS
}

async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
        return std::future::pending().await;
    };
    tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #38 / #11: simulate a daemon that has been running (and rotating) for a month by
    /// pre-seeding a logs dir with more dated files than the retention cap, then starting
    /// logging fresh as the real daemon does on every launch. `RollingFileAppender` prunes on
    /// construction, so this does not need to wait for an actual day boundary to observe it.
    #[test]
    fn stale_dated_logs_past_the_retention_cap_are_pruned_on_startup() {
        let home = tempfile::tempdir().unwrap();
        let logs_dir = home.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        for day in 1..=30 {
            std::fs::write(logs_dir.join(format!("daemon.log.2026-09-{day:02}")), "old\n").unwrap();
        }

        let guard = init_daemon_logging(home.path());
        assert!(guard.is_some(), "a writable temp dir must produce a working appender");
        drop(guard);

        let names: Vec<String> = std::fs::read_dir(&logs_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), LOG_RETAIN_FILES, "{names:?}");
        assert!(names.iter().all(|n| n.starts_with("daemon.log.")), "{names:?}");
    }
}
