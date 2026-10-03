//! `Ctx` is the capability object: the only thing a module receives. Adding a field here is
//! a security decision, not a convenience.
//!
//! Forbidden, permanently: a raw `PathBuf` to the data root, a raw HTTP client, a
//! `rusqlite::Connection`, a handle to another module, anything identity-related (§1.5), a
//! raw process handle or a way to name a script outside a module's own workspace directory
//! (ADR 0010 — `Launcher` takes a logical step and a namespace-relative id, never a path).

use std::future::Future;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::bus::Emitter;
use crate::clock::Clock;
use crate::error::{Error, Result};
use crate::http::HttpGateway;
use crate::ids::ModuleId;
use crate::launcher::Launcher;
use crate::queue::QueueHandle;
use crate::retry::RetryPolicy;
use crate::store::NamespacedStore;
use crate::time::LocalTimezone;

/// Receives `(fraction, note)` from inside a queued task.
pub type ProgressFn = Arc<dyn Fn(f32, &str) + Send + Sync>;

/// This module's section of `config.toml`, as JSON.
#[derive(Clone, Debug, Default)]
pub struct ModuleConfig(Value);

impl ModuleConfig {
    pub fn new(value: Value) -> Self {
        Self(value)
    }

    pub fn raw(&self) -> &Value {
        &self.0
    }

    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => serde_json::from_value(v.clone())
                .map(Some)
                .map_err(|e| Error::invalid_params(format!("config key '{key}': {e}"))),
        }
    }
}

#[derive(Clone)]
pub struct Ctx {
    /// Store scoped to this module's namespace. Cannot read or write outside it.
    pub store: NamespacedStore,
    /// Rate-limited, on-disk-cached HTTP client. The ONLY way out to the network.
    pub http: HttpGateway,
    /// Emit-only handle onto the event bus.
    pub bus: Emitter,
    /// Enqueue work. Available to any module, not just the scheduler.
    pub queue: QueueHandle,
    /// Injectable clock. Modules must never call `Utc::now()` directly.
    pub clock: Clock,
    /// The configured `[general] local_timezone` (ADR 0009). Resolved once at daemon
    /// startup. Use with [`crate::time::local_date`] to derive a calendar date for stamping
    /// or display; never store a local-time timestamp — event and file timestamps stay UTC.
    pub local_tz: LocalTimezone,
    /// Spawn a workspace's numbered launch steps (`steps/<index>-<name>.{sh,ps1}`) and its
    /// `cleanup.{sh,ps1}` (ADR 0010 §2a). Defaults to [`Launcher::unavailable`] — the
    /// daemon has no real backend or capability-scoped wiring yet; that follows in a
    /// separate change once ADR 0010 is signed off. Modules must never call
    /// `std::process::Command` directly (§12 rule 4).
    pub launcher: Launcher,
    /// Cooperative cancellation. Long tasks must poll this.
    pub cancel: CancellationToken,
    pub config: ModuleConfig,
    pub module_id: ModuleId,
    /// Not a capability, only a reporting callback the queue installs per task.
    progress: Option<ProgressFn>,
}

impl Ctx {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: NamespacedStore,
        http: HttpGateway,
        bus: Emitter,
        queue: QueueHandle,
        clock: Clock,
        local_tz: LocalTimezone,
        cancel: CancellationToken,
        config: ModuleConfig,
        module_id: ModuleId,
    ) -> Self {
        Self {
            store,
            http,
            bus,
            queue,
            clock,
            local_tz,
            launcher: Launcher::unavailable(),
            cancel,
            config,
            module_id,
            progress: None,
        }
    }

    /// A copy for running one queued task: its own cancellation token and progress sink.
    pub fn for_task(&self, cancel: CancellationToken, progress: ProgressFn) -> Self {
        Self { cancel, progress: Some(progress), ..self.clone() }
    }

    /// Report progress from inside a queued task. No-op when running inline.
    pub fn progress(&self, fraction: f32, note: &str) {
        if let Some(p) = &self.progress {
            p(fraction.clamp(0.0, 1.0), note);
        }
    }

    /// Shared retry with exponential backoff. `f` receives the 1-based attempt number.
    /// Retries only retryable errors (see [`Error::is_retryable`]), stops early on
    /// cancellation, and returns the last error when attempts run out. The queue itself
    /// never retries (§11.2).
    pub async fn retry_with_backoff<T, F, Fut>(&self, policy: RetryPolicy, mut f: F) -> Result<T>
    where
        F: FnMut(u32) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let max = policy.max_attempts.max(1);
        let mut attempt = 1;
        loop {
            match f(attempt).await {
                Ok(v) => return Ok(v),
                Err(e) if e.is_retryable() && attempt < max && !self.cancel.is_cancelled() => {
                    tokio::select! {
                        _ = tokio::time::sleep(policy.delay_after(attempt)) => {}
                        _ = self.cancel.cancelled() => return Err(e),
                    }
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::testing::TestEnv;

    fn policy() -> RetryPolicy {
        RetryPolicy { max_attempts: 3, initial_delay: Duration::from_millis(10), max_delay: Duration::from_millis(100) }
    }

    #[tokio::test(start_paused = true)]
    async fn retries_unavailable_then_succeeds() {
        let env = TestEnv::new("fetchers");
        let calls = AtomicU32::new(0);
        let r = env
            .ctx
            .retry_with_backoff(policy(), |attempt| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt < 3 {
                        Err(Error::unavailable("network down"))
                    } else {
                        Ok(attempt)
                    }
                }
            })
            .await;
        assert_eq!(r.unwrap(), 3);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_retry_non_retryable_and_gives_up_after_max() {
        let env = TestEnv::new("fetchers");
        let calls = AtomicU32::new(0);
        let r: Result<()> = env
            .ctx
            .retry_with_backoff(policy(), |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err(Error::invalid_params("bad")) }
            })
            .await;
        assert!(r.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        calls.store(0, Ordering::SeqCst);
        let r: Result<()> = env
            .ctx
            .retry_with_backoff(policy(), |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err(Error::unavailable("down")) }
            })
            .await;
        assert!(r.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn progress_is_noop_inline_and_reports_in_task() {
        let env = TestEnv::new("fetchers");
        env.ctx.progress(0.5, "ignored");
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = seen.clone();
        let ctx =
            env.ctx.for_task(CancellationToken::new(), Arc::new(move |f, n| s.lock().unwrap().push((f, n.to_owned()))));
        ctx.progress(2.0, "page 2");
        assert_eq!(*seen.lock().unwrap(), [(1.0, "page 2".to_owned())]);
    }
}
