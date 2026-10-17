//! `ctx.desktop` (ADR 0032 §1): show one desktop notification, and nothing else. No program, no
//! path, no handle reaches a module. The daemon implements the backend for real
//! (`notify-send`, `osascript`); tests replace it with `testing::FakeDesktop` (§12 rule 13).
//!
//! Same posture as `ctx.launcher` and `ctx.http`: every module gets [`Desktop::unavailable`],
//! and the daemon swaps in the real backend only for a module whose manifest declares the
//! `"desktop"` capability.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// How insistent a note is; the backend maps it to what the desktop offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

/// One notification: plain text, never markup or a script.
#[derive(Clone, Debug, PartialEq)]
pub struct DesktopNote {
    pub title: String,
    pub body: String,
    pub urgency: Urgency,
}

/// Implemented once, for real, in the daemon; faked in tests (`testing::FakeDesktop`).
#[async_trait]
pub trait DesktopBackend: Send + Sync {
    /// `Ok` means handed to the operating system, not seen: the OS may still hide it (Focus,
    /// notifications turned off). `unavailable` when there's no desktop to show it on.
    async fn show(&self, note: &DesktopNote) -> Result<()>;
}

/// `Ctx`'s desktop capability.
#[derive(Clone)]
pub struct Desktop(Arc<dyn DesktopBackend>);

impl Desktop {
    pub fn new(backend: Arc<dyn DesktopBackend>) -> Self {
        Self(backend)
    }

    /// For every module that doesn't declare `"desktop"`: every call fails `unavailable`.
    pub fn unavailable() -> Self {
        Self(Arc::new(Unavailable))
    }

    pub async fn show(&self, note: &DesktopNote) -> Result<()> {
        self.0.show(note).await
    }
}

impl Default for Desktop {
    fn default() -> Self {
        Self::unavailable()
    }
}

struct Unavailable;

#[async_trait]
impl DesktopBackend for Unavailable {
    async fn show(&self, _note: &DesktopNote) -> Result<()> {
        Err(Error::unavailable("no desktop: this module doesn't declare the \"desktop\" capability (ADR 0032 §1)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn without_the_capability_it_fails_closed() {
        let note = DesktopNote { title: "t".into(), body: "b".into(), urgency: Urgency::Normal };
        let e = Desktop::unavailable().show(&note).await.unwrap_err();
        assert_eq!(e.code, crate::ErrorCode::Unavailable);
        assert!(e.message.contains("\"desktop\" capability"), "{e}");
    }
}
