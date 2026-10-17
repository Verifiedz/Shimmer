//! A way of reaching the user (CLAUDE.md §8, ADR 0032 §3): one small `impl` each. The host
//! module owns the inbox, quiet hours and fallbacks; a sink only delivers one message.
//!
//! The first sink is [`DesktopSink`], through `ctx.desktop` (ADR 0032 §1). Later ones (ntfy, email, a webhook) reach the network only through `ctx.http` and retry with
//! `ctx.retry_with_backoff`: the queue never retries (§11.2).

use async_trait::async_trait;
use shimmer_core::{Ctx, DesktopNote, Result, Urgency};

use crate::message::{Message, Priority};

#[async_trait]
pub trait Sink: Send + Sync {
    /// What deliveries are recorded as: `desktop`.
    fn id(&self) -> &str;

    /// Another sink's id, tried once when this one fails. A sink named as a fallback is only
    /// ever tried as one.
    fn fallback(&self) -> Option<&str> {
        None
    }

    async fn deliver(&self, message: &Message, ctx: &Ctx) -> Result<()>;
}

/// A desktop notification, through `ctx.desktop`: `notify-send` on Linux, `osascript` on macOS
/// (the daemon's backend, ADR 0032 §2). Priority maps to urgency; `high` is `critical`.
pub struct DesktopSink;

#[async_trait]
impl Sink for DesktopSink {
    fn id(&self) -> &str {
        "desktop"
    }

    async fn deliver(&self, message: &Message, ctx: &Ctx) -> Result<()> {
        let urgency = match message.priority {
            Priority::Low => Urgency::Low,
            Priority::Normal => Urgency::Normal,
            Priority::High => Urgency::Critical,
        };
        ctx.desktop.show(&DesktopNote { title: message.title.clone(), body: message.body.clone(), urgency }).await
    }
}
