//! The real `LaunchBackend` (ADR 0010). Lands incrementally, one `SpawnMode`/concern per
//! sub-branch of the `m3-launch-backend` umbrella; this slice replaces the `session_id`
//! placeholder with real minting from the injected `Clock`, on top of the previous slices'
//! spawn mechanics and env injection.
//!
//! Linux/macOS (`cfg(unix)`) only, same scoping as ADR 0010 §7 — Windows is sketched there,
//! not implemented. Not yet wired into any `Ctx` (that is the capability-scoped-wiring
//! sub-branch); this module is exercised only by its own tests until then.

// Unwired until the capability-scoped-wiring sub-branch has `Core::new` construct this
// behind the module's `"process"` capability — remove once it does.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use shimmer_core::launcher::env_names;
use shimmer_core::store::validate_path;
use shimmer_core::{Clock, Error, LaunchBackend, LaunchStep, Result, SpawnMode, Step, StepOutcome};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

/// Implements [`LaunchBackend`] for real, spawning scripts under
/// `$SHIMMER_HOME/data/workspaces/<workspace_dir>/`.
pub struct RealLaunchBackend {
    /// `$SHIMMER_HOME`. The backend is the one place allowed to hold this as a raw path
    /// (ADR 0010 §4) — nothing upstream of it ever sees one.
    home: PathBuf,
    /// `$SHIMMER_SOCKET`, injected into every spawned script so it can call back in
    /// (CLAUDE.md §10.1).
    socket: PathBuf,
    /// Source for minted session ids (ADR 0010 §9) — never `SystemTime::now()`.
    clock: Clock,
    /// Random for a real daemon process — the caller's responsibility to draw fresh each
    /// time a real backend is constructed (e.g. `rand::random()` once at daemon startup),
    /// never generated inside this type, same injected-not-hidden pattern as `clock`. Tests
    /// pass a fixed value for reproducibility. See [`mint_session_id`] for why this exists
    /// alongside `sequence` (ADR 0010 §9's "Amendment" below).
    instance_id: u64,
    /// Folded into each minted session id's random component alongside `instance_id` (see
    /// [`mint_session_id`]). Resets to 0 whenever a new backend is constructed, so a fresh
    /// backend over a fresh `Clock::fake` with the same `instance_id` reproduces the exact
    /// same id sequence for the same sequence of calls.
    sequence: AtomicU64,
}

impl RealLaunchBackend {
    pub fn new(home: PathBuf, socket: PathBuf, clock: Clock, instance_id: u64) -> Self {
        Self { home, socket, clock, instance_id, sequence: AtomicU64::new(0) }
    }
}

#[async_trait]
impl LaunchBackend for RealLaunchBackend {
    async fn run(&self, step: &LaunchStep, cancel: &CancellationToken) -> Result<StepOutcome> {
        let script = resolve_script(&self.home, &step.workspace_dir, &step.step)?;
        let session_id = mint_session_id(&self.clock, self.instance_id, &self.sequence);
        match step.mode {
            SpawnMode::Detached => run_detached(&self.home, &self.socket, &script, step, &session_id).await,
            SpawnMode::Supervised { timeout } => {
                run_supervised(&self.home, &self.socket, &script, step, &session_id, timeout, cancel).await
            }
        }
    }
}

/// Minted here, not by the calling module (ADR 0010 §9) — fresh per launch attempt, from
/// the injected `Clock`, never `SystemTime::now()`.
///
/// **Amendment to ADR 0010 §9's sketch** (`Ulid::from_parts(timestamp_ms, rand::random())`):
/// using fresh OS randomness for every call makes ids collision-resistant but defeats the
/// reproducibility a module needs for deterministic ordering tests, while a pure in-process
/// counter (an earlier version of this function) is reproducible but *not*
/// collision-resistant across daemon lifetimes — it resets to 0 on every restart, so two
/// daemon runs (a restart, or two machines sharing a git-synced `$SHIMMER_HOME`, CLAUDE.md
/// §1.4) whose clocks agree on the millisecond could mint the identical session id, which
/// matters because `session_id` lands in the event log (`workspaces.session.launched`/
/// `.dirty` payloads) and in a log file's name — a collision there is a real correctness
/// bug, not a cosmetic one.
///
/// This resolves both: the 80-bit random field is `instance_id`'s low 48 bits as the high
/// bits, `sequence`'s low 32 bits as the low bits. `instance_id` is random per daemon
/// process (the caller's job to draw fresh for a real backend), so two different daemon
/// lifetimes essentially never collide even if their clocks and sequences agree;
/// `sequence` alone still gives exact, deterministic ordering within one process, since a
/// fresh backend's `sequence` always starts at 0.
fn mint_session_id(clock: &Clock, instance_id: u64, sequence: &AtomicU64) -> String {
    let seq = sequence.fetch_add(1, Ordering::SeqCst) & 0xFFFF_FFFF;
    let random = ((instance_id & 0xFFFF_FFFF_FFFF) as u128) << 32 | seq as u128;
    Ulid::from_parts(clock.now().timestamp_millis() as u64, random).to_string()
}

/// `Step` -> a real path under the workspace's own directory, never outside it (ADR 0010
/// §2a/§4). Only escape-safety is checked here (`validate_path`, already applied to every
/// other module's paths); the stricter `[a-z0-9][a-z0-9_-]*` charset on `name` lands in the
/// step-name-validation sub-branch (closes #14).
fn resolve_script(home: &Path, workspace_dir: &str, step: &Step) -> Result<PathBuf> {
    validate_path(workspace_dir)?;
    let rel = match step {
        Step::Launch { index, name, .. } => {
            validate_path(name)?;
            format!("{index:02}-{name}.sh")
        }
        Step::Cleanup => "cleanup.sh".to_string(),
    };
    let rel = if matches!(step, Step::Cleanup) { rel } else { format!("steps/{rel}") };
    Ok(home.join("data/workspaces").join(workspace_dir).join(rel))
}

/// ADR 0010 §3 "Detached": own process group, stdio to null (same pattern
/// `crates/cli/src/autostart.rs` already uses for the daemon autostart spawn), `run` returns
/// as soon as the process is spawned, and a second task reaps it independently so it never
/// becomes a zombie — without ever retaining a handle (§10.2: "no handle retained").
async fn run_detached(
    home: &Path,
    socket: &Path,
    script: &Path,
    step: &LaunchStep,
    session_id: &str,
) -> Result<StepOutcome> {
    if !script.exists() {
        return Err(Error::unavailable(format!("launch script not found: {}", script.display())));
    }
    let mut cmd = Command::new("sh");
    cmd.arg(script);
    inject_env(&mut cmd, home, socket, step, session_id);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|e| Error::unavailable(format!("cannot spawn {}: {e}", script.display())))?;
    // Reaping only — no store write, no event, no module state (ADR 0010 §3). Dropping the
    // JoinHandle is deliberate: holding it would be the retained handle §10.2 forbids.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(StepOutcome {
        session_id: session_id.to_string(),
        exit_code: None,
        timed_out: false,
        // Detached stdio goes to null (above), so there is nothing to capture.
        log_path: String::new(),
    })
}

/// ADR 0010 §3 "Supervised": same process-group mechanism as `Detached`, but the handle is
/// retained and raced against a timeout and the caller's cancellation — both converge on the
/// same process-group kill (§9: "both paths converge on the same kill"). Unlike `Detached`,
/// whose stdio goes to null, this captures the child's stdout/stderr to `logs/` (§3 "Logs").
async fn run_supervised(
    home: &Path,
    socket: &Path,
    script: &Path,
    step: &LaunchStep,
    session_id: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<StepOutcome> {
    if !script.exists() {
        return Err(Error::unavailable(format!("launch script not found: {}", script.display())));
    }
    let log_rel = log_path_for(step);
    let log_abs = home.join(&log_rel);
    if let Some(parent) = log_abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stdout_file = std::fs::File::create(&log_abs)?;
    let stderr_file = stdout_file.try_clone()?;

    let mut cmd = Command::new("sh");
    cmd.arg(script);
    inject_env(&mut cmd, home, socket, step, session_id);
    cmd.stdin(Stdio::null()).stdout(Stdio::from(stdout_file)).stderr(Stdio::from(stderr_file));
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|e| Error::unavailable(format!("cannot spawn {}: {e}", script.display())))?;
    let pid = child.id();

    let (exit_code, timed_out) = tokio::select! {
        status = child.wait() => {
            let status = status.map_err(|e| Error::unavailable(format!("waiting on {}: {e}", script.display())))?;
            (status.code(), false)
        }
        _ = tokio::time::sleep(timeout) => {
            kill_process_group(pid);
            let _ = child.wait().await;
            (None, true)
        }
        _ = cancel.cancelled() => {
            kill_process_group(pid);
            let _ = child.wait().await;
            (None, false)
        }
    };

    Ok(StepOutcome { session_id: session_id.to_string(), exit_code, timed_out, log_path: log_rel })
}

/// Relative to `$SHIMMER_HOME`, matching `docs/protocol.md`'s `workspace_dirty` detail shape
/// (ADR 0010 §3 "Logs"). Provisional naming — folds in the real session id once the
/// session-id sub-branch mints one; nothing downstream depends on this exact shape yet.
fn log_path_for(step: &LaunchStep) -> String {
    let label = match &step.step {
        Step::Launch { name, .. } => name.as_str(),
        Step::Cleanup => "cleanup",
    };
    format!("logs/{}-{}.log", step.workspace_dir, label)
}

/// Negative PID targets the whole process group (ADR 0010 §3), so a script's own children —
/// e.g. a build tool's compiler subprocesses — die with it, not just the immediate child.
/// A no-op if the process already exited and its id could not be read.
fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        let pgid = nix::unistd::Pid::from_raw(-(pid as i32));
        let _ = nix::sys::signal::kill(pgid, nix::sys::signal::Signal::SIGKILL);
    }
}

/// The six vars every spawned script gets (CLAUDE.md §10.1), then `workspace.toml`'s
/// `[env]` for every key that is not one of those six — the override guard (ADR 0010 §2):
/// "a `workspace.toml` that (accidentally or otherwise) declares `SHIMMER_SOCKET = "..."`
/// cannot redirect a script's daemon connection." Checked by membership, not by call order,
/// since `Command::env` is last-write-wins for a repeated key.
fn inject_env(cmd: &mut Command, home: &Path, socket: &Path, step: &LaunchStep, session_id: &str) {
    let injected = [
        (env_names::WORKSPACE_ID, step.workspace_id.clone()),
        (env_names::WORKSPACE_DIR, step.workspace_dir.clone()),
        (env_names::HOME, home.display().to_string()),
        (env_names::SOCKET, socket.display().to_string()),
        (env_names::SESSION_ID, session_id.to_string()),
        (env_names::PLATFORM, platform().to_string()),
    ];
    for (k, v) in &injected {
        cmd.env(k, v);
    }
    for (k, v) in &step.user_env {
        if !injected.iter().any(|(ik, _)| ik == k) {
            cmd.env(k, v);
        }
    }
}

fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use tempfile::TempDir;

    use super::*;

    /// Arbitrary but fixed, so tests that care about `session_id` are reproducible. A real
    /// daemon must draw this fresh (e.g. `rand::random()`) at startup instead.
    const TEST_INSTANCE_ID: u64 = 0x1234_5678_9abc;

    fn backend(home: &TempDir) -> RealLaunchBackend {
        backend_with_clock(home, Clock::system())
    }

    fn backend_with_clock(home: &TempDir, clock: Clock) -> RealLaunchBackend {
        backend_with_clock_and_instance(home, clock, TEST_INSTANCE_ID)
    }

    fn backend_with_clock_and_instance(home: &TempDir, clock: Clock, instance_id: u64) -> RealLaunchBackend {
        RealLaunchBackend::new(home.path().to_path_buf(), home.path().join("d.sock"), clock, instance_id)
    }

    fn write_step_script(home: &Path, workspace_dir: &str, name: &str, body: &str) -> PathBuf {
        let dir = home.join("data/workspaces").join(workspace_dir).join("steps");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("01-{name}.sh"));
        std::fs::write(&path, body).unwrap();
        path
    }

    fn step(workspace_dir: &str, name: &str) -> LaunchStep {
        LaunchStep {
            workspace_id: "deep-work".into(),
            workspace_dir: workspace_dir.into(),
            step: Step::Launch { index: 1, count: 1, name: name.into() },
            mode: SpawnMode::Detached,
            user_env: vec![],
        }
    }

    #[tokio::test]
    async fn detached_script_outlives_the_backend_that_spawned_it() {
        let home = TempDir::new().unwrap();
        let marker = home.path().join("marker");
        write_step_script(home.path(), "deep-work", "setup", &format!("sleep 0.3 && touch {}\n", marker.display()));

        {
            let backend = backend(&home);
            let outcome = backend.run(&step("deep-work", "setup"), &CancellationToken::new()).await.unwrap();
            assert_eq!(outcome.exit_code, None);
            assert!(!outcome.timed_out);
            // `backend` (and the `Launcher`/`LaunchBackend` value in general) drops here —
            // ADR 0010 §10: the process must still be running afterwards.
        }

        assert!(!marker.exists(), "script should not have finished yet");
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(marker.exists(), "detached process did not outlive the backend that spawned it");
    }

    const ECHO_ALL_INJECTED: &str = "echo \"$SHIMMER_WORKSPACE_ID|$SHIMMER_WORKSPACE_DIR|$SHIMMER_HOME|$SHIMMER_SOCKET|$SHIMMER_SESSION_ID|$SHIMMER_PLATFORM\"";

    #[tokio::test]
    async fn injects_all_six_env_names() {
        let home = TempDir::new().unwrap();
        let out = home.path().join("out");
        write_step_script(home.path(), "deep-work", "setup", &format!("{ECHO_ALL_INJECTED} > {}\n", out.display()));

        let b = backend(&home);
        let outcome = b.run(&step("deep-work", "setup"), &CancellationToken::new()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(!outcome.session_id.is_empty());
        let seen = std::fs::read_to_string(&out).unwrap();
        let expected = format!(
            "deep-work|deep-work|{}|{}|{}|{}",
            home.path().display(),
            home.path().join("d.sock").display(),
            outcome.session_id,
            platform()
        );
        assert_eq!(seen.trim(), expected);
    }

    #[tokio::test]
    async fn distinct_session_ids_across_separate_launch_attempts() {
        let home = TempDir::new().unwrap();
        write_step_script(home.path(), "deep-work", "setup", "true\n");
        let b = backend(&home);

        let first = b.run(&step("deep-work", "setup"), &CancellationToken::new()).await.unwrap();
        let second = b.run(&step("deep-work", "setup"), &CancellationToken::new()).await.unwrap();

        assert_ne!(first.session_id, second.session_id);
    }

    #[tokio::test]
    async fn session_id_sequence_is_reproducible_when_the_fake_clock_resets_to_the_same_state() {
        let home = TempDir::new().unwrap();
        write_step_script(home.path(), "deep-work", "setup", "true\n");
        let start = DateTime::parse_from_rfc3339("2026-10-03T00:00:00Z").unwrap().to_utc();

        async fn three_ids(home: &TempDir, start: DateTime<Utc>) -> Vec<String> {
            let b = backend_with_clock(home, Clock::fake(start));
            let mut ids = Vec::new();
            for _ in 0..3 {
                ids.push(b.run(&step("deep-work", "setup"), &CancellationToken::new()).await.unwrap().session_id);
            }
            ids
        }

        // A fresh backend over a fresh fake clock reset to the same start reproduces the
        // exact same sequence — what lets a module write deterministic tests against
        // session ordering (not just distinctness).
        let first = three_ids(&home, start).await;
        let second = three_ids(&home, start).await;
        assert_eq!(first, second, "resetting the clock and starting a fresh backend must reproduce the same sequence");
        assert_eq!(first.iter().collect::<HashSet<_>>().len(), 3, "still distinct within one run");
    }

    #[tokio::test]
    async fn different_daemon_instances_do_not_collide_even_with_the_same_clock_and_sequence() {
        // Regression test for the gap a pure sequence-only design has: two daemon
        // lifetimes (e.g. a restart) whose clocks and per-process sequences happen to
        // agree must still mint different session ids.
        let home = TempDir::new().unwrap();
        write_step_script(home.path(), "deep-work", "setup", "true\n");
        let start = DateTime::parse_from_rfc3339("2026-10-03T00:00:00Z").unwrap().to_utc();

        let a = backend_with_clock_and_instance(&home, Clock::fake(start), 0x1111_1111_1111);
        let b = backend_with_clock_and_instance(&home, Clock::fake(start), 0x2222_2222_2222);
        let id_a = a.run(&step("deep-work", "setup"), &CancellationToken::new()).await.unwrap().session_id;
        let id_b = b.run(&step("deep-work", "setup"), &CancellationToken::new()).await.unwrap().session_id;

        assert_ne!(id_a, id_b, "two different instance_ids at the same clock+sequence state must not collide");
    }

    #[tokio::test]
    async fn user_env_cannot_override_an_injected_name() {
        let home = TempDir::new().unwrap();
        let out = home.path().join("out");
        write_step_script(
            home.path(),
            "deep-work",
            "setup",
            &format!("echo \"$SHIMMER_SOCKET|$MY_VAR\" > {}\n", out.display()),
        );

        let mut s = step("deep-work", "setup");
        s.user_env = vec![
            ("SHIMMER_SOCKET".into(), "attacker-controlled".into()),
            ("MY_VAR".into(), "from-workspace-toml".into()),
        ];
        backend(&home).run(&s, &CancellationToken::new()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        let seen = std::fs::read_to_string(&out).unwrap();
        assert_eq!(seen.trim(), format!("{}|from-workspace-toml", home.path().join("d.sock").display()));
    }

    #[tokio::test]
    async fn missing_script_fails_closed_without_spawning() {
        let home = TempDir::new().unwrap();
        let backend = backend(&home);
        let e = backend.run(&step("deep-work", "nope"), &CancellationToken::new()).await.unwrap_err();
        assert_eq!(e.code, shimmer_core::ErrorCode::Unavailable);
    }

    #[test]
    fn resolve_script_stays_inside_the_workspace_dir() {
        let home = Path::new("/home/shimmer");
        let launch =
            resolve_script(home, "deep-work", &Step::Launch { index: 2, count: 3, name: "editor".into() }).unwrap();
        assert_eq!(launch, home.join("data/workspaces/deep-work/steps/02-editor.sh"));

        let cleanup = resolve_script(home, "deep-work", &Step::Cleanup).unwrap();
        assert_eq!(cleanup, home.join("data/workspaces/deep-work/cleanup.sh"));

        assert!(resolve_script(home, "../escape", &Step::Cleanup).is_err());
        assert!(resolve_script(home, "deep-work", &Step::Launch { index: 1, count: 1, name: "../x".into() }).is_err());
    }

    fn supervised_step(workspace_dir: &str, name: &str, timeout: Duration) -> LaunchStep {
        let mut s = step(workspace_dir, name);
        s.mode = SpawnMode::Supervised { timeout };
        s
    }

    #[tokio::test]
    async fn supervised_step_waits_for_exit_and_captures_stdout_verbatim() {
        let home = TempDir::new().unwrap();
        write_step_script(home.path(), "deep-work", "setup", "echo hello-from-setup\n");

        let backend = backend(&home);
        let outcome = backend
            .run(&supervised_step("deep-work", "setup", Duration::from_secs(5)), &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(outcome.exit_code, Some(0));
        assert!(!outcome.timed_out);
        let log = std::fs::read_to_string(home.path().join(&outcome.log_path)).unwrap();
        assert_eq!(log, "hello-from-setup\n");
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_process_group_not_just_the_immediate_child() {
        let home = TempDir::new().unwrap();
        let grandchild_marker = home.path().join("grandchild_marker");
        // The backgrounded subshell inherits the script's process group (no setsid of its
        // own), so a process-group kill must take it down too — proving §10's "kills the
        // grandchild too (proves process-group kill, not just the immediate child)".
        write_step_script(
            home.path(),
            "deep-work",
            "setup",
            &format!("(sleep 1 && touch {}) &\nsleep 5\n", grandchild_marker.display()),
        );

        let backend = backend(&home);
        let outcome = backend
            .run(&supervised_step("deep-work", "setup", Duration::from_millis(200)), &CancellationToken::new())
            .await
            .unwrap();

        assert!(outcome.timed_out);
        assert_eq!(outcome.exit_code, None);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!grandchild_marker.exists(), "grandchild survived the process-group kill");
    }

    #[tokio::test]
    async fn cancellation_kills_the_group_and_is_not_reported_as_timed_out() {
        let home = TempDir::new().unwrap();
        let grandchild_marker = home.path().join("grandchild_marker");
        write_step_script(
            home.path(),
            "deep-work",
            "setup",
            &format!("(sleep 1 && touch {}) &\nsleep 5\n", grandchild_marker.display()),
        );

        let backend = backend(&home);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel_clone.cancel();
        });
        let outcome =
            backend.run(&supervised_step("deep-work", "setup", Duration::from_secs(5)), &cancel).await.unwrap();

        assert!(!outcome.timed_out, "a cancelled step is not timed out (callers distinguish via cancel)");
        assert_eq!(outcome.exit_code, None);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!grandchild_marker.exists(), "grandchild survived the process-group kill");
    }
}
