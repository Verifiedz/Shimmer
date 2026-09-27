//! The workspace state machine (CLAUDE.md §10.3), exactly as its diagram: plain data and
//! plain transition functions, no `Ctx`, no I/O (§12 rule 10). Each transition takes the
//! current state and returns the next one, or an error if the transition does not apply —
//! never a panic, since a module built on this later must be able to answer a caller's
//! mistake with `internal` rather than crashing.

use serde_json::json;
use swe_core::{Error, ErrorCode, Result};

#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceState {
    Ready,
    Launching,
    Active,
    Dirty { reason: String, failed_step: String, log_path: String },
}

/// `ready -> launching` (§10.3's diagram).
///
/// Judgment call not pinned by CLAUDE.md: activating an already-`active` or still-
/// `launching` workspace is treated as a restart (back to `launching`) rather than an
/// error, since `active` carries no liveness guarantee anyway (§10.3: "active means launch
/// succeeded, nothing more") — there is no state in which re-running the launch script is
/// worse than a no-op. Only `dirty` refuses, because a half-configured workspace must never
/// be launched into (§10.3).
pub fn activate(state: &WorkspaceState) -> Result<WorkspaceState> {
    match state {
        WorkspaceState::Ready | WorkspaceState::Active | WorkspaceState::Launching => Ok(WorkspaceState::Launching),
        WorkspaceState::Dirty { failed_step, log_path, .. } => {
            Err(Error::new(ErrorCode::WorkspaceDirty, "workspace is dirty; activate refused")
                .with_detail(json!({"failed_step": failed_step, "log_path": log_path})))
        }
    }
}

/// `launching -> active`: "all steps ok" in §10.3's diagram.
pub fn launch_succeeded(state: &WorkspaceState) -> Result<WorkspaceState> {
    match state {
        WorkspaceState::Launching => Ok(WorkspaceState::Active),
        _ => Err(Error::internal("launch_succeeded called outside launching")),
    }
}

/// `launching -> dirty`: "step fails" in §10.3's diagram.
pub fn step_failed(
    state: &WorkspaceState,
    reason: impl Into<String>,
    failed_step: impl Into<String>,
    log_path: impl Into<String>,
) -> Result<WorkspaceState> {
    match state {
        WorkspaceState::Launching => Ok(WorkspaceState::Dirty {
            reason: reason.into(),
            failed_step: failed_step.into(),
            log_path: log_path.into(),
        }),
        _ => Err(Error::internal("step_failed called outside launching")),
    }
}

/// `dirty -> ready`: the cleanup script succeeded (§10.3).
pub fn cleanup_succeeded(state: &WorkspaceState) -> Result<WorkspaceState> {
    match state {
        WorkspaceState::Dirty { .. } => Ok(WorkspaceState::Ready),
        _ => Err(Error::internal("cleanup_succeeded called outside dirty")),
    }
}

/// `dirty -> active`, logged as forced: distinct from [`activate`] on purpose (ADR 0010) so
/// the audit trail (`workspaces.session.forced`) has something to hang off later.
pub fn force_relaunch(state: &WorkspaceState) -> Result<WorkspaceState> {
    match state {
        WorkspaceState::Dirty { .. } => Ok(WorkspaceState::Active),
        _ => Err(Error::internal("force_relaunch called outside dirty")),
    }
}

/// `dirty -> ready` without running a cleanup script — the last resort when there is none
/// (§10.3: "clears only via explicit force relaunch or `workspaces.reset`").
pub fn reset(state: &WorkspaceState) -> Result<WorkspaceState> {
    match state {
        WorkspaceState::Dirty { .. } => Ok(WorkspaceState::Ready),
        _ => Err(Error::internal("reset called outside dirty")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirty() -> WorkspaceState {
        WorkspaceState::Dirty {
            reason: "exit code 1".into(),
            failed_step: "launch.sh".into(),
            log_path: "logs/deep-work.log".into(),
        }
    }

    #[test]
    fn activate_from_ready_goes_to_launching() {
        assert_eq!(activate(&WorkspaceState::Ready).unwrap(), WorkspaceState::Launching);
    }

    #[test]
    fn activate_from_active_or_launching_restarts_the_launch() {
        assert_eq!(activate(&WorkspaceState::Active).unwrap(), WorkspaceState::Launching);
        assert_eq!(activate(&WorkspaceState::Launching).unwrap(), WorkspaceState::Launching);
    }

    #[test]
    fn activate_from_dirty_is_refused_with_the_failed_step_and_log_path() {
        let e = activate(&dirty()).unwrap_err();
        assert_eq!(e.code, ErrorCode::WorkspaceDirty);
        assert_eq!(e.detail.unwrap(), json!({"failed_step": "launch.sh", "log_path": "logs/deep-work.log"}));
    }

    #[test]
    fn launch_succeeded_from_launching_goes_active() {
        assert_eq!(launch_succeeded(&WorkspaceState::Launching).unwrap(), WorkspaceState::Active);
    }

    #[test]
    fn launch_succeeded_outside_launching_is_an_error() {
        for s in [WorkspaceState::Ready, WorkspaceState::Active, dirty()] {
            assert!(launch_succeeded(&s).is_err());
        }
    }

    #[test]
    fn step_failed_from_launching_goes_dirty_with_the_given_reason() {
        let d = step_failed(&WorkspaceState::Launching, "exit code 1", "launch.sh", "logs/deep-work.log").unwrap();
        assert_eq!(d, dirty());
    }

    #[test]
    fn step_failed_outside_launching_is_an_error() {
        for s in [WorkspaceState::Ready, WorkspaceState::Active, dirty()] {
            assert!(step_failed(&s, "r", "s", "l").is_err());
        }
    }

    #[test]
    fn cleanup_succeeded_from_dirty_goes_ready() {
        assert_eq!(cleanup_succeeded(&dirty()).unwrap(), WorkspaceState::Ready);
    }

    #[test]
    fn cleanup_succeeded_outside_dirty_is_an_error() {
        for s in [WorkspaceState::Ready, WorkspaceState::Launching, WorkspaceState::Active] {
            assert!(cleanup_succeeded(&s).is_err());
        }
    }

    #[test]
    fn force_relaunch_from_dirty_goes_active_logged_as_forced() {
        assert_eq!(force_relaunch(&dirty()).unwrap(), WorkspaceState::Active);
    }

    #[test]
    fn force_relaunch_outside_dirty_is_an_error() {
        for s in [WorkspaceState::Ready, WorkspaceState::Launching, WorkspaceState::Active] {
            assert!(force_relaunch(&s).is_err());
        }
    }

    #[test]
    fn reset_from_dirty_goes_ready_without_a_cleanup_script() {
        assert_eq!(reset(&dirty()).unwrap(), WorkspaceState::Ready);
    }

    #[test]
    fn reset_outside_dirty_is_an_error() {
        for s in [WorkspaceState::Ready, WorkspaceState::Launching, WorkspaceState::Active] {
            assert!(reset(&s).is_err());
        }
    }
}
