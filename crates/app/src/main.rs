//! The one binary. `shimmer daemon` and `shimmer mockd` run servers; every other command belongs to the
//! CLI. TUI dispatch arrives with its crate. Clients hold no business logic (§2), so nothing
//! else belongs here.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use shimmer_core::Module;
use shimmer_daemon::{Daemon, DaemonConfig};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

/// Daily rotation, oldest files pruned once there are more than this many (§7 of CLAUDE.md).
/// `tracing-appender` can dip below its cap by one on prune, so this is set one higher than
/// the week of history we actually want to guarantee.
const LOG_RETAIN_FILES: usize = 8;

#[tokio::main]
async fn main() -> ExitCode {
    // Started through a command-pack link (`ikuzo deep-work`, ADR 0013 §10): the whole line is
    // the alias's, even `ikuzo daemon`. Started as `shimmer`, or anything else, nothing changes.
    if let Some(alias) = std::env::args().next().as_deref().and_then(shimmer_cli::invoked_alias) {
        init_stderr_logging();
        return shimmer_cli::run_as_alias(alias, std::env::args().skip(1).collect()).await;
    }
    match std::env::args().nth(1).as_deref() {
        Some("daemon") => {
            // Autostart (crates/cli/src/autostart.rs) nulls the child's stderr, so this is the
            // only trace of a cold-start crash unless it also lands in a file (ADR 0007).
            let _guard = init_daemon_logging(&shimmer_proto::paths::shimmer_home());
            run_daemon().await
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
    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_filter(env_filter());
    let logs_dir = home.join("logs");
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
            let file_layer =
                tracing_subscriber::fmt::layer().with_ansi(false).with_writer(writer).with_filter(env_filter());
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

async fn run_daemon() -> ExitCode {
    // Every module the daemon runs. A new module is one more line here.
    let modules: Vec<Arc<dyn Module>> = vec![
        Arc::new(shimmer_records::Records::default()),
        Arc::new(shimmer_workspaces::Workspaces::default()),
        Arc::new(shimmer_fetchers::Fetchers::default()),
        Arc::new(shimmer_notify::Notify::default()),
    ];
    let daemon = match Daemon::start(DaemonConfig::from_env(), modules).await {
        Ok(d) => d,
        Err(e) => {
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
