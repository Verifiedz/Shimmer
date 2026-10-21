//! The real `LaunchBackend` (ADR 0010). Lands incrementally, one `SpawnMode`/concern per
//! sub-branch of the `m3-launch-backend` umbrella — spawn mechanics, env injection,
//! session-id minting, and step-name validation (closes #14) in the previous slices; this
//! one is wired into `Ctx` for any module whose manifest declares the `"process"`
//! capability (`crates/daemon/src/core.rs`'s `Core::new`, ADR 0010 §4).
//!
//! Linux/macOS (`cfg(unix)`) only, same scoping as ADR 0010 §7 — Windows is sketched there,
//! not implemented.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use shimmer_core::ids::is_valid_id;
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
        // Reuse the id the module threaded back from this attempt's first step; mint only when
        // this is that first step (ADR 0010 §9 amendment — "One id per attempt").
        let session_id = match &step.session_id {
            Some(id) => id.clone(),
            None => mint_session_id(&self.clock, self.instance_id, &self.sequence),
        };
        match step.mode {
            SpawnMode::Detached => run_detached(&self.home, &self.socket, &script, step, &session_id).await,
            SpawnMode::Supervised { timeout } => {
                run_supervised(&self.home, &self.socket, &script, step, &session_id, timeout, cancel).await
            }
        }
    }
}

/// Minted here, not by the calling module (ADR 0010 §9) — only for the first step of a launch
/// attempt (`run`'s caller leaves `LaunchStep.session_id` `None`); every later step of that same
/// attempt reuses the id the module threaded back, so `run` never calls this a second time
/// within one attempt. From the injected `Clock`, never `SystemTime::now()`.
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
/// §2a/§4). `workspace_dir` gets escape-safety only (`validate_path`, the same check every
/// other module's paths get); `name` additionally gets the stricter
/// `[a-z0-9][a-z0-9_-]*` charset (`shimmer_core::ids::is_valid_id`, ADR 0008 — closes #14:
/// `validate_path` alone, the original check here, only rejects path escapes and accepted
/// anything else, so `Setup.v2` or `my step` passed it despite ADR 0010 §4's claim that
/// `name` gets this exact charset) before either is ever interpolated into a path.
fn resolve_script(home: &Path, workspace_dir: &str, step: &Step) -> Result<PathBuf> {
    validate_path(workspace_dir)?;
    let rel = match step {
        Step::Launch { index, name, .. } => {
            if !is_valid_id(name) {
                return Err(Error::invalid_params(format!("launch step name '{name}' must match [a-z0-9][a-z0-9_-]*")));
            }
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
    let log_rel = log_path_for(step, session_id);
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

    prune_step_logs(home, &step.workspace_dir);
    Ok(StepOutcome { session_id: session_id.to_string(), exit_code, timed_out, log_path: log_rel })
}

/// Step logs to keep per workspace (#74): `logs/` gets a new file every supervised run
/// (setup steps, cleanup) and nothing ever removed one before this, so it grew without bound.
const KEEP_STEP_LOGS: usize = 10;

/// Deletes every step log for `workspace_dir` beyond the newest [`KEEP_STEP_LOGS`]
/// *launches*, except the one a `dirty` state currently points at (read straight from
/// `state.toml`, below) — run right after each supervised step's own log is written, so
/// `logs/` stays bounded continuously rather than needing a sweep at daemon start.
/// Best-effort: a failure to list or remove a file is swallowed, since missing a prune is
/// harmless and must never fail the launch step that just succeeded or failed on its own
/// merits.
///
/// Counts distinct *sessions*, not files (#105): a workspace with several supervised steps
/// (a setup step plus a cleanup script, say) writes one log per step but one session id per
/// launch, so counting files alone kept only `KEEP_STEP_LOGS` divided by however many
/// supervised steps a launch happens to have, not `KEEP_STEP_LOGS` launches. Entries are
/// already grouped by session, since `workspace_log_files` sorts by filename and every file
/// from one session shares the identical `<workspace_dir>-<session_id>-` prefix.
fn prune_step_logs(home: &Path, workspace_dir: &str) {
    let mut files = workspace_log_files(home, workspace_dir);
    // Newest first (filenames sort lexically by session id, and session ids are ULIDs, so
    // this is also chronological order — ADR 0010 §3 "Logs").
    files.reverse();
    let protected = dirty_log_pointer(home, workspace_dir).map(|rel| home.join(rel));
    let mut sessions_seen = 0;
    let mut last_session: Option<String> = None;
    for (session, path) in files {
        if last_session.as_ref() != Some(&session) {
            sessions_seen += 1;
            last_session = Some(session);
        }
        if sessions_seen <= KEEP_STEP_LOGS {
            continue;
        }
        if protected.as_deref() == Some(path.as_path()) {
            continue;
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// Every `logs/<workspace_dir>-<session_id>-<label>.log` file for this workspace, with its
/// session id, oldest first. Matches on an exact `<workspace_dir>-` prefix followed by a
/// 26-character ULID, not a plain substring prefix, so a workspace named `foo` never matches
/// another one's logs named `foo-bar-<session>-<label>.log`. A ULID is Crockford base32 —
/// uppercase letters and digits only (#105: `is_ascii_alphanumeric` also accepted lowercase,
/// so e.g. `site-abcdefghijklmnopqrstuvwxyz-setup.log`, a 26-lowercase-letter *label* on a
/// workspace named `site`, was wrongly counted and pruned as one of `site`'s own sessions).
fn workspace_log_files(home: &Path, workspace_dir: &str) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(home.join("logs")) else { return Vec::new() };
    let prefix = format!("{workspace_dir}-");
    let mut files: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let rest = name.strip_prefix(&prefix)?.strip_suffix(".log")?;
            let (session, _label) = rest.split_once('-')?;
            let is_ulid = session.len() == 26 && session.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
            is_ulid.then(|| (session.to_owned(), e.path()))
        })
        .collect();
    files.sort();
    files
}

/// The log path a `dirty` state currently protects, if the workspace is dirty at all. Read
/// directly from `data/workspaces/<workspace_dir>/state.toml` rather than through the
/// workspaces module (the launcher has no `Ctx` into another module's store, §4) — this
/// reads only the one field it needs from an ordinary human-readable file (§1.4), so it
/// stays correct even as the module's own state shape grows around it.
fn dirty_log_pointer(home: &Path, workspace_dir: &str) -> Option<String> {
    let path = home.join("data/workspaces").join(workspace_dir).join("state.toml");
    let table: toml::Table = std::fs::read_to_string(path).ok()?.parse().ok()?;
    table.get("dirty")?.get("log")?.as_str().map(str::to_owned)
}

/// Relative to `$SHIMMER_HOME`, matching `docs/protocol.md`'s `workspace_dirty` detail shape
/// (ADR 0010 §3 "Logs"). The session id is part of the name (#69) so that a relaunch or a
/// force relaunch never overwrites the log a previous `dirty` state still points at — each
/// launch attempt mints its own `session_id` (ADR 0010 §9), so each gets its own log file.
fn log_path_for(step: &LaunchStep, session_id: &str) -> String {
    let label = match &step.step {
        Step::Launch { name, .. } => name.as_str(),
        Step::Cleanup => "cleanup",
    };
    format!("logs/{}-{}-{}.log", step.workspace_dir, session_id, label)
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
/// `[env]` for every key that is not reserved — the override guard (ADR 0010 §2): "a
/// `workspace.toml` that (accidentally or otherwise) declares `SHIMMER_SOCKET = "..."`
/// cannot redirect a script's daemon connection."
fn inject_env(cmd: &mut Command, home: &Path, socket: &Path, step: &LaunchStep, session_id: &str) {
    let workspace_dir = home.join("data/workspaces").join(&step.workspace_dir);
    let injected = [
        (env_names::WORKSPACE_ID, step.workspace_id.clone()),
        (env_names::WORKSPACE_DIR, workspace_dir.display().to_string()),
        (env_names::HOME, home.display().to_string()),
        (env_names::SOCKET, socket.display().to_string()),
        (env_names::SESSION_ID, session_id.to_string()),
        (env_names::PLATFORM, platform().to_string()),
    ];
    for (k, v) in &injected {
        cmd.env(k, v);
    }
    for (k, v) in &step.user_env {
        if !is_shimmer_reserved(k) {
            cmd.env(k, v);
        }
    }
}

const SHIMMER_ENV_PREFIX: &str = "SHIMMER_";

/// ADR 0012 §6 reserves the *whole* `SHIMMER_` prefix, case-insensitively (Windows env names
/// are case-insensitive), not just the six names currently injected — so a `user_env` entry
/// can never collide with a `SHIMMER_*` variable this backend (or a future one) adds later,
/// without this guard needing to grow a matching entry of its own (#69).
fn is_shimmer_reserved(name: &str) -> bool {
    name.to_ascii_uppercase().starts_with(SHIMMER_ENV_PREFIX)
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
            session_id: None,
        }
    }

    /// Polls for a detached script's output file instead of a fixed sleep (#69) — a
    /// `Detached` step is never waited on, so a fixed sleep is a flakiness risk on a slow CI
    /// runner. Polls every 20ms, up to 5s, then panics with what was (or wasn't) there.
    async fn wait_for_file(path: &Path) -> String {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(contents) = std::fs::read_to_string(path) {
                if !contents.is_empty() {
                    return contents;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting for {} to be written", path.display());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Polls until `path` exists (empty is fine), up to 5s.
    async fn wait_for_file_to_exist(path: &Path) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(tokio::time::Instant::now() < deadline, "{} never appeared", path.display());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A script line that starts a grandchild in the script's process group and writes its pid
    /// to `pid_file` (atomically, so a reader never sees half of it). The grandchild sleeps
    /// far longer than any test waits, so it's only gone if something killed it.
    fn grandchild_writing_pid(pid_file: &Path) -> String {
        let p = pid_file.display();
        format!("sh -c 'echo $$ > {p}.tmp && mv {p}.tmp {p}; exec sleep 30' &\nsleep 30\n")
    }

    /// Polls until `pid` no longer exists, up to 5s, instead of a fixed sleep: the kill tests
    /// were flaky on macOS CI because they guessed how long a kill takes.
    async fn gone_within_5s(pid: i32) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test]
    async fn detached_script_outlives_the_backend_that_spawned_it() {
        let home = TempDir::new().unwrap();
        let marker = home.path().join("marker");
        let release = home.path().join("release");
        // The script waits for the test to say go, rather than a guessed 0.3 s that a slow
        // runner could spend just spawning it; it gives up after 10 s so it never lingers.
        write_step_script(
            home.path(),
            "deep-work",
            "setup",
            &format!(
                "i=0; while [ ! -e {r} ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done; touch {m}\n",
                r = release.display(),
                m = marker.display()
            ),
        );

        {
            let backend = backend(&home);
            let outcome = backend.run(&step("deep-work", "setup"), &CancellationToken::new()).await.unwrap();
            assert_eq!(outcome.exit_code, None);
            assert!(!outcome.timed_out);
            // `backend` (and the `Launcher`/`LaunchBackend` value in general) drops here —
            // ADR 0010 §10: the process must still be running afterwards.
        }

        assert!(!marker.exists(), "script should not have finished yet");
        std::fs::write(&release, "").unwrap();
        wait_for_file_to_exist(&marker).await;
    }

    const ECHO_ALL_INJECTED: &str = "echo \"$SHIMMER_WORKSPACE_ID|$SHIMMER_WORKSPACE_DIR|$SHIMMER_HOME|$SHIMMER_SOCKET|$SHIMMER_SESSION_ID|$SHIMMER_PLATFORM\"";

    #[tokio::test]
    async fn injects_all_six_env_names() {
        let home = TempDir::new().unwrap();
        let out = home.path().join("out");
        write_step_script(home.path(), "deep-work", "setup", &format!("{ECHO_ALL_INJECTED} > {}\n", out.display()));

        let b = backend(&home);
        let outcome = b.run(&step("deep-work", "setup"), &CancellationToken::new()).await.unwrap();

        assert!(!outcome.session_id.is_empty());
        let seen = wait_for_file(&out).await;
        let expected = format!(
            "deep-work|{}|{}|{}|{}|{}",
            home.path().join("data/workspaces/deep-work").display(),
            home.path().display(),
            home.path().join("d.sock").display(),
            outcome.session_id,
            platform()
        );
        assert_eq!(seen.trim(), expected);
    }

    #[tokio::test]
    async fn workspace_dir_is_the_absolute_directory_a_script_can_cd_into() {
        // CLAUDE.md §10.1: "SHIMMER_WORKSPACE_DIR — that workspace's own directory." ADR 0012
        // drops a `cwd` setting because "the script can cd" — which only works if the var is an
        // absolute path to the directory, not the bare workspace id (the launcher's own home
        // directory differs from the script's cwd, so a relative id would fail to resolve).
        let home = TempDir::new().unwrap();
        let out = home.path().join("out");
        write_step_script(
            home.path(),
            "deep-work",
            "setup",
            &format!("cd \"$SHIMMER_WORKSPACE_DIR\" && pwd > {}\n", out.display()),
        );

        backend(&home).run(&step("deep-work", "setup"), &CancellationToken::new()).await.unwrap();

        let seen = wait_for_file(&out).await;
        assert_eq!(seen.trim(), home.path().join("data/workspaces/deep-work").display().to_string());
    }

    #[tokio::test]
    async fn distinct_session_ids_across_separate_launch_attempts() {
        // Each call here leaves `session_id: None`, exactly as the module does for the first
        // step of a new attempt — so this is two separate attempts, not two steps of one.
        let home = TempDir::new().unwrap();
        write_step_script(home.path(), "deep-work", "setup", "true\n");
        let b = backend(&home);

        let first = b.run(&step("deep-work", "setup"), &CancellationToken::new()).await.unwrap();
        let second = b.run(&step("deep-work", "setup"), &CancellationToken::new()).await.unwrap();

        assert_ne!(first.session_id, second.session_id);
    }

    #[tokio::test]
    async fn a_later_step_of_the_same_attempt_reuses_the_first_steps_id() {
        // ADR 0010 §9 amendment ("one id per attempt"): the module threads the first step's
        // returned id into every later `LaunchStep` of that attempt via `session_id`, and the
        // backend must echo it back rather than minting again.
        let home = TempDir::new().unwrap();
        write_step_script(home.path(), "deep-work", "setup", "true\n");
        write_step_script(home.path(), "deep-work", "editor", "true\n");
        let b = backend(&home);

        let first = b.run(&step("deep-work", "setup"), &CancellationToken::new()).await.unwrap();
        let mut continued = step("deep-work", "editor");
        continued.session_id = Some(first.session_id.clone());
        let second = b.run(&continued, &CancellationToken::new()).await.unwrap();

        assert_eq!(first.session_id, second.session_id);

        // A later, separate attempt (session_id: None again) still gets a fresh id.
        let third = b.run(&step("deep-work", "setup"), &CancellationToken::new()).await.unwrap();
        assert_ne!(third.session_id, first.session_id);
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

        let seen = wait_for_file(&out).await;
        assert_eq!(seen.trim(), format!("{}|from-workspace-toml", home.path().join("d.sock").display()));
    }

    #[tokio::test]
    async fn user_env_cannot_override_any_shimmer_prefixed_name() {
        // ADR 0012 §6 reserves the whole SHIMMER_ prefix, case-insensitively, not just the
        // six names currently injected (#69) — a workspace.toml declaring a future SHIMMER_*
        // var, or one in a different case, must not reach the script either.
        let home = TempDir::new().unwrap();
        let out = home.path().join("out");
        write_step_script(
            home.path(),
            "deep-work",
            "setup",
            &format!(
                "echo \"${{SHIMMER_SOMETHING_NEW:-unset}}|${{shimmer_socket:-unset}}|$MY_VAR\" > {}\n",
                out.display()
            ),
        );

        let mut s = step("deep-work", "setup");
        s.user_env = vec![
            ("SHIMMER_SOMETHING_NEW".into(), "from-workspace-toml".into()),
            ("shimmer_socket".into(), "attacker-controlled".into()),
            ("MY_VAR".into(), "from-workspace-toml".into()),
        ];
        backend(&home).run(&s, &CancellationToken::new()).await.unwrap();

        let seen = wait_for_file(&out).await;
        assert_eq!(
            seen.trim(),
            "unset|unset|from-workspace-toml",
            "both SHIMMER_SOMETHING_NEW and shimmer_socket must be dropped"
        );
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

    #[test]
    fn invalid_step_names_are_rejected_before_any_path_is_built() {
        let home = Path::new("/home/shimmer");
        // #14's exact examples: a disallowed char, a path escape, and a path separator —
        // none of these should ever reach a path join, let alone the filesystem.
        for bad in ["Setup.v2", "../x", "a/b"] {
            let e =
                resolve_script(home, "deep-work", &Step::Launch { index: 1, count: 1, name: bad.into() }).unwrap_err();
            assert_eq!(e.code, shimmer_core::ErrorCode::InvalidParams, "name = {bad:?}");
        }
    }

    #[test]
    fn step_names_may_start_with_a_digit() {
        // Unlike shimmer_core::ids::is_valid_name (leading letter only) — a step name uses
        // the record-id charset instead (ADR 0008), which also allows a leading digit.
        let home = Path::new("/home/shimmer");
        let launch =
            resolve_script(home, "deep-work", &Step::Launch { index: 1, count: 1, name: "1-setup".into() }).unwrap();
        assert_eq!(launch, home.join("data/workspaces/deep-work/steps/01-1-setup.sh"));
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
    async fn two_launches_of_the_same_workspace_leave_two_separate_logs() {
        // #69: log_path_for used to be workspace+step only, so a relaunch (or a force
        // relaunch of a dirty workspace) overwrote the very log a dirty state's `log_path`
        // still pointed at. Each attempt now mints its own session_id (ADR 0010 §9), so
        // folding it into the log name keeps every attempt's log distinct.
        let home = TempDir::new().unwrap();
        write_step_script(home.path(), "deep-work", "setup", "echo run\n");
        let backend = backend(&home);

        let first = backend
            .run(&supervised_step("deep-work", "setup", Duration::from_secs(5)), &CancellationToken::new())
            .await
            .unwrap();
        let second = backend
            .run(&supervised_step("deep-work", "setup", Duration::from_secs(5)), &CancellationToken::new())
            .await
            .unwrap();

        assert_ne!(first.log_path, second.log_path, "two attempts must not share a log file");
        assert_eq!(std::fs::read_to_string(home.path().join(&first.log_path)).unwrap(), "run\n");
        assert_eq!(std::fs::read_to_string(home.path().join(&second.log_path)).unwrap(), "run\n");
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_process_group_not_just_the_immediate_child() {
        let home = TempDir::new().unwrap();
        let pid_file = home.path().join("grandchild.pid");
        // The backgrounded `sh -c` inherits the script's process group (no setsid of its own),
        // so a process-group kill must take it down too — proving §10's "kills the grandchild
        // too (proves process-group kill, not just the immediate child)". The timeout is long
        // enough that the grandchild is running, not being started, when the kill lands.
        write_step_script(home.path(), "deep-work", "setup", &grandchild_writing_pid(&pid_file));

        let backend = backend(&home);
        let outcome = backend
            .run(&supervised_step("deep-work", "setup", Duration::from_secs(2)), &CancellationToken::new())
            .await
            .unwrap();

        assert!(outcome.timed_out);
        assert_eq!(outcome.exit_code, None);
        let pid: i32 = wait_for_file(&pid_file).await.trim().parse().unwrap();
        assert!(gone_within_5s(pid).await, "grandchild {pid} survived the process-group kill");
    }

    #[tokio::test]
    async fn cancellation_kills_the_group_and_is_not_reported_as_timed_out() {
        let home = TempDir::new().unwrap();
        let pid_file = home.path().join("grandchild.pid");
        write_step_script(home.path(), "deep-work", "setup", &grandchild_writing_pid(&pid_file));

        let backend = backend(&home);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let pid_path = pid_file.clone();
        // Cancelled once the grandchild is running, not at a guessed moment: a kill that lands
        // while the script is still starting it is the race this test kept losing on macOS CI.
        let waiter = tokio::spawn(async move {
            let pid: i32 = wait_for_file(&pid_path).await.trim().parse().unwrap();
            cancel_clone.cancel();
            pid
        });
        let outcome =
            backend.run(&supervised_step("deep-work", "setup", Duration::from_secs(10)), &cancel).await.unwrap();
        let pid = waiter.await.unwrap();

        assert!(!outcome.timed_out, "a cancelled step is not timed out (callers distinguish via cancel)");
        assert_eq!(outcome.exit_code, None);
        assert!(gone_within_5s(pid).await, "grandchild {pid} survived the process-group kill");
    }

    async fn launch_once(backend: &RealLaunchBackend) -> StepOutcome {
        backend
            .run(&supervised_step("deep-work", "setup", Duration::from_secs(5)), &CancellationToken::new())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn n_plus_three_launches_leave_only_the_newest_n_logs() {
        let home = TempDir::new().unwrap();
        write_step_script(home.path(), "deep-work", "setup", "true\n");
        let backend = backend(&home);

        let mut outcomes = Vec::new();
        for _ in 0..KEEP_STEP_LOGS + 3 {
            outcomes.push(launch_once(&backend).await);
        }

        let remaining = workspace_log_files(home.path(), "deep-work");
        assert_eq!(remaining.len(), KEEP_STEP_LOGS, "exactly the newest {KEEP_STEP_LOGS} logs survive");
        for stale in &outcomes[..3] {
            assert!(!home.path().join(&stale.log_path).exists(), "an old, unprotected log must be pruned");
        }
        for fresh in &outcomes[3..] {
            assert!(home.path().join(&fresh.log_path).exists(), "one of the newest {KEEP_STEP_LOGS} must survive");
        }
    }

    /// Two supervised steps of ONE launch attempt sharing a session id, the way the
    /// workspaces module threads `session_id` from the first step's outcome onto the rest of
    /// that same attempt (ADR 0010 §9) -- e.g. a setup step plus a second supervised step.
    async fn launch_once_with_two_steps(backend: &RealLaunchBackend) -> (StepOutcome, StepOutcome) {
        let first = backend
            .run(&supervised_step("deep-work", "setup", Duration::from_secs(5)), &CancellationToken::new())
            .await
            .unwrap();
        let mut second_step = supervised_step("deep-work", "second", Duration::from_secs(5));
        second_step.session_id = Some(first.session_id.clone());
        let second = backend.run(&second_step, &CancellationToken::new()).await.unwrap();
        (first, second)
    }

    #[tokio::test]
    async fn the_cap_counts_launches_not_files_when_one_launch_has_several_logs() {
        // #105: before this fix, a workspace whose launches write more than one supervised
        // step's log (a setup step plus a second one here) only kept KEEP_STEP_LOGS files --
        // half as many *launches* as a single-step workspace, since pruning counted files.
        let home = TempDir::new().unwrap();
        write_step_script(home.path(), "deep-work", "setup", "true\n");
        write_step_script(home.path(), "deep-work", "second", "true\n");
        let backend = backend(&home);

        let mut launches = Vec::new();
        for _ in 0..KEEP_STEP_LOGS + 1 {
            launches.push(launch_once_with_two_steps(&backend).await);
        }

        let remaining = workspace_log_files(home.path(), "deep-work");
        assert_eq!(
            remaining.len(),
            KEEP_STEP_LOGS * 2,
            "kept both logs of each of the newest {KEEP_STEP_LOGS} launches"
        );
        let (oldest_first, oldest_second) = &launches[0];
        assert!(!home.path().join(&oldest_first.log_path).exists(), "the oldest launch's first log must be pruned");
        assert!(
            !home.path().join(&oldest_second.log_path).exists(),
            "the oldest launch's second log must be pruned too"
        );
        let (newest_first, newest_second) = &launches[KEEP_STEP_LOGS];
        assert!(home.path().join(&newest_first.log_path).exists());
        assert!(home.path().join(&newest_second.log_path).exists());
    }

    #[test]
    fn a_26_lowercase_label_is_never_mistaken_for_a_ulid_session_id() {
        // #105: `is_ascii_alphanumeric` also accepts lowercase, so a workspace named `site`
        // used to count (and could prune) a file like this one as if it were one of its own
        // sessions, even though it has nothing to do with `site` at all -- a ULID is
        // Crockford base32, uppercase letters and digits only.
        let home = TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join("logs")).unwrap();
        std::fs::write(home.path().join("logs/site-abcdefghijklmnopqrstuvwxyz-setup.log"), "x").unwrap();
        assert!(workspace_log_files(home.path(), "site").is_empty());
    }

    #[tokio::test]
    async fn pruning_never_deletes_the_log_a_dirty_state_still_points_at() {
        let home = TempDir::new().unwrap();
        write_step_script(home.path(), "deep-work", "setup", "true\n");
        let backend = backend(&home);

        // The very first attempt's log is the one a `dirty` state protects -- written to
        // state.toml exactly as the workspaces module would after that attempt failed.
        let first = launch_once(&backend).await;
        let state_dir = home.path().join("data/workspaces/deep-work");
        std::fs::write(
            state_dir.join("state.toml"),
            format!("state = \"dirty\"\n\n[dirty]\nreason = \"exit code 1\"\nlog = \"{}\"\n", first.log_path),
        )
        .unwrap();

        // KEEP_STEP_LOGS more attempts: without protection, this alone would already be
        // enough to prune the first attempt's log (it is now the oldest of 11).
        for _ in 0..KEEP_STEP_LOGS {
            launch_once(&backend).await;
        }
        assert!(home.path().join(&first.log_path).exists(), "the dirty state's own log must survive pruning");

        let remaining = workspace_log_files(home.path(), "deep-work");
        assert_eq!(remaining.len(), KEEP_STEP_LOGS + 1, "the cap, plus the one protected log that aged out of it");
    }
}
