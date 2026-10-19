//! The workspace state machine (CLAUDE.md §10.3): plain data and plain transition functions,
//! no `Ctx`, no I/O (§12 rule 10). Each transition takes the current state and returns the
//! next one, or an error if it does not apply. Never a panic: the module answers the error.
//!
//! Who can cause a wrong transition decides the error code (issue #30):
//! - a user request that doesn't fit the state (e.g. `reset` on a workspace that isn't dirty)
//!   is `invalid_params` with a message saying why; activating one that is already launching
//!   is `conflict`;
//! - a transition only the module itself drives (`launch_succeeded`, `step_failed`) arriving
//!   in the wrong state is a bug in the module: `internal`.

use std::fmt;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::json;
use shimmer_core::{Error, ErrorCode, Result};

#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceState {
    Ready,
    Launching,
    Active,
    /// A launch step failed or timed out (§10.3). Blocks `activate` until cleanup, reset or
    /// force relaunch.
    Dirty {
        /// Why the step failed, e.g. "exit code 1" or "timed out after 60s".
        reason: String,
        failed_step: FailedStep,
        failed_at: DateTime<Utc>,
        /// The step's log, relative to `$SHIMMER_HOME` (`StepOutcome::log_path`, ADR 0010).
        log: String,
    },
}

/// Which launch step failed. Shown as `"<index>/<count> <name>"`, e.g. `"1/3 setup"`, the
/// format of `failed_step` in docs/protocol.md's `workspace_dirty` detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedStep {
    /// 1-based, as in `Step::Launch` (ADR 0010 §2a).
    pub index: u32,
    pub count: u32,
    pub name: String,
}

impl fmt::Display for FailedStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{} {}", self.index, self.count, self.name)
    }
}

/// `ready -> launching`, and `active -> launching`: since `active` makes no promise that the
/// apps are still open (§10.3), activating again simply launches again.
///
/// Refused while a launch is already running (`conflict`), and on a dirty workspace
/// (`workspace_dirty`), because a half-configured workspace must never be launched into
/// (§10.3). `workspace` and `has_cleanup_script` are only used for that error's `detail`,
/// which matches docs/protocol.md so a client can offer cleanup, force relaunch or the log.
pub fn activate(state: &WorkspaceState, workspace: &str, has_cleanup_script: bool) -> Result<WorkspaceState> {
    match state {
        WorkspaceState::Ready | WorkspaceState::Active => Ok(WorkspaceState::Launching),
        WorkspaceState::Launching => Err(Error::conflict(format!("workspace '{workspace}' is already launching"))),
        WorkspaceState::Dirty { .. } => Err(dirty_error(state, workspace, has_cleanup_script)),
    }
}

/// The `workspace_dirty` error, with docs/protocol.md's `detail`. Returned by [`activate`] on a
/// dirty workspace, and by a launch whose step just failed, so both look the same to a client.
/// Called with any other state it is `internal`: only a dirty workspace has this error.
pub fn dirty_error(state: &WorkspaceState, workspace: &str, has_cleanup_script: bool) -> Error {
    let WorkspaceState::Dirty { reason, failed_step, failed_at, log } = state else {
        return Error::internal(format!("dirty_error called in state {state:?}"));
    };
    Error::new(
        ErrorCode::WorkspaceDirty,
        format!("workspace '{workspace}' failed at step {failed_step} ({reason}) and was not cleaned up"),
    )
    .with_detail(json!({
        "workspace": workspace,
        "failed_step": failed_step.to_string(),
        "failed_at": failed_at.to_rfc3339_opts(SecondsFormat::Secs, true),
        "log": log,
        "has_cleanup_script": has_cleanup_script,
        // The command that clears it (ADR 0035 §10): what a message or the CLI tells you to run.
        "fix": format!("shimmer workspaces {} {workspace}", if has_cleanup_script { "cleanup" } else { "reset" }),
    }))
}

/// `launching -> active`: every step succeeded (§10.3).
pub fn launch_succeeded(state: &WorkspaceState) -> Result<WorkspaceState> {
    match state {
        WorkspaceState::Launching => Ok(WorkspaceState::Active),
        other => Err(Error::internal(format!("launch_succeeded called in state {other:?}"))),
    }
}

/// `launching -> dirty`: a step failed or timed out (§10.3).
pub fn step_failed(
    state: &WorkspaceState,
    reason: impl Into<String>,
    failed_step: FailedStep,
    failed_at: DateTime<Utc>,
    log: impl Into<String>,
) -> Result<WorkspaceState> {
    match state {
        WorkspaceState::Launching => {
            Ok(WorkspaceState::Dirty { reason: reason.into(), failed_step, failed_at, log: log.into() })
        }
        other => Err(Error::internal(format!("step_failed called in state {other:?}"))),
    }
}

/// `launching -> <the state before>`: the launch was claimed but no step ran (cancelled before
/// the first step, or the first step couldn't be started). Nothing changed on the machine, so the
/// workspace goes back to exactly where it was, and the module records it
/// (`workspaces.session.abandoned`), so a forced relaunch that never ran still leaves a trail
/// (§10.3).
pub fn launch_abandoned(state: &WorkspaceState, before: &WorkspaceState) -> Result<WorkspaceState> {
    match (state, before) {
        (WorkspaceState::Launching, WorkspaceState::Launching) => {
            Err(Error::internal("launch_abandoned: the state before a launch can't be launching"))
        }
        (WorkspaceState::Launching, before) => Ok(before.clone()),
        (other, _) => Err(Error::internal(format!("launch_abandoned called in state {other:?}"))),
    }
}

/// Check before running `cleanup.sh`: only a dirty workspace has anything to clean up.
pub fn start_cleanup(state: &WorkspaceState) -> Result<()> {
    match state {
        WorkspaceState::Dirty { .. } => Ok(()),
        _ => Err(not_dirty("clean up")),
    }
}

/// `dirty -> ready`: the cleanup script succeeded (§10.3). Refused if the workspace stopped
/// being dirty meanwhile (e.g. a `reset` while cleanup was running).
pub fn cleanup_succeeded(state: &WorkspaceState) -> Result<WorkspaceState> {
    match state {
        WorkspaceState::Dirty { .. } => Ok(WorkspaceState::Ready),
        _ => Err(not_dirty("finish cleaning up")),
    }
}

/// `dirty -> launching`: the explicit, loud override (§10.3). It still runs every step and can
/// fail again, so it goes through `launching` like any launch; the module records it as
/// forced (`workspaces.session.forced`).
pub fn force_relaunch(state: &WorkspaceState) -> Result<WorkspaceState> {
    match state {
        WorkspaceState::Dirty { .. } => Ok(WorkspaceState::Launching),
        _ => Err(not_dirty("force a relaunch; use activate")),
    }
}

/// `dirty -> ready` without running a cleanup script: the last resort when there is none
/// (§10.3).
pub fn reset(state: &WorkspaceState) -> Result<WorkspaceState> {
    match state {
        WorkspaceState::Dirty { .. } => Ok(WorkspaceState::Ready),
        _ => Err(not_dirty("reset")),
    }
}

/// Check before stopping (ADR 0026 §1): only an `active` workspace is running anything to stop.
pub fn start_stop(state: &WorkspaceState) -> Result<()> {
    match state {
        WorkspaceState::Active => Ok(()),
        WorkspaceState::Ready => Err(Error::invalid_params("the workspace isn't running, so there is nothing to stop")),
        WorkspaceState::Launching => {
            Err(Error::conflict("the workspace is still launching; stop it once it is active"))
        }
        WorkspaceState::Dirty { .. } => {
            Err(Error::invalid_params("the workspace is dirty: run workspaces.cleanup (or reset) instead of stop"))
        }
    }
}

/// `active -> ready`: the cleanup script stopped what the launch started (ADR 0026 §1).
/// Refused if the workspace stopped being active meanwhile.
pub fn stop_succeeded(state: &WorkspaceState) -> Result<WorkspaceState> {
    match state {
        WorkspaceState::Active => Ok(WorkspaceState::Ready),
        other => Err(Error::conflict(format!("the workspace changed while stopping (now {other:?})"))),
    }
}

/// Check before changing a workspace's answers (ADR 0025 §9): only when nothing of it runs. A
/// running or dirty workspace's cleanup reads the answers, and would then stop what the new ones
/// name rather than what the old ones started (another folder, another stop command).
pub fn start_reconfigure(state: &WorkspaceState) -> Result<()> {
    match state {
        WorkspaceState::Ready => Ok(()),
        WorkspaceState::Active | WorkspaceState::Launching => Err(Error::conflict(
            "the workspace is running: stop it first (workspaces.stop), so stop uses the answers it started with",
        )),
        WorkspaceState::Dirty { .. } => Err(Error::conflict(
            "the workspace is dirty: clean it up first (workspaces.cleanup), so cleanup uses the answers it started with",
        )),
    }
}

/// Check before renaming (ADR 0026 §2a): only when nothing of it runs. Stop and cleanup find what
/// a launch started by the workspace's id (its temp folder, its process groups' records), so a
/// running or dirty workspace keeps its name until it's stopped or cleaned up.
pub fn start_rename(state: &WorkspaceState) -> Result<()> {
    match state {
        WorkspaceState::Ready => Ok(()),
        WorkspaceState::Active | WorkspaceState::Launching => Err(Error::conflict(
            "the workspace is running: stop it first (workspaces.stop), so stop finds what it started",
        )),
        WorkspaceState::Dirty { .. } => Err(Error::conflict(
            "the workspace is dirty: clean it up first (workspaces.cleanup), so cleanup finds what it started",
        )),
    }
}

/// Check before removing (ADR 0026 §2): never while it may have processes running, so nothing
/// is left behind with no workspace to stop it.
pub fn start_remove(state: &WorkspaceState) -> Result<()> {
    match state {
        WorkspaceState::Ready => Ok(()),
        WorkspaceState::Active | WorkspaceState::Launching => {
            Err(Error::conflict("the workspace is running: stop it first (workspaces.stop)"))
        }
        WorkspaceState::Dirty { .. } => Err(Error::conflict("the workspace is dirty: clean it up or reset it first")),
    }
}

fn not_dirty(action: &str) -> Error {
    Error::invalid_params(format!("the workspace is not dirty, so there is nothing to {action}"))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 16, 9, 12, 44).unwrap()
    }

    fn setup_step() -> FailedStep {
        FailedStep { index: 1, count: 3, name: "setup".into() }
    }

    fn dirty() -> WorkspaceState {
        WorkspaceState::Dirty {
            reason: "exit code 1".into(),
            failed_step: setup_step(),
            failed_at: at(),
            log: "logs/deep-work-01JD2T.log".into(),
        }
    }

    const NOT_DIRTY: [WorkspaceState; 3] = [WorkspaceState::Ready, WorkspaceState::Launching, WorkspaceState::Active];

    #[test]
    fn failed_step_reads_index_count_name() {
        assert_eq!(setup_step().to_string(), "1/3 setup");
    }

    #[test]
    fn activate_from_ready_or_active_launches() {
        for s in [WorkspaceState::Ready, WorkspaceState::Active] {
            assert_eq!(activate(&s, "deep-work", false).unwrap(), WorkspaceState::Launching);
        }
    }

    #[test]
    fn activate_while_launching_is_a_conflict() {
        let e = activate(&WorkspaceState::Launching, "deep-work", false).unwrap_err();
        assert_eq!(e.code, ErrorCode::Conflict);
        assert!(e.message.contains("already launching"), "{}", e.message);
    }

    #[test]
    fn activate_on_dirty_is_refused_with_the_protocol_detail() {
        let e = activate(&dirty(), "deep-work", true).unwrap_err();
        assert_eq!(e.code, ErrorCode::WorkspaceDirty);
        assert!(e.message.contains("step 1/3 setup (exit code 1)"), "{}", e.message);
        // docs/protocol.md, "workspace_dirty detail".
        assert_eq!(
            e.detail.unwrap(),
            json!({"workspace": "deep-work", "failed_step": "1/3 setup", "failed_at": "2026-09-16T09:12:44Z",
                   "log": "logs/deep-work-01JD2T.log", "has_cleanup_script": true,
                   "fix": "shimmer workspaces cleanup deep-work"})
        );
        let without = activate(&dirty(), "deep-work", false).unwrap_err();
        assert_eq!(without.detail.unwrap()["fix"], "shimmer workspaces reset deep-work", "no cleanup script");
    }

    #[test]
    fn launch_succeeded_goes_active_and_is_internal_anywhere_else() {
        assert_eq!(launch_succeeded(&WorkspaceState::Launching).unwrap(), WorkspaceState::Active);
        for s in [WorkspaceState::Ready, WorkspaceState::Active, dirty()] {
            assert_eq!(launch_succeeded(&s).unwrap_err().code, ErrorCode::Internal);
        }
    }

    #[test]
    fn step_failed_goes_dirty_and_is_internal_anywhere_else() {
        let d = step_failed(&WorkspaceState::Launching, "exit code 1", setup_step(), at(), "logs/deep-work-01JD2T.log");
        assert_eq!(d.unwrap(), dirty());
        for s in [WorkspaceState::Ready, WorkspaceState::Active, dirty()] {
            let e = step_failed(&s, "r", setup_step(), at(), "l").unwrap_err();
            assert_eq!(e.code, ErrorCode::Internal);
        }
    }

    #[test]
    fn an_abandoned_launch_goes_back_to_where_it_was() {
        for before in [WorkspaceState::Ready, WorkspaceState::Active, dirty()] {
            assert_eq!(launch_abandoned(&WorkspaceState::Launching, &before).unwrap(), before);
        }
        let e = launch_abandoned(&WorkspaceState::Launching, &WorkspaceState::Launching).unwrap_err();
        assert_eq!(e.code, ErrorCode::Internal);
        for not_launching in [WorkspaceState::Ready, WorkspaceState::Active, dirty()] {
            let e = launch_abandoned(&not_launching, &WorkspaceState::Ready).unwrap_err();
            assert_eq!(e.code, ErrorCode::Internal);
        }
    }

    #[test]
    fn cleanup_only_applies_to_a_dirty_workspace() {
        start_cleanup(&dirty()).unwrap();
        assert_eq!(cleanup_succeeded(&dirty()).unwrap(), WorkspaceState::Ready);
        for s in NOT_DIRTY {
            assert_eq!(start_cleanup(&s).unwrap_err().code, ErrorCode::InvalidParams);
            assert_eq!(cleanup_succeeded(&s).unwrap_err().code, ErrorCode::InvalidParams);
        }
    }

    #[test]
    fn force_relaunch_goes_through_launching_and_can_fail_again() {
        let s = force_relaunch(&dirty()).unwrap();
        assert_eq!(s, WorkspaceState::Launching);
        // A forced launch is an ordinary launch from here: its steps can fail again.
        let again = step_failed(&s, "exit code 1", setup_step(), at(), "logs/deep-work-01JD2T.log").unwrap();
        assert_eq!(again, dirty());
        for s in NOT_DIRTY {
            let e = force_relaunch(&s).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidParams);
            assert!(e.message.contains("use activate"), "{}", e.message);
        }
    }

    #[test]
    fn reset_clears_dirty_without_cleanup_and_is_refused_otherwise() {
        assert_eq!(reset(&dirty()).unwrap(), WorkspaceState::Ready);
        for s in NOT_DIRTY {
            assert_eq!(reset(&s).unwrap_err().code, ErrorCode::InvalidParams);
        }
    }

    #[test]
    fn a_full_round_trip() {
        // ready → launching → dirty → (activate refused) → cleanup → ready → launching → active
        let s = activate(&WorkspaceState::Ready, "deep-work", true).unwrap();
        let s = step_failed(&s, "timed out after 60s", setup_step(), at(), "logs/x.log").unwrap();
        assert!(activate(&s, "deep-work", true).is_err());
        start_cleanup(&s).unwrap();
        let s = cleanup_succeeded(&s).unwrap();
        let s = activate(&s, "deep-work", true).unwrap();
        assert_eq!(launch_succeeded(&s).unwrap(), WorkspaceState::Active);
    }

    #[test]
    fn only_an_active_workspace_can_be_stopped() {
        assert!(start_stop(&WorkspaceState::Active).is_ok());
        assert_eq!(stop_succeeded(&WorkspaceState::Active).unwrap(), WorkspaceState::Ready);
        assert_eq!(start_stop(&WorkspaceState::Ready).unwrap_err().code, ErrorCode::InvalidParams);
        assert_eq!(start_stop(&WorkspaceState::Launching).unwrap_err().code, ErrorCode::Conflict);
        assert!(start_stop(&dirty()).unwrap_err().message.contains("cleanup"));
        assert_eq!(stop_succeeded(&WorkspaceState::Ready).unwrap_err().code, ErrorCode::Conflict);
    }

    #[test]
    fn only_a_workspace_that_isnt_running_can_be_removed() {
        assert!(start_remove(&WorkspaceState::Ready).is_ok());
        for s in [WorkspaceState::Active, WorkspaceState::Launching, dirty()] {
            assert_eq!(start_remove(&s).unwrap_err().code, ErrorCode::Conflict, "{s:?}");
        }
        assert!(start_remove(&WorkspaceState::Active).unwrap_err().message.contains("stop it first"));
    }

    #[test]
    fn answers_change_only_while_nothing_runs() {
        assert!(start_reconfigure(&WorkspaceState::Ready).is_ok());
        let e = start_reconfigure(&WorkspaceState::Active).unwrap_err();
        assert!(e.message.contains("stop it first"), "{}", e.message);
        assert!(start_reconfigure(&WorkspaceState::Launching).is_err());
        let dirty = WorkspaceState::Dirty {
            reason: "x".into(),
            failed_step: FailedStep { index: 1, count: 1, name: "s".into() },
            failed_at: "2026-10-07T00:00:00Z".parse().unwrap(),
            log: String::new(),
        };
        assert!(start_reconfigure(&dirty).unwrap_err().message.contains("clean it up first"));
    }

    #[test]
    fn a_name_changes_only_while_nothing_runs() {
        assert!(start_rename(&WorkspaceState::Ready).is_ok());
        assert!(start_rename(&WorkspaceState::Active).unwrap_err().message.contains("stop it first"));
        assert!(start_rename(&WorkspaceState::Launching).is_err());
    }
}
