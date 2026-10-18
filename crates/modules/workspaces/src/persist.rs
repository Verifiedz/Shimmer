//! A workspace's saved state, `data/workspaces/<id>/state.toml`. Written only by the daemon;
//! human-readable like every other file (CLAUDE.md §1.4). Pure: text in, text out.
//!
//! ```toml
//! state = "dirty"
//!
//! [dirty]
//! reason = "exit code 1"
//! step = { index = 1, count = 3, name = "setup" }
//! failed_at = "2026-09-16T09:12:44Z"
//! log = "logs/deep-work-01JD2T.log"
//! ```
//!
//! While launching, `step` records the step that is running, so a daemon that dies mid-launch
//! can say which step it was on when it restarts. `[last_session]` describes the most recent
//! launch attempt, for `workspaces.show`:
//!
//! ```toml
//! [last_session]
//! id = "01JD2T…"
//! started_at = "2026-09-16T09:12:30Z"
//! forced = false
//! outcome = "dirty"
//! ```

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use shimmer_core::{Error, Result};

use crate::state::{self, FailedStep, WorkspaceState};

/// The state file's name inside each workspace folder.
pub const STATE_FILE: &str = "state.toml";

/// What `state.toml` holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Saved {
    pub state: WorkspaceState,
    /// While `Launching`: the step that was running when this was written.
    pub running_step: Option<FailedStep>,
    pub last_session: Option<LastSession>,
}

/// The most recent launch attempt.
#[derive(Clone, Debug, PartialEq)]
pub struct LastSession {
    /// `SHIMMER_SESSION_ID` (ADR 0010 §9), as the launcher reported it.
    pub id: String,
    pub started_at: DateTime<Utc>,
    pub forced: bool,
    pub outcome: Outcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Every step succeeded.
    Launched,
    /// A step failed; the workspace is dirty.
    Dirty,
    /// Cancelled part-way; the workspace is dirty.
    Cancelled,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Launched => "launched",
            Self::Dirty => "dirty",
            Self::Cancelled => "cancelled",
        }
    }
}

impl Saved {
    pub fn new(state: WorkspaceState) -> Self {
        Self { state, running_step: None, last_session: None }
    }

    pub fn with_last_session(mut self, last: Option<LastSession>) -> Self {
        self.last_session = last;
        self
    }

    /// No `state.toml` yet: a workspace nobody has launched.
    pub fn ready() -> Self {
        Self::new(WorkspaceState::Ready)
    }
}

/// Read `state.toml`. A file Shimmer can't read is `invalid_params` naming it; `reset` clears it.
pub fn decode(id: &str, text: &str) -> Result<Saved> {
    let bad = |msg: String| Error::invalid_params(format!("{id}/{STATE_FILE}: {msg}"));
    let file: File = toml::from_str(text).map_err(|e| bad(e.to_string()))?;
    let state = match (file.state.as_str(), file.dirty) {
        ("ready", None) => WorkspaceState::Ready,
        ("launching", None) => WorkspaceState::Launching,
        ("active", None) => WorkspaceState::Active,
        ("dirty", Some(d)) => WorkspaceState::Dirty {
            reason: d.reason,
            failed_step: d.step.into(),
            failed_at: DateTime::parse_from_rfc3339(&d.failed_at)
                .map_err(|e| bad(format!("[dirty] failed_at: {e}")))?
                .with_timezone(&Utc),
            log: d.log,
        },
        ("dirty", None) => return Err(bad("state = \"dirty\" needs a [dirty] table".into())),
        (s @ ("ready" | "launching" | "active"), Some(_)) => {
            return Err(bad(format!("[dirty] is only valid when state = \"dirty\", not \"{s}\"")))
        }
        (other, _) => return Err(bad(format!("unknown state \"{other}\""))),
    };
    let running_step = match (&state, file.step) {
        (WorkspaceState::Launching, step) => step.map(Into::into),
        (_, None) => None,
        (_, Some(_)) => return Err(bad("step is only valid when state = \"launching\"".into())),
    };
    let last_session = match file.last_session {
        None => None,
        Some(l) => Some(LastSession {
            id: l.id,
            started_at: DateTime::parse_from_rfc3339(&l.started_at)
                .map_err(|e| bad(format!("[last_session] started_at: {e}")))?
                .with_timezone(&Utc),
            forced: l.forced,
            outcome: match l.outcome.as_str() {
                "launched" => Outcome::Launched,
                "dirty" => Outcome::Dirty,
                "cancelled" => Outcome::Cancelled,
                other => return Err(bad(format!("[last_session] unknown outcome \"{other}\""))),
            },
        }),
    };
    Ok(Saved { state, running_step, last_session })
}

pub fn encode(saved: &Saved) -> String {
    let (state, dirty) = match &saved.state {
        WorkspaceState::Ready => ("ready", None),
        WorkspaceState::Launching => ("launching", None),
        WorkspaceState::Active => ("active", None),
        WorkspaceState::Dirty { reason, failed_step, failed_at, log } => (
            "dirty",
            Some(DirtyTable {
                reason: reason.clone(),
                step: failed_step.into(),
                failed_at: failed_at.to_rfc3339_opts(SecondsFormat::Secs, true),
                log: log.clone(),
            }),
        ),
    };
    let step = match saved.state {
        WorkspaceState::Launching => saved.running_step.as_ref().map(Into::into),
        _ => None,
    };
    let last_session = saved.last_session.as_ref().map(|l| LastSessionTable {
        id: l.id.clone(),
        started_at: l.started_at.to_rfc3339_opts(SecondsFormat::Secs, true),
        forced: l.forced,
        outcome: l.outcome.as_str().into(),
    });
    toml::to_string(&File { state: state.into(), step, dirty, last_session }).expect("state.toml always serialises")
}

/// The daemon found a workspace still saved as `launching` on startup: it stopped in the middle
/// of a launch. Steps that already ran can't be undone, so it is `dirty` (§10.3, §7.1). Any
/// `last_session` is kept as it was.
pub fn recover_after_restart(saved: &Saved, now: DateTime<Utc>) -> Option<WorkspaceState> {
    if saved.state != WorkspaceState::Launching {
        return None;
    }
    let (failed_step, reason) = match &saved.running_step {
        Some(step) => (step.clone(), format!("the daemon stopped during step {step}")),
        None => {
            (FailedStep { index: 0, count: 0, name: "unknown".into() }, "the daemon stopped during a launch".to_owned())
        }
    };
    // Through the state machine, so this can never drift from an ordinary failed step.
    state::step_failed(&WorkspaceState::Launching, reason, failed_step, now, "").ok()
}

// ---------------------------------------------------------------- the file

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    step: Option<StepTable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dirty: Option<DirtyTable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_session: Option<LastSessionTable>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LastSessionTable {
    id: String,
    started_at: String,
    forced: bool,
    outcome: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StepTable {
    index: u32,
    count: u32,
    name: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirtyTable {
    reason: String,
    step: StepTable,
    failed_at: String,
    /// Keep this name and place (`[dirty] log`): the daemon's launcher reads it straight from
    /// the file, so step-log pruning never deletes the log a dirty workspace points at (#100,
    /// `crates/daemon/src/launcher.rs`'s `dirty_log_pointer`). Renaming it would silently let
    /// that log be pruned.
    log: String,
}

impl From<StepTable> for FailedStep {
    fn from(s: StepTable) -> Self {
        Self { index: s.index, count: s.count, name: s.name }
    }
}

impl From<&FailedStep> for StepTable {
    fn from(s: &FailedStep) -> Self {
        Self { index: s.index, count: s.count, name: s.name.clone() }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use shimmer_core::ErrorCode;

    use super::*;

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 16, 9, 12, 44).unwrap()
    }

    fn step() -> FailedStep {
        FailedStep { index: 1, count: 3, name: "setup".into() }
    }

    fn dirty() -> WorkspaceState {
        WorkspaceState::Dirty {
            reason: "exit code 1".into(),
            failed_step: step(),
            failed_at: at(),
            log: "logs/deep-work-01JD2T.log".into(),
        }
    }

    #[test]
    fn every_state_round_trips() {
        let launching = Saved { state: WorkspaceState::Launching, running_step: Some(step()), last_session: None };
        let session = |outcome| LastSession { id: "01JD2T".into(), started_at: at(), forced: true, outcome };
        for saved in [
            Saved::ready(),
            Saved::new(WorkspaceState::Active),
            Saved::new(WorkspaceState::Launching),
            launching,
            Saved::new(dirty()),
            Saved::new(WorkspaceState::Active).with_last_session(Some(session(Outcome::Launched))),
            Saved::new(dirty()).with_last_session(Some(session(Outcome::Dirty))),
            Saved::new(dirty()).with_last_session(Some(session(Outcome::Cancelled))),
        ] {
            assert_eq!(decode("deep-work", &encode(&saved)).unwrap(), saved);
        }
    }

    #[test]
    fn the_file_is_readable() {
        let text = encode(&Saved::new(dirty()));
        assert!(text.starts_with("state = \"dirty\""), "{text}");
        assert!(text.contains("reason = \"exit code 1\""), "{text}");
        assert!(text.contains("failed_at = \"2026-09-16T09:12:44Z\""), "{text}");
    }

    #[test]
    fn running_step_is_only_kept_while_launching() {
        let text = encode(&Saved { state: WorkspaceState::Active, running_step: Some(step()), last_session: None });
        assert_eq!(decode("deep-work", &text).unwrap(), Saved::new(WorkspaceState::Active));
    }

    #[test]
    fn a_bad_file_is_invalid_params_naming_it() {
        for text in [
            "not toml [",
            "state = \"sleeping\"",
            "state = \"dirty\"",
            "state = \"ready\"\n[dirty]\nreason = \"x\"\nfailed_at = \"2026-09-16T09:12:44Z\"\nlog = \"\"\nstep = { index = 1, count = 1, name = \"a\" }",
            "state = \"ready\"\nstep = { index = 1, count = 1, name = \"a\" }",
            "state = \"ready\"\ncolour = \"red\"",
            "state = \"ready\"\n[last_session]\nid = \"x\"\nstarted_at = \"2026-09-16T09:12:44Z\"\nforced = false\noutcome = \"exploded\"",
        ] {
            let e = decode("deep-work", text).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidParams);
            assert!(e.message.starts_with("deep-work/state.toml: "), "{text:?} -> {}", e.message);
        }
    }

    #[test]
    fn a_launch_cut_short_by_a_restart_comes_back_dirty() {
        let saved = Saved { state: WorkspaceState::Launching, running_step: Some(step()), last_session: None };
        let state = recover_after_restart(&saved, at()).unwrap();
        let WorkspaceState::Dirty { reason, failed_step, failed_at, .. } = state else { panic!("{state:?}") };
        assert_eq!(reason, "the daemon stopped during step 1/3 setup");
        assert_eq!(failed_step, step());
        assert_eq!(failed_at, at());

        // Without a recorded step, it is still dirty, just less specific.
        let state = recover_after_restart(&Saved::new(WorkspaceState::Launching), at()).unwrap();
        assert!(
            matches!(state, WorkspaceState::Dirty { ref reason, .. } if reason == "the daemon stopped during a launch")
        );

        for other in [WorkspaceState::Ready, WorkspaceState::Active, dirty()] {
            assert_eq!(recover_after_restart(&Saved::new(other), at()), None);
        }
    }
}
