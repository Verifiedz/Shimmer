//! Notify's settings (ADR 0032 §3): `settings.toml` in its own namespace, changed by commands at
//! once (no restart) and still plain TOML to edit by hand. Read each time they're needed.
//!
//! A broken file never silences anything: whatever can't be read falls back to the default, and
//! the problem is reported (by `status`, and by any command that would rewrite the file).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::alerts::Alerts;
use crate::quiet::Quiet;

pub const FILE: &str = "settings.toml";
pub const DEFAULT_CAP: usize = 2000;
pub const MIN_CAP: usize = 100;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pause {
    pub since: DateTime<Utc>,
    /// `None`: until `resume`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<DateTime<Utc>>,
    /// High priority waits too.
    #[serde(default)]
    pub all: bool,
}

impl Pause {
    pub fn active(&self, now: DateTime<Utc>) -> bool {
        self.until.is_none_or(|until| now < until)
    }
}

/// The file as written: every key optional.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<usize>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub quiet: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause: Option<Pause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin_alerts: Option<Alerts>,
}

/// The settings in force.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub cap: usize,
    pub quiet: Quiet,
    pub pause: Option<Pause>,
    pub alerts: Alerts,
}

impl Default for Settings {
    fn default() -> Self {
        Self { cap: DEFAULT_CAP, quiet: Quiet::default(), pause: None, alerts: Alerts::default() }
    }
}

/// `text` (`None`: no file) → the settings in force, and what's wrong with the file, if anything.
/// Each part that can't be read is its default; the rest still counts.
pub fn read(text: Option<&str>) -> (Settings, Option<String>) {
    let Some(text) = text else { return (Settings::default(), None) };
    let file: File = match toml::from_str(text) {
        Ok(f) => f,
        Err(e) => return (Settings::default(), Some(format!("{FILE}: {}", e.message()))),
    };
    let mut problem = None;
    let quiet = Quiet::parse(&file.quiet).unwrap_or_else(|e| {
        problem = Some(format!("{FILE}: {e}"));
        Quiet::default()
    });
    let cap = match file.cap {
        Some(n) if n < MIN_CAP => {
            problem = Some(format!("{FILE}: cap {n} is under {MIN_CAP}; using {MIN_CAP}"));
            MIN_CAP
        }
        Some(n) => n,
        None => DEFAULT_CAP,
    };
    (Settings { cap, quiet, pause: file.pause, alerts: file.builtin_alerts.unwrap_or_default() }, problem)
}

/// The file to rewrite, or why it can't be: a command never overwrites a file it can't read,
/// so a hand edit with a typo isn't lost.
pub fn editable(text: Option<&str>) -> Result<File, String> {
    let Some(text) = text else { return Ok(File::default()) };
    let file: File = toml::from_str(text)
        .map_err(|e| format!("data/notify/{FILE} can't be read ({}); fix it or remove it first", e.message()))?;
    Quiet::parse(&file.quiet).map_err(|e| format!("data/notify/{FILE}: {e}; fix it or remove it first"))?;
    Ok(file)
}

pub fn encode(file: &File) -> String {
    toml::to_string(file).expect("settings always encode")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_file_is_every_default() {
        assert_eq!(read(None), (Settings::default(), None));
        assert_eq!(read(Some("")), (Settings::default(), None));
    }

    #[test]
    fn a_full_file_reads() {
        let text =
            "cap = 500\n[quiet]\nevery_day = \"22:00-08:00\"\n[pause]\nsince = \"2026-10-16T14:00:00Z\"\nall = true\n\
                    [builtin_alerts]\nworkspace_dirty = false\n";
        let (s, problem) = read(Some(text));
        assert_eq!(problem, None);
        assert_eq!(s.cap, 500);
        assert_ne!(s.quiet, Quiet::default());
        assert!(s.pause.as_ref().is_some_and(|p| p.all && p.until.is_none()));
        assert!(!s.alerts.workspace_dirty && s.alerts.scheduled_task_failed);
    }

    #[test]
    fn a_broken_part_falls_back_and_is_reported() {
        let (s, problem) = read(Some("cap = 5\n"));
        assert_eq!(s.cap, MIN_CAP);
        assert!(problem.unwrap().contains("under 100"));
        let (s, problem) = read(Some("[quiet]\nevery_day = \"late\"\n[builtin_alerts]\nworkspace_dirty = false\n"));
        assert!(s.quiet == Quiet::default() && !s.alerts.workspace_dirty, "the readable part still counts");
        assert!(problem.unwrap().contains("quiet.every_day"));
        let (s, problem) = read(Some("cap = [\n"));
        assert_eq!(s, Settings::default());
        assert!(problem.is_some());
        assert!(read(Some("volume = 3\n")).1.unwrap().contains("volume"), "unknown keys are named");
    }

    #[test]
    fn a_file_that_cant_be_read_is_never_overwritten() {
        assert!(editable(Some("cap = [\n")).unwrap_err().contains("fix it or remove it"));
        assert!(editable(Some("[quiet]\nfri = \"x\"\n")).unwrap_err().contains("quiet.fri"));
        let file = editable(Some("cap = 300\n")).unwrap();
        assert_eq!(encode(&file), "cap = 300\n");
    }
}
