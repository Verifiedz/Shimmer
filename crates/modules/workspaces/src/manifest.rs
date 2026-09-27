//! `workspace.toml` parsing (CLAUDE.md §10.1, ADR 0010). Pure: takes the already-read file
//! text, does no I/O. The caller (blocked on ADR 0010's storage decision) reads the file.

use std::collections::BTreeMap;

use serde::Deserialize;
use swe_core::ids::is_valid_name;
use swe_core::{Error, Result};

/// How the daemon spawns `launch.sh`/`launch.ps1` itself (CLAUDE.md §10.2). `cleanup.sh` is
/// always supervised (§10.3) and so is not declared here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnMode {
    /// Must outlive the daemon; no handle retained.
    Detached,
    /// Handle retained, killed as a group on timeout.
    Supervised { timeout_s: u64 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceManifest {
    pub id: String,
    pub label: String,
    pub mode: SpawnMode,
    /// `[env]`: merged with the §10.1 injected table by the (not yet implemented) launcher;
    /// this manifest wins on collision.
    pub env: BTreeMap<String, String>,
}

/// The file as written. `deny_unknown_fields` turns a typo into a named error instead of a
/// silently ignored key, same pattern as `records::schema::Collection` (ADR 0008).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    workspace: Header,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    id: String,
    label: String,
    mode: String,
    #[serde(default)]
    timeout_s: Option<u64>,
}

/// Parse `workspace.toml`'s text. Every problem is `invalid_params` naming the field —
/// the user hand-wrote this file (§10.1: launch scripts, and the manifest beside them, are
/// a permanent public interface).
pub fn parse_manifest(text: &str) -> Result<WorkspaceManifest> {
    let bad = |msg: String| Error::invalid_params(format!("workspace.toml: {msg}"));
    let file: File = toml::from_str(text).map_err(|e| bad(e.to_string()))?;
    let h = file.workspace;

    if !is_valid_name(&h.id) {
        return Err(bad(format!("id '{}' is invalid: must be [a-z][a-z0-9_-]*", h.id)));
    }

    let mode = match (h.mode.as_str(), h.timeout_s) {
        ("detached", None) => SpawnMode::Detached,
        ("detached", Some(_)) => return Err(bad("timeout_s is only valid when mode = \"supervised\"".to_owned())),
        ("supervised", Some(timeout_s)) => SpawnMode::Supervised { timeout_s },
        ("supervised", None) => return Err(bad("mode = \"supervised\" requires timeout_s".to_owned())),
        (other, _) => return Err(bad(format!("mode must be \"detached\" or \"supervised\", got '{other}'"))),
    };

    Ok(WorkspaceManifest { id: h.id, label: h.label, mode, env: file.env })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(body: &str) -> Result<WorkspaceManifest> {
        parse_manifest(&format!("[workspace]\nid = \"deep-work\"\nlabel = \"Deep Work\"\n{body}"))
    }

    #[test]
    fn parses_a_valid_detached_workspace() {
        let m = manifest("mode = \"detached\"\n").unwrap();
        assert_eq!(m.id, "deep-work");
        assert_eq!(m.label, "Deep Work");
        assert_eq!(m.mode, SpawnMode::Detached);
        assert!(m.env.is_empty());
    }

    #[test]
    fn parses_a_valid_supervised_workspace_with_env() {
        let m = manifest("mode = \"supervised\"\ntimeout_s = 30\n\n[env]\nPROJECT_DIR = \"~/code/thing\"\n").unwrap();
        assert_eq!(m.mode, SpawnMode::Supervised { timeout_s: 30 });
        assert_eq!(m.env.get("PROJECT_DIR"), Some(&"~/code/thing".to_owned()));
    }

    #[test]
    fn supervised_without_timeout_s_is_rejected() {
        let e = manifest("mode = \"supervised\"\n").unwrap_err();
        assert!(e.message.contains("requires timeout_s"), "{}", e.message);
    }

    #[test]
    fn detached_with_timeout_s_is_rejected() {
        let e = manifest("mode = \"detached\"\ntimeout_s = 30\n").unwrap_err();
        assert!(e.message.contains("only valid when mode"), "{}", e.message);
    }

    #[test]
    fn unknown_mode_is_rejected() {
        let e = manifest("mode = \"background\"\n").unwrap_err();
        assert!(e.message.contains("must be \"detached\" or \"supervised\""), "{}", e.message);
    }

    #[test]
    fn bad_id_is_rejected() {
        for id in ["Deep-Work", "deep work", "1deep", ""] {
            let e = parse_manifest(&format!("[workspace]\nid = \"{id}\"\nlabel = \"x\"\nmode = \"detached\"\n"))
                .unwrap_err();
            assert!(e.message.contains("is invalid"), "{id:?}: {}", e.message);
        }
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        let e = parse_manifest(
            "[workspace]\nid = \"deep-work\"\nlabel = \"Deep Work\"\nmode = \"detached\"\ncolor = \"red\"\n",
        )
        .unwrap_err();
        assert!(e.message.contains("unknown field"), "{}", e.message);
    }

    #[test]
    fn unparseable_toml_is_invalid_params_not_a_panic() {
        let e = parse_manifest("not valid toml {{{").unwrap_err();
        assert_eq!(e.code, swe_core::ErrorCode::InvalidParams);
    }
}
