//! `swe-workspaces`: pure logic only for now (ADR 0010) — the `ready`/`launching`/`active`/
//! `dirty` state machine (CLAUDE.md §10.3) and `workspace.toml` parsing (§10.1).
//!
//! Deliberately not implemented here: the `Module` trait, any `Ctx` use, any file I/O, any
//! process spawning. Those wait on ADR 0010's `Ctx` launcher capability and its proposed
//! move of `workspaces/` under `data/workspaces/`. Everything in this crate is a plain
//! function over plain data a test can call with no runtime (§12 rule 10).
//!
//! Depends on `core` only.

pub mod manifest;
pub mod state;

pub use manifest::{parse_manifest, SpawnMode, WorkspaceManifest};
pub use state::{activate, cleanup_succeeded, force_relaunch, launch_succeeded, reset, step_failed, WorkspaceState};
