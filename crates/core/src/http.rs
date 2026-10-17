//! The only way out to the network. Rate limiting and caching live behind the backend, in
//! the daemon; a module never holds a raw HTTP client. Backoff does not: like the queue
//! (§11.2), the gateway never retries — a module opts into that itself, with
//! `Ctx::retry_with_backoff` around a `get` call.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::ids::ModuleId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    /// Served from the gateway's on-disk cache rather than a fresh request (ADR 0027).
    pub from_cache: bool,
    /// A cached copy served despite a failed refetch or revalidation (ADR 0027,
    /// "stale-if-error") — `from_cache` is also `true` whenever this is.
    pub stale: bool,
    /// Present on a `429`/`503`: how long the gateway is treating this host as cooling down,
    /// read from the response's own `Retry-After` (ADR 0027). The gateway never retries this
    /// itself; a module decides what to do with it.
    pub retry_after_secs: Option<u64>,
}

#[async_trait]
pub trait HttpBackend: Send + Sync {
    /// `module` lets the gateway attribute and rate-limit per caller. `credential`, when
    /// given, names a `[credentials.<name>]` entry the backend resolves and attaches as a
    /// header (ADR 0036) — the caller never says which header or which host, and never sees
    /// the secret value; both are fixed by that entry's own configuration.
    async fn get(&self, module: &ModuleId, url: &str, credential: Option<&str>) -> Result<HttpResponse>;
}

#[derive(Clone)]
pub struct HttpGateway {
    module: ModuleId,
    backend: Arc<dyn HttpBackend>,
}

impl HttpGateway {
    pub fn new(module: ModuleId, backend: Arc<dyn HttpBackend>) -> Self {
        Self { module, backend }
    }

    pub async fn get(&self, url: &str) -> Result<HttpResponse> {
        self.backend.get(&self.module, url, None).await
    }

    /// Like [`Self::get`], but attaches the named credential (ADR 0036). `credential` must
    /// name a `[credentials.<name>]` entry bound to this module; anything else is
    /// `invalid_params`.
    pub async fn get_with_credential(&self, url: &str, credential: &str) -> Result<HttpResponse> {
        self.backend.get(&self.module, url, Some(credential)).await
    }
}
