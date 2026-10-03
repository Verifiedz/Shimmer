//! Spawning a workspace's numbered launch steps (`steps/<index>-<name>.{sh,ps1}`) and its
//! `cleanup.{sh,ps1}` (ADR 0010 §2a) — `core`'s one sanctioned exception to "no raw I/O in a
//! module" (§2). A module never calls `std::process::Command`, `setsid`, or any spawning
//! primitive directly; it only ever sees [`Launcher`], backed by a [`LaunchBackend`] the
//! daemon implements for real and tests replace with a fake (§12 rule 13).
//!
//! Interface only here — no real backend and no daemon code. `Ctx.launcher` defaults to
//! [`Launcher::unavailable`] so every existing `Ctx::new` call site (daemon and tests alike)
//! keeps compiling unchanged; the real backend and the daemon's capability-scoped wiring
//! follow in a separate change once ADR 0010 is signed off.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::{Error, Result};

/// The env var names the launcher injects (CLAUDE.md §10.1). Defined once, here, so renaming
/// them (ADR 0011) touches this module and nothing else in the launcher.
pub mod env_names {
    pub const WORKSPACE_ID: &str = "SHIMMER_WORKSPACE_ID";
    pub const WORKSPACE_DIR: &str = "SHIMMER_WORKSPACE_DIR";
    pub const HOME: &str = "SHIMMER_HOME";
    pub const SOCKET: &str = "SHIMMER_SOCKET";
    pub const SESSION_ID: &str = "SHIMMER_SESSION_ID";
    pub const PLATFORM: &str = "SHIMMER_PLATFORM";
}

/// Which script, not which file — the backend resolves the real path and `.sh` vs `.ps1` by
/// platform itself (ADR 0010 §2, §2a). `cleanup` is always run `Supervised` (CLAUDE.md
/// §10.3); nothing here enforces that, since it is the calling module's choice of `mode`,
/// not the launcher's to decide.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// One numbered step out of a workspace's ordered launch list, run in sequence by the
    /// calling module — one `LaunchStep` per step, each with its own `SpawnMode` (ADR 0010
    /// §2a, added after PR review: a single all-or-nothing `Launch` step could not give a
    /// setup script and a following `Detached` terminal/editor step different modes, and a
    /// failed setup step behind a `Detached` step could never be detected).
    ///
    /// `index` is 1-based. `count` and `name` exist only for reporting — together they are
    /// what `docs/protocol.md`'s `workspace_dirty` detail means by
    /// `failed_step: "3/7 tmux-session"`. The backend resolves `index`/`name` to
    /// `steps/<index>-<name>.{sh,ps1}` under the workspace's own directory, never an
    /// arbitrary path (ADR 0010 §4) — `name` must pass the same charset check as any other
    /// stored id (ADR 0008: `[a-z0-9][a-z0-9_-]*`).
    Launch {
        index: u32,
        count: u32,
        name: String,
    },
    Cleanup,
}

/// How the backend spawns the step (CLAUDE.md §10.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnMode {
    /// Must outlive the daemon. No handle retained.
    Detached,
    /// Handle retained; killed as a whole process group on timeout or on the caller's
    /// `ctx.cancel` (ADR 0010 §9 — both paths converge on the same kill).
    Supervised { timeout: Duration },
}

/// What a module asks the launcher to do. Carries a *logical* step and a namespace-relative
/// directory, never a `PathBuf` — the backend alone resolves real paths, and only ever under
/// `data/workspaces/<workspace_dir>/` (ADR 0010 §4), so nothing upstream of the backend can
/// name a script outside a workspace's own directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchStep {
    /// Becomes `SHIMMER_WORKSPACE_ID`.
    pub workspace_id: String,
    /// Namespace-relative, e.g. `"deep-work"` — validated the same way as any other module
    /// path (`crate::store::validate_path`), never an absolute path.
    pub workspace_dir: String,
    pub step: Step,
    pub mode: SpawnMode,
    /// `workspace.toml`'s `[env]`, nothing else. The backend injects `SHIMMER_HOME`, `SHIMMER_SOCKET`,
    /// `SHIMMER_PLATFORM` and `SHIMMER_SESSION_ID` itself; an entry here can never override one of
    /// those (ADR 0010 §2). Merge logic lives in the real backend, not this interface.
    pub user_env: Vec<(String, String)>,
}

/// What running a step produced. A failed or timed-out step is still `Ok` — it is data the
/// calling module interprets, not an error the launcher raises (ADR 0010 §8). `run` returns
/// `Err` only when the backend could not attempt the step at all (missing script, spawn
/// syscall failure) — see [`LaunchBackend::run`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepOutcome {
    /// Becomes `SHIMMER_SESSION_ID`. Minted by the backend, fresh per launch attempt (ADR 0010
    /// §9), from the daemon's injected `Clock` — never `SystemTime::now()`, and never
    /// supplied by the calling module, which learns it only here (in time to use it in its
    /// own `workspaces.session.launched`/`.dirty` event payloads).
    pub session_id: String,
    /// `None` for `Detached` (never waited on) or if the process was killed by signal rather
    /// than exiting normally.
    pub exit_code: Option<i32>,
    /// `true` only when the `Supervised` timeout fired. A cancelled step is not "timed
    /// out" — callers distinguish it via the cancellation token they passed in.
    pub timed_out: bool,
    /// Relative to `$SHIMMER_HOME`, matching `docs/protocol.md`'s `workspace_dirty` detail shape
    /// (whose JSON key is `log`, not `log_path` — see ADR 0010 §3 for the naming note).
    pub log_path: String,
}

/// Implemented once, for real, in the daemon; faked in tests (`testing::FakeLauncher`).
#[async_trait]
pub trait LaunchBackend: Send + Sync {
    async fn run(&self, step: &LaunchStep, cancel: &tokio_util::sync::CancellationToken) -> Result<StepOutcome>;
}

/// `Ctx`'s launcher capability. Defaults to [`Launcher::unavailable`] until the daemon wires
/// in a real backend, gated on the calling module's manifest declaring the `"process"`
/// capability (ADR 0010 §4) — that gating is daemon-side and not implemented by this change.
#[derive(Clone)]
pub struct Launcher(Arc<dyn LaunchBackend>);

impl Launcher {
    pub fn new(backend: Arc<dyn LaunchBackend>) -> Self {
        Self(backend)
    }

    /// The "no backend registered yet" stub — fails closed, same posture `ctx.http` already
    /// takes before the HTTP gateway exists (`crates/daemon/src/lib.rs`: "`ctx.http` fails
    /// closed").
    pub fn unavailable() -> Self {
        Self(Arc::new(Unavailable))
    }

    pub async fn run(&self, step: &LaunchStep, cancel: &tokio_util::sync::CancellationToken) -> Result<StepOutcome> {
        self.0.run(step, cancel).await
    }
}

impl Default for Launcher {
    fn default() -> Self {
        Self::unavailable()
    }
}

struct Unavailable;

#[async_trait]
impl LaunchBackend for Unavailable {
    async fn run(&self, _step: &LaunchStep, _cancel: &tokio_util::sync::CancellationToken) -> Result<StepOutcome> {
        Err(Error::unavailable("no launch backend registered (ADR 0010 not yet implemented)"))
    }
}

#[cfg(test)]
mod tests {
    use tokio_util::sync::CancellationToken;

    use super::*;

    #[tokio::test]
    async fn unavailable_launcher_fails_closed() {
        let launcher = Launcher::unavailable();
        let step = LaunchStep {
            workspace_id: "deep-work".into(),
            workspace_dir: "deep-work".into(),
            step: Step::Launch { index: 1, count: 1, name: "launch".into() },
            mode: SpawnMode::Detached,
            user_env: vec![],
        };
        let e = launcher.run(&step, &CancellationToken::new()).await.unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::Unavailable);
    }

    #[test]
    fn env_names_all_share_the_one_prefix() {
        for name in [
            env_names::WORKSPACE_ID,
            env_names::WORKSPACE_DIR,
            env_names::HOME,
            env_names::SOCKET,
            env_names::SESSION_ID,
            env_names::PLATFORM,
        ] {
            assert!(name.starts_with("SHIMMER_"), "{name} does not share the script-ABI prefix");
        }
    }
}
