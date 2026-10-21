//! The real `HttpBackend` (ADR 0027): rate limiting, redirects, and the on-disk response
//! cache. Cross-platform — unlike the launcher, nothing here is `cfg(unix)`-gated.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use shimmer_core::{Clock, Error, HttpBackend, HttpResponse, ModuleId, Result};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::secrets::SecretStore;

/// A `[credentials.<name>]` entry (ADR 0036 §2): the one module, one host and one header a
/// named credential may ever be used for. The secret value itself is never here — resolved by
/// name through a [`SecretStore`] only at the point a request actually needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialBinding {
    pub module: ModuleId,
    /// `host:port`, matching [`host_key`]'s own bucketing.
    pub host: String,
    pub header: String,
}

/// A credential resolved for exactly one in-flight request: attached on a hop only when that
/// hop's host matches `host` (ADR 0036 §4) — never logged (§6).
struct ResolvedAuth {
    header: String,
    value: String,
    host: String,
}

/// A cooling-down host is never waited on past this if the real `Retry-After` is longer —
/// the caller gets the last-known failure back immediately instead (ADR 0027 §3).
const DEFAULT_MAX_COOLDOWN_WAIT: Duration = Duration::from_secs(30);
/// Used only when a `429`/`503` carries no usable `Retry-After`.
const DEFAULT_COOLDOWN_SECS: u64 = 30;
const MAX_REDIRECTS: u8 = 10;

/// Per-host limits. Falls back to `HttpConfig`'s `default_*` fields when a host has no entry
/// of its own.
#[derive(Clone, Copy, Debug)]
pub struct HostLimits {
    pub requests_per_sec: f64,
    pub concurrent: usize,
}

/// What `Core::new` builds once from `config.toml`'s `[http]` section (a later slice) and
/// hands to `RealHttpBackend::new`. Defaults are deliberately conservative — a module that
/// opts into `"network"` should not be able to hammer a host, or this daemon's own memory
/// or disk, by accident.
#[derive(Clone, Debug)]
pub struct HttpConfig {
    pub default_requests_per_sec: f64,
    pub default_concurrent: usize,
    /// Keyed on `host:port`, matching how `RealHttpBackend` buckets everything else.
    pub per_host: HashMap<String, HostLimits>,
    pub timeout: Duration,
    pub max_cooldown_wait: Duration,
    /// Total bytes the cache may hold per module before the oldest entries are evicted.
    pub cache_max_bytes: u64,
    /// Used when a response carries no `Cache-Control: max-age`.
    pub cache_default_ttl: Duration,
    /// A response body larger than this aborts the request — enforced while streaming, so
    /// an oversized or lying `Content-Length` never gets buffered in memory first.
    pub max_response_bytes: u64,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            default_requests_per_sec: 2.0,
            default_concurrent: 2,
            per_host: HashMap::new(),
            timeout: Duration::from_secs(30),
            max_cooldown_wait: DEFAULT_MAX_COOLDOWN_WAIT,
            cache_max_bytes: 100 * 1024 * 1024,
            cache_default_ttl: Duration::from_secs(300),
            max_response_bytes: 20 * 1024 * 1024,
        }
    }
}

/// A classic token bucket: starts full, refills continuously at `rate_per_sec`, capped at a
/// small burst (`rate_per_sec`, floor 1 — a very slow rate still allows the first request
/// through immediately rather than stalling it on an empty bucket).
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    rate_per_sec: f64,
    capacity: f64,
    last_refill: DateTime<Utc>,
}

impl TokenBucket {
    fn new(rate_per_sec: f64, now: DateTime<Utc>) -> Self {
        let capacity = rate_per_sec.max(1.0);
        Self { tokens: capacity, rate_per_sec, capacity, last_refill: now }
    }

    /// Takes one token if available, refilling first for the elapsed time. `Err` carries how
    /// long until one will be.
    fn try_take(&mut self, now: DateTime<Utc>) -> std::result::Result<(), Duration> {
        let elapsed_secs = (now - self.last_refill).num_milliseconds().max(0) as f64 / 1000.0;
        self.tokens = (self.tokens + elapsed_secs * self.rate_per_sec).min(self.capacity);
        self.last_refill = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else if self.rate_per_sec > 0.0 {
            Err(Duration::from_secs_f64(((1.0 - self.tokens) / self.rate_per_sec).max(0.0)))
        } else {
            Err(Duration::from_secs(u64::MAX / 2))
        }
    }
}

struct HostMutable {
    bucket: TokenBucket,
    cooldown_until: Option<DateTime<Utc>>,
    /// The status (`429`/`503`) that set the current cooldown, so a wait past the cap can
    /// still hand back something meaningful instead of inventing a status.
    last_cooldown_status: Option<u16>,
}

struct HostEntry {
    mutable: Mutex<HostMutable>,
    semaphore: Arc<Semaphore>,
}

/// On-disk cache metadata for one URL, within one module's cache subdirectory. Paired with a
/// same-named `.body` file holding the raw bytes (ADR 0027 §3).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CacheMeta {
    url: String,
    status: u16,
    etag: Option<String>,
    last_modified: Option<String>,
    max_age_secs: Option<u64>,
    cached_at: DateTime<Utc>,
    /// The paired `.body` file's exact length, checked on every [`ResponseCache::read`].
    /// `write` writes the body first and this meta last, so meta is the commit marker — but a
    /// crash or a swallowed disk error between the two writes can still leave this meta sitting
    /// next to a body from a *different* generation (e.g. a stale body left from a previous
    /// write, with a fresh meta failing to land). A length mismatch means the pair does not
    /// belong together, so it reads back as a miss exactly like a wholly absent pair (ADR 0027
    /// §3; PR #109 review).
    body_len: u64,
}

impl CacheMeta {
    fn refreshed(&self, now: DateTime<Utc>) -> Self {
        Self { cached_at: now, ..self.clone() }
    }
}

/// Cache-relevant fields pulled from a fresh response, before its body is read. Kept apart
/// from `CacheMeta` since a `Synthesized` cooldown response (never a real HTTP response) has
/// none of these.
#[derive(Default)]
struct ResponseMeta {
    etag: Option<String>,
    last_modified: Option<String>,
    max_age_secs: Option<u64>,
}

/// `cache/http/<module>/<hash-of-the-url>.{meta.json,body}`. Reads need no lock (a `Mutex`
/// per entry would only matter for concurrent writers of the *same* URL, which `Ctx`'s
/// serialization of a given module's calls already makes rare enough not to bother with);
/// writes go through `write_atomic` and never fail the request they were caching for.
struct ResponseCache {
    home: PathBuf,
}

impl ResponseCache {
    fn dir(&self, module: &ModuleId) -> PathBuf {
        self.home.join("cache/http").join(module.as_str())
    }

    fn paths(&self, module: &ModuleId, url: &str) -> (PathBuf, PathBuf) {
        let dir = self.dir(module);
        let key = cache_key(url);
        (dir.join(format!("{key}.meta.json")), dir.join(format!("{key}.body")))
    }

    fn read(&self, module: &ModuleId, url: &str) -> Option<(CacheMeta, Vec<u8>)> {
        let (meta_path, body_path) = self.paths(module, url);
        let meta: CacheMeta = serde_json::from_str(&std::fs::read_to_string(&meta_path).ok()?).ok()?;
        let body = std::fs::read(&body_path).ok()?;
        if body.len() as u64 != meta.body_len {
            // A torn pair from two different writes (see `write`'s doc comment) -- treat it
            // exactly like a wholly missing entry rather than serving a mismatched body.
            return None;
        }
        Some((meta, body))
    }

    /// Body first, meta last — meta is the commit marker (ADR 0027 §3). Writing the other order
    /// let a meta update outrun a failed body write: a fresh meta (new ETag) paired with the
    /// previous, stale body would then serve that stale body as fresh forever, since the next
    /// `get` would send the new ETag and accept the resulting 304 at face value (PR #109
    /// review). `meta.body_len` (checked in `read`) catches the mirror case — a body write that
    /// lands but whose meta write then fails or crashes — so neither write order can leave a
    /// torn pair readable as valid.
    fn write(&self, module: &ModuleId, url: &str, meta: &CacheMeta, body: &[u8], max_bytes: u64) {
        let (meta_path, body_path) = self.paths(module, url);
        if let Err(e) = shimmer_store::write_atomic(&body_path, body) {
            tracing::warn!(error = %e, path = %body_path.display(), "http cache: could not write body");
            return;
        }
        let meta_text = match serde_json::to_vec(meta) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "http cache: could not serialize metadata, not caching");
                return;
            }
        };
        if let Err(e) = shimmer_store::write_atomic(&meta_path, &meta_text) {
            tracing::warn!(error = %e, path = %meta_path.display(), "http cache: could not write metadata");
            return;
        }
        self.evict_if_over_cap(module, max_bytes);
    }

    /// Evicts whole `(meta, body)` pairs, oldest first, until this module's cache directory
    /// is back under `max_bytes`. Grouped by the pair's shared key so an eviction never
    /// leaves an orphaned half behind.
    fn evict_if_over_cap(&self, module: &ModuleId, max_bytes: u64) {
        let Ok(entries) = std::fs::read_dir(self.dir(module)) else { return };
        let mut by_key: HashMap<String, (Option<PathBuf>, Option<PathBuf>, u64, std::time::SystemTime)> =
            HashMap::new();
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let Ok(file_meta) = entry.metadata() else { continue };
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            let (key, is_body) = match name.strip_suffix(".meta.json") {
                Some(k) => (k.to_owned(), false),
                None => match name.strip_suffix(".body") {
                    Some(k) => (k.to_owned(), true),
                    None => continue,
                },
            };
            let slot = by_key.entry(key).or_insert((None, None, 0, std::time::SystemTime::UNIX_EPOCH));
            if is_body {
                slot.1 = Some(path)
            } else {
                slot.0 = Some(path)
            }
            slot.2 += file_meta.len();
            if let Ok(mtime) = file_meta.modified() {
                if slot.3 == std::time::SystemTime::UNIX_EPOCH || mtime < slot.3 {
                    slot.3 = mtime;
                }
            }
        }
        let total: u64 = by_key.values().map(|(_, _, size, _)| size).sum();
        if total <= max_bytes {
            return;
        }
        let mut pairs: Vec<_> = by_key.into_values().collect();
        pairs.sort_by_key(|(_, _, _, mtime)| *mtime);
        let mut over = total - max_bytes;
        for (meta_path, body_path, size, _) in pairs {
            if over == 0 {
                break;
            }
            if let Some(p) = meta_path {
                let _ = std::fs::remove_file(p);
            }
            if let Some(p) = body_path {
                let _ = std::fs::remove_file(p);
            }
            over = over.saturating_sub(size);
        }
    }
}

/// A stable, dependency-free (FNV-1a) hash of the URL, used only as a filesystem-safe cache
/// filename — not a security boundary, so collision resistance beyond "very unlikely for a
/// module's own modest URL set" is not a design goal.
fn cache_key(url: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in url.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

/// The gateway never retries (§11.2's principle, applied here too) — one network attempt (or
/// one synthesized cooldown response, or one cache hit) per `get` call. Redirects are their
/// own internal loop, not a retry: each hop is a different request to a different URL.
pub struct RealHttpBackend {
    client: reqwest::Client,
    clock: Clock,
    config: HttpConfig,
    hosts: Mutex<HashMap<String, Arc<HostEntry>>>,
    cache: ResponseCache,
    credentials: HashMap<String, CredentialBinding>,
    secrets: Arc<dyn SecretStore>,
    /// Why `secrets.toml` could not be loaded, if it existed and failed (ADR 0036 §3). Quoted
    /// on a credentialed request that finds no value, so "your secrets file is mode 0644"
    /// reaches the caller instead of a bare "no secret value available".
    secrets_problem: Option<String>,
}

impl RealHttpBackend {
    pub fn new(
        home: PathBuf,
        config: HttpConfig,
        clock: Clock,
        credentials: HashMap<String, CredentialBinding>,
        secrets: Arc<dyn SecretStore>,
        secrets_problem: Option<String>,
    ) -> Result<Self> {
        // Every request this daemon makes, from any network-capable module, goes out
        // identified -- an anonymous UA gets silently dropped by some hosts and makes a
        // robots.txt rule impossible to address correctly (PR #126 review). This is a
        // single daemon-wide identity, not the fetchers-specific `Shimmer-Fetchers/<version>`
        // ADR 0028 §7 describes: there is no per-module custom-header plumbing yet (ADR 0027
        // deferred it), and this client is shared by every module that declares `"network"`.
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!("Shimmer/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::internal(format!("building http client: {e}")))?;
        Ok(Self {
            client,
            clock,
            config,
            hosts: Mutex::new(HashMap::new()),
            cache: ResponseCache { home },
            credentials,
            secrets,
            secrets_problem,
        })
    }

    /// Looks up `name` in `[credentials.*]`, checks it's bound to the calling module, and
    /// resolves its secret value (ADR 0036 §2/§3). The module never sees the value — only
    /// this backend does, for exactly the lifetime of one request.
    fn resolve_credential(&self, module: &ModuleId, name: &str) -> Result<ResolvedAuth> {
        let binding = self
            .credentials
            .get(name)
            .ok_or_else(|| Error::invalid_params(format!("no credential named '{name}' is configured")))?;
        if &binding.module != module {
            return Err(Error::invalid_params(format!("credential '{name}' is not configured for module '{module}'")));
        }
        let value = self.secrets.resolve(name).ok_or_else(|| match &self.secrets_problem {
            Some(why) => Error::unavailable(format!("credential '{name}' is unavailable: {why}")),
            None => Error::unavailable(format!(
                "credential '{name}' has no secret value available (set it in secrets.toml or SHIMMER_SECRET_*)"
            )),
        })?;
        Ok(ResolvedAuth { header: binding.header.clone(), value, host: binding.host.clone() })
    }

    fn host_entry(&self, host_key: &str) -> Arc<HostEntry> {
        let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        hosts
            .entry(host_key.to_owned())
            .or_insert_with(|| {
                let limits = self.config.per_host.get(host_key).copied().unwrap_or(HostLimits {
                    requests_per_sec: self.config.default_requests_per_sec,
                    concurrent: self.config.default_concurrent,
                });
                Arc::new(HostEntry {
                    mutable: Mutex::new(HostMutable {
                        bucket: TokenBucket::new(limits.requests_per_sec, self.clock.now()),
                        cooldown_until: None,
                        last_cooldown_status: None,
                    }),
                    semaphore: Arc::new(Semaphore::new(limits.concurrent.max(1))),
                })
            })
            .clone()
    }

    fn is_fresh(&self, meta: &CacheMeta) -> bool {
        let ttl = meta.max_age_secs.map(Duration::from_secs).unwrap_or(self.config.cache_default_ttl);
        let ttl = chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::zero());
        self.clock.now() < meta.cached_at + ttl
    }

    /// Rate limit, wait out any cooldown (or synthesize past it), then send exactly one
    /// request, optionally conditional on a cached entry's validator. Never follows a
    /// redirect itself — the caller's loop decides that.
    async fn send_one(
        &self,
        module: &ModuleId,
        url: &reqwest::Url,
        if_none_match: Option<&str>,
        if_modified_since: Option<&str>,
        auth: Option<&ResolvedAuth>,
    ) -> Result<HopOutcome> {
        let host_key = host_key(url)?;
        let entry = self.host_entry(&host_key);

        let cooldown_remaining = {
            let m = entry.mutable.lock().unwrap_or_else(|e| e.into_inner());
            m.cooldown_until.and_then(|until| {
                let now = self.clock.now();
                (until > now).then(|| (until - now).to_std().unwrap_or(Duration::ZERO))
            })
        };
        if let Some(remaining) = cooldown_remaining {
            if remaining > self.config.max_cooldown_wait {
                let status =
                    entry.mutable.lock().unwrap_or_else(|e| e.into_inner()).last_cooldown_status.unwrap_or(503);
                tracing::debug!(module = %module, host = %host_key, remaining_s = remaining.as_secs(), "http: cooling down past the wait cap, not sending");
                return Ok(HopOutcome::Synthesized(HttpResponse {
                    status,
                    body: Vec::new(),
                    from_cache: false,
                    stale: false,
                    retry_after_secs: Some(remaining.as_secs().max(1)),
                }));
            }
            tokio::time::sleep(remaining).await;
        }

        loop {
            let wait = {
                let mut m = entry.mutable.lock().unwrap_or_else(|e| e.into_inner());
                match m.bucket.try_take(self.clock.now()) {
                    Ok(()) => break,
                    Err(wait) => wait,
                }
            };
            tokio::time::sleep(wait).await;
        }

        // Held past `send_one`'s own return and only dropped once the body is fully read
        // (`finish`'s caller holds it through `read_body_capped`) -- the per-host cap bounds
        // concurrent *downloads*, not just concurrent "send the headers and let go" (PR #109
        // review).
        let permit = entry
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::internal("http concurrency limiter closed unexpectedly"))?;

        let mut req = self.client.get(url.clone());
        if let Some(etag) = if_none_match {
            req = req.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        if let Some(lm) = if_modified_since {
            req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
        }
        // Attached only when THIS hop's host matches the credential's bound host (ADR 0036
        // §4) -- re-checked on every hop, not just the first, so a cross-host redirect simply
        // stops carrying it instead of needing a separate "strip on redirect" rule.
        let authenticated = auth.is_some_and(|a| a.host == host_key);
        if let Some(a) = auth {
            if authenticated {
                req = req.header(a.header.as_str(), a.value.as_str());
            }
        }
        tracing::debug!(module = %module, url = %url.as_str(), revalidating = if_none_match.is_some(), authenticated, "http: sending");
        let resp = req.send().await.map_err(|e| Error::unavailable(format!("{url}: {e}")))?;
        Ok(HopOutcome::Response(resp, permit))
    }

    /// Follows redirects (each hop through its own host's limiter) until a terminal
    /// response, sending the revalidation headers (if any) only on the first hop — a
    /// redirect target is a different URL, with nothing of its own cached to revalidate
    /// against.
    async fn fetch(
        &self,
        module: &ModuleId,
        mut current: reqwest::Url,
        revalidate: Option<&CacheMeta>,
        auth: Option<&ResolvedAuth>,
    ) -> Result<(HttpResponse, ResponseMeta)> {
        for hop in 0..=MAX_REDIRECTS {
            let (if_none_match, if_modified_since) = if hop == 0 {
                (revalidate.and_then(|m| m.etag.as_deref()), revalidate.and_then(|m| m.last_modified.as_deref()))
            } else {
                (None, None)
            };
            match self.send_one(module, &current, if_none_match, if_modified_since, auth).await? {
                HopOutcome::Synthesized(r) => return Ok((r, ResponseMeta::default())),
                HopOutcome::Response(resp, permit) => {
                    if resp.status().is_redirection() {
                        if hop == MAX_REDIRECTS {
                            return Err(Error::unavailable(format!("too many redirects fetching {current}")));
                        }
                        if let Some(next) = redirect_target(&current, &resp) {
                            // This hop's permit is dropped here (a redirect has no body worth
                            // holding it for); the next hop acquires its own, from its own
                            // host's semaphore.
                            current = next;
                            continue;
                        }
                    }
                    return self.finish(&current, resp, permit).await;
                }
            }
        }
        unreachable!("the loop above always returns before exhausting its range")
    }

    /// Reads the body (capped, streamed) and, on a `429`/`503`, records this host's cooldown
    /// before returning. Extracts cache-relevant headers before consuming the body, since
    /// they're unavailable afterward. Takes ownership of this hop's concurrency permit purely
    /// to hold it alive (and so release it) across the body read -- the per-host `concurrent`
    /// cap must bound the download, not just the time to get headers back (PR #109 review).
    async fn finish(
        &self,
        url: &reqwest::Url,
        resp: reqwest::Response,
        _permit: OwnedSemaphorePermit,
    ) -> Result<(HttpResponse, ResponseMeta)> {
        let status = resp.status().as_u16();
        let retry_after = retry_after_secs(&resp);
        let response_meta = ResponseMeta {
            etag: header_str(&resp, reqwest::header::ETAG),
            last_modified: header_str(&resp, reqwest::header::LAST_MODIFIED),
            max_age_secs: max_age_from_cache_control(&resp),
        };
        if status == 429 || status == 503 {
            self.record_cooldown(url, status, retry_after)?;
        }
        let body = read_body_capped(resp, self.config.max_response_bytes).await?;
        let http_response = HttpResponse {
            status,
            body,
            from_cache: false,
            stale: false,
            retry_after_secs: (status == 429 || status == 503).then(|| retry_after.unwrap_or(DEFAULT_COOLDOWN_SECS)),
        };
        Ok((http_response, response_meta))
    }

    fn record_cooldown(&self, url: &reqwest::Url, status: u16, retry_after_secs: Option<u64>) -> Result<()> {
        let host_key = host_key(url)?;
        let entry = self.host_entry(&host_key);
        let secs = retry_after_secs.unwrap_or(DEFAULT_COOLDOWN_SECS);
        let mut m = entry.mutable.lock().unwrap_or_else(|e| e.into_inner());
        m.cooldown_until = Some(self.clock.now() + chrono::Duration::seconds(secs as i64));
        m.last_cooldown_status = Some(status);
        Ok(())
    }
}

enum HopOutcome {
    /// The permit is this hop's own host's concurrency slot, carried along so the caller can
    /// decide how long to hold it (redirect: drop now; terminal response: hold through the body
    /// read in `finish`) instead of `send_one` releasing it the moment headers arrive.
    Response(reqwest::Response, OwnedSemaphorePermit),
    Synthesized(HttpResponse),
}

#[async_trait]
impl HttpBackend for RealHttpBackend {
    async fn get(&self, module: &ModuleId, url: &str, credential: Option<&str>) -> Result<HttpResponse> {
        let parsed = parse_checked_url(url)?;
        let auth = credential.map(|name| self.resolve_credential(module, name)).transpose()?;
        // The per-hop host check (§4) exists so a *redirect* to another host silently drops the
        // header. For the URL the caller actually asked for, silence is the wrong answer: it
        // sends an unauthenticated request and hands back an opaque 401 the caller can't tell
        // apart from a bad token (#257's review). Mismatch on the first hop is an error that
        // names both hosts.
        if let (Some(a), Some(name)) = (&auth, credential) {
            let asked = host_key(&parsed)?;
            if a.host != asked {
                return Err(Error::invalid_params(format!(
                    "credential '{name}' is bound to host '{}', but this request is for '{asked}'",
                    a.host
                )));
            }
        }
        // A credentialed response is never cached (ADR 0036 §5): a credential can be rotated
        // or revoked at any time, and a cached body from before that must never outlive it.
        let cacheable = auth.is_none();
        let cached = if cacheable { self.cache.read(module, url) } else { None };

        if let Some((meta, body)) = &cached {
            if self.is_fresh(meta) {
                return Ok(HttpResponse {
                    status: meta.status,
                    body: body.clone(),
                    from_cache: true,
                    stale: false,
                    retry_after_secs: None,
                });
            }
        }

        match self.fetch(module, parsed, cached.as_ref().map(|(m, _)| m), auth.as_ref()).await {
            Ok((resp, response_meta)) if resp.status == 304 => {
                let Some((meta, body)) = cached else {
                    // No cached body to pair with a bare 304 -- cannot happen in practice
                    // (conditional headers are only ever sent when `cached` is `Some`), but
                    // degrade to the plain response rather than inventing one.
                    return Ok(resp);
                };
                let refreshed = meta.refreshed(self.clock.now());
                self.cache.write(module, url, &refreshed, &body, self.config.cache_max_bytes);
                let _ = response_meta;
                Ok(HttpResponse {
                    status: refreshed.status,
                    body,
                    from_cache: true,
                    stale: false,
                    retry_after_secs: None,
                })
            }
            Ok((resp, response_meta)) => {
                if resp.status == 200 && cacheable {
                    let meta = CacheMeta {
                        url: url.to_owned(),
                        status: resp.status,
                        etag: response_meta.etag,
                        last_modified: response_meta.last_modified,
                        max_age_secs: response_meta.max_age_secs,
                        cached_at: self.clock.now(),
                        body_len: resp.body.len() as u64,
                    };
                    self.cache.write(module, url, &meta, &resp.body, self.config.cache_max_bytes);
                }
                Ok(resp)
            }
            Err(e) => {
                if let Some((meta, body)) = cached {
                    tracing::warn!(error = %e, url, "http: refetch failed, serving stale cache (ADR 0027 §3)");
                    return Ok(HttpResponse {
                        status: meta.status,
                        body,
                        from_cache: true,
                        stale: true,
                        retry_after_secs: None,
                    });
                }
                Err(e)
            }
        }
    }
}

async fn read_body_capped(resp: reqwest::Response, max_bytes: u64) -> Result<Vec<u8>> {
    let mut stream = resp.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Error::unavailable(format!("reading response body: {e}")))?;
        if body.len() as u64 + chunk.len() as u64 > max_bytes {
            return Err(Error::unavailable(format!("response body exceeds the {max_bytes}-byte cap")));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn header_str(resp: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    resp.headers().get(name)?.to_str().ok().map(str::to_owned)
}

fn max_age_from_cache_control(resp: &reqwest::Response) -> Option<u64> {
    header_str(resp, reqwest::header::CACHE_CONTROL)?
        .split(',')
        .find_map(|part| part.trim().strip_prefix("max-age=").and_then(|v| v.parse::<u64>().ok()))
}

fn parse_checked_url(url: &str) -> Result<reqwest::Url> {
    let parsed = reqwest::Url::parse(url).map_err(|e| Error::invalid_params(format!("invalid url '{url}': {e}")))?;
    check_scheme(&parsed)?;
    Ok(parsed)
}

fn check_scheme(url: &reqwest::Url) -> Result<()> {
    match url.scheme() {
        "http" | "https" => Ok(()),
        other => Err(Error::invalid_params(format!("unsupported url scheme '{other}' in '{url}'; only http/https"))),
    }
}

fn host_key(url: &reqwest::Url) -> Result<String> {
    let host = url.host_str().ok_or_else(|| Error::invalid_params(format!("http url has no host: {url}")))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| Error::invalid_params(format!("http url has no known port: {url}")))?;
    Ok(format!("{host}:{port}"))
}

fn redirect_target(base: &reqwest::Url, resp: &reqwest::Response) -> Option<reqwest::Url> {
    let location = resp.headers().get(reqwest::header::LOCATION)?.to_str().ok()?;
    let next = base.join(location).ok()?;
    (check_scheme(&next).is_ok()).then_some(next)
}

fn retry_after_secs(resp: &reqwest::Response) -> Option<u64> {
    resp.headers().get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim().parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use shimmer_core::ErrorCode;
    use tempfile::TempDir;

    use super::*;

    const OK: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
    const NOT_MODIFIED: &str = "HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\n\r\n";

    /// One response, written in parts: each part is sent after waiting its `Duration`. A plain
    /// response is one part with no wait; a slow body is the head and some bytes, then the rest
    /// later.
    type Reply = Vec<(&'static str, Duration)>;

    /// Hand-rolled HTTP/1.1 server on an ephemeral loopback port, one response per connection,
    /// recording each request's raw head (and when it arrived) so a test can assert on what was
    /// actually sent (e.g. `If-None-Match`). No real network, no new dependency (ADR 0027 §9).
    ///
    /// Every test's server is this one, so every response goes through [`send`], which says
    /// `Connection: close`. A test with its own listener once missed that and flaked on macOS
    /// (#134, then again in #246's CI); `only_test_server_listens` keeps it that way.
    struct TestServer {
        addr: std::net::SocketAddr,
        /// Each request's raw head and how long after the server started it arrived.
        requests: Arc<Mutex<Vec<(String, Duration)>>>,
    }

    /// Writes `reply`. One response per connection, then the connection closes: say so.
    /// Without it the client keeps the connection for its next request, and when that races
    /// the close it fails or (for a refetch) serves the stale copy (#134).
    fn send(stream: &mut std::net::TcpStream, reply: &[(&str, Duration)]) {
        for (i, (part, wait)) in reply.iter().enumerate() {
            std::thread::sleep(*wait);
            let part = if i == 0 { part.replacen("\r\n", "\r\nConnection: close\r\n", 1) } else { (*part).to_owned() };
            if stream.write_all(part.as_bytes()).is_err() {
                return;
            }
        }
    }

    impl TestServer {
        /// Serves exactly `responses`, in order, one connection after another.
        fn start(responses: Vec<&'static str>) -> Self {
            Self::serve(false, responses.len(), move |i, _| vec![(responses[i], Duration::ZERO)])
        }

        /// Serves `connections` connections, each on its own thread the moment it's accepted, so
        /// the server never serialises requests itself; `reply` picks each response from the
        /// request.
        fn start_concurrent(connections: usize, reply: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Self {
            Self::serve(true, connections, move |_, request| reply(request))
        }

        fn serve(
            concurrent: bool,
            connections: usize,
            reply: impl Fn(usize, &str) -> Reply + Send + Sync + 'static,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let (recorded, reply) = (requests.clone(), Arc::new(reply));
            let started = std::time::Instant::now();
            std::thread::spawn(move || {
                for i in 0..connections {
                    let Ok((mut stream, _)) = listener.accept() else { return };
                    let (recorded, reply) = (recorded.clone(), reply.clone());
                    let mut handle = move || {
                        let mut buf = [0u8; 8192];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                        recorded.lock().unwrap().push((request.clone(), started.elapsed()));
                        send(&mut stream, &reply(i, &request));
                    };
                    if concurrent {
                        std::thread::spawn(handle);
                    } else {
                        handle();
                    }
                }
            });
            Self { addr, requests }
        }

        fn url(&self, path: &str) -> String {
            format!("http://{}{}", self.addr, path)
        }

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        fn last_request_sent(&self, header: &str) -> bool {
            self.requests
                .lock()
                .unwrap()
                .last()
                .is_some_and(|(r, _)| r.to_ascii_lowercase().contains(&header.to_ascii_lowercase()))
        }

        /// How long after the server started the first request for `path` arrived.
        fn arrived(&self, path: &str) -> Option<Duration> {
            let first_line = format!(" {path} ");
            self.requests.lock().unwrap().iter().find(|(r, _)| r.contains(&first_line)).map(|(_, at)| *at)
        }
    }

    struct TestSecrets(HashMap<String, String>);

    impl SecretStore for TestSecrets {
        fn resolve(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }

    fn backend(config: HttpConfig, clock: Clock) -> (RealHttpBackend, TempDir) {
        backend_with_credentials(config, clock, HashMap::new(), HashMap::new())
    }

    fn backend_with_credentials(
        config: HttpConfig,
        clock: Clock,
        credentials: HashMap<String, CredentialBinding>,
        secrets: HashMap<String, String>,
    ) -> (RealHttpBackend, TempDir) {
        let home = TempDir::new().unwrap();
        let b = RealHttpBackend::new(
            home.path().to_path_buf(),
            config,
            clock,
            credentials,
            Arc::new(TestSecrets(secrets)),
            None,
        )
        .unwrap();
        (b, home)
    }

    fn generous_config() -> HttpConfig {
        HttpConfig { default_requests_per_sec: 1000.0, default_concurrent: 10, ..HttpConfig::default() }
    }

    #[tokio::test]
    async fn non_http_scheme_is_rejected_before_any_network_work() {
        let (b, _home) = backend(generous_config(), Clock::system());
        let err = b.get(&ModuleId::new("test"), "ftp://example.com/file", None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn a_plain_get_round_trips_status_and_body() {
        let server = TestServer::start(vec![OK]);
        let (b, _home) = backend(generous_config(), Clock::system());
        let resp = b.get(&ModuleId::new("test"), &server.url("/a"), None).await.unwrap();
        assert_eq!((resp.status, resp.body), (200, b"ok".to_vec()));
        assert!(!resp.from_cache && !resp.stale && resp.retry_after_secs.is_none());
    }

    #[tokio::test]
    async fn every_request_identifies_itself_with_a_user_agent() {
        // Every module sharing this backend (not just fetchers) must never send an anonymous
        // request -- some hosts silently drop one, and a robots.txt rule is unenforceable
        // against a client that never says who it is (PR #126 review).
        let server = TestServer::start(vec![OK]);
        let (b, _home) = backend(generous_config(), Clock::system());
        b.get(&ModuleId::new("test"), &server.url("/a"), None).await.unwrap();
        assert!(
            server.last_request_sent(&format!("User-Agent: Shimmer/{}", env!("CARGO_PKG_VERSION"))),
            "the daemon-wide user agent must be sent on every request"
        );
    }

    #[tokio::test]
    async fn the_first_request_to_a_host_never_waits_even_at_a_slow_rate() {
        // The bucket starts full, so the very first request through a fresh host entry must
        // not stall on it, however slow the configured steady rate is.
        let server = TestServer::start(vec![OK]);
        let mut cfg = generous_config();
        cfg.default_requests_per_sec = 0.01;
        let (b, _home) = backend(cfg, Clock::system());
        tokio::time::timeout(Duration::from_secs(2), b.get(&ModuleId::new("test"), &server.url("/a"), None))
            .await
            .expect("the first request must not wait for a token")
            .unwrap();
    }

    #[tokio::test]
    async fn two_different_hosts_never_share_a_bucket_or_a_concurrency_cap() {
        let a = TestServer::start(vec![OK]);
        let b_server = TestServer::start(vec![OK]);
        let mut cfg = generous_config();
        cfg.default_requests_per_sec = 0.001; // so a shared bucket would starve the second host
        cfg.default_concurrent = 1;
        let (backend, _home) = backend(cfg, Clock::system());
        let module = ModuleId::new("test");
        tokio::time::timeout(Duration::from_secs(2), async {
            backend.get(&module, &a.url("/a"), None).await.unwrap();
            backend.get(&module, &b_server.url("/b"), None).await.unwrap();
        })
        .await
        .expect("a different host must get its own fresh bucket, not share the first host's");
    }

    #[tokio::test]
    async fn a_429_records_a_cooldown_that_a_bounded_wait_then_honors() {
        let retry_after_1s = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\n\r\n";
        let server = TestServer::start(vec![retry_after_1s, OK]);
        let mut cfg = generous_config();
        cfg.max_cooldown_wait = Duration::from_secs(5); // comfortably above the 1s Retry-After
        let (b, _home) = backend(cfg, Clock::system());
        let module = ModuleId::new("test");

        let first = b.get(&module, &server.url("/a"), None).await.unwrap();
        assert_eq!((first.status, first.retry_after_secs), (429, Some(1)));

        // Second call hits the same host while it's cooling down. Since 1s <= the 5s cap,
        // this must actually wait out the cooldown and then succeed for real -- one of the
        // few places in this test suite where a short, deliberate real wait is unavoidable,
        // since a real wait is exactly the behavior under test.
        let second = tokio::time::timeout(Duration::from_secs(3), b.get(&module, &server.url("/a"), None))
            .await
            .unwrap()
            .unwrap();
        assert_eq!((second.status, second.body), (200, b"ok".to_vec()));
        assert_eq!(server.request_count(), 2, "it waited and sent a real second request, not a synthesized one");
    }

    #[tokio::test]
    async fn a_cooldown_past_the_wait_cap_is_synthesized_without_touching_the_network() {
        // A fake, unadvanced clock: the exact remaining-seconds math (no real time may pass
        // between recording the cooldown and checking it) is itself part of what's asserted.
        let clock = Clock::fake(DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap().to_utc());
        let retry_after_1h = "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 3600\r\nContent-Length: 0\r\n\r\n";
        let server = TestServer::start(vec![retry_after_1h]);
        let mut cfg = generous_config();
        cfg.max_cooldown_wait = Duration::from_millis(50);
        let (b, _home) = backend(cfg, clock);
        let module = ModuleId::new("test");

        let first = b.get(&module, &server.url("/a"), None).await.unwrap();
        assert_eq!((first.status, first.retry_after_secs), (503, Some(3600)));

        // The cooldown is an hour, the cap is 50ms -- the next call must return instantly
        // with a synthesized response, never reaching the server at all.
        let second = tokio::time::timeout(Duration::from_millis(500), b.get(&module, &server.url("/a"), None))
            .await
            .unwrap()
            .unwrap();
        assert_eq!((second.status, second.retry_after_secs), (503, Some(3600)));
        assert_eq!(server.request_count(), 1, "the second call must never touch the network");
    }

    #[tokio::test]
    async fn a_cross_host_redirect_is_followed_and_rate_limited_by_the_new_host() {
        let b_server = TestServer::start(vec![OK]);
        let redirect = format!("HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\n\r\n", b_server.url("/b"));
        let a_server = TestServer::start(vec![Box::leak(redirect.into_boxed_str())]);
        let (backend, _home) = backend(generous_config(), Clock::system());

        let resp = backend.get(&ModuleId::new("test"), &a_server.url("/a"), None).await.unwrap();
        assert_eq!((resp.status, resp.body), (200, b"ok".to_vec()));
        assert_eq!(b_server.request_count(), 1, "the redirect target must actually have been requested");
    }

    #[tokio::test]
    async fn a_fresh_cache_entry_is_served_without_touching_the_network_again() {
        // Only one response scripted: a second network hit would panic the server thread.
        let server = TestServer::start(vec![OK]);
        let (b, _home) = backend(generous_config(), Clock::system());
        let module = ModuleId::new("test");

        let first = b.get(&module, &server.url("/a"), None).await.unwrap();
        assert!(!first.from_cache);

        let second = b.get(&module, &server.url("/a"), None).await.unwrap();
        assert_eq!((second.status, second.body), (200, b"ok".to_vec()));
        assert!(second.from_cache && !second.stale);
        assert_eq!(server.request_count(), 1, "a fresh cache hit must not touch the network");
    }

    #[tokio::test]
    async fn an_expired_cache_entry_is_refetched() {
        let clock = Clock::fake(DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap().to_utc());
        let server = TestServer::start(vec![OK, OK]);
        let mut cfg = generous_config();
        cfg.cache_default_ttl = Duration::from_secs(60);
        let (b, _home) = backend(cfg, clock.clone());
        let module = ModuleId::new("test");

        b.get(&module, &server.url("/a"), None).await.unwrap();
        clock.advance(Duration::from_secs(61));
        let second = b.get(&module, &server.url("/a"), None).await.unwrap();
        assert!(!second.from_cache, "past its TTL, the cache entry must trigger a real refetch");
        assert_eq!(server.request_count(), 2);
    }

    #[tokio::test]
    async fn an_etag_is_revalidated_with_if_none_match_and_a_304_serves_the_cached_body() {
        let etag_ok = "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: 2\r\n\r\nok";
        let server = TestServer::start(vec![etag_ok, NOT_MODIFIED]);
        let mut cfg = generous_config();
        cfg.cache_default_ttl = Duration::ZERO; // expires immediately, forcing a revalidation
        let (b, _home) = backend(cfg, Clock::system());
        let module = ModuleId::new("test");

        let first = b.get(&module, &server.url("/a"), None).await.unwrap();
        assert_eq!(first.body, b"ok".to_vec());

        let second = b.get(&module, &server.url("/a"), None).await.unwrap();
        assert_eq!((second.status, second.body), (200, b"ok".to_vec()), "a 304 must still hand back the cached body");
        assert!(second.from_cache && !second.stale);
        assert_eq!(server.request_count(), 2, "the TTL expiry must have triggered a real revalidation request");
        assert!(
            server.last_request_sent("If-None-Match: \"v1\""),
            "the cached ETag must be sent back as If-None-Match"
        );
    }

    #[tokio::test]
    async fn an_oversized_response_is_rejected_without_buffering_it_all() {
        let huge = format!("HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{}", "x".repeat(1000));
        let server = TestServer::start(vec![Box::leak(huge.into_boxed_str())]);
        let mut cfg = generous_config();
        cfg.max_response_bytes = 10;
        let (b, _home) = backend(cfg, Clock::system());

        let err = b.get(&ModuleId::new("test"), &server.url("/a"), None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Unavailable);
    }

    #[tokio::test]
    async fn two_modules_never_share_a_cache_entry_for_the_identical_url() {
        let server = TestServer::start(vec![OK, OK]);
        let (b, _home) = backend(generous_config(), Clock::system());
        let url = server.url("/a");

        b.get(&ModuleId::new("one"), &url, None).await.unwrap();
        let second = b.get(&ModuleId::new("two"), &url, None).await.unwrap();
        assert!(!second.from_cache, "a different module must never see the first module's cache entry");
        assert_eq!(server.request_count(), 2);
    }

    #[tokio::test]
    async fn a_failed_refetch_serves_the_stale_cached_copy() {
        let clock = Clock::fake(DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap().to_utc());
        let server = TestServer::start(vec![OK]);
        let url = server.url("/a");
        let mut cfg = generous_config();
        cfg.cache_default_ttl = Duration::from_secs(60);
        let (b, _home) = backend(cfg, clock.clone());
        let module = ModuleId::new("test");

        let first = b.get(&module, &url, None).await.unwrap();
        assert!(!first.from_cache);

        clock.advance(Duration::from_secs(61));
        // The server has already served its one scripted response and its listener thread
        // has exited; give it a moment to actually close the port so the next connection
        // attempt fails instead of racing the thread's shutdown.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let second = b.get(&module, &url, None).await.unwrap();
        assert_eq!((second.status, second.body), (200, b"ok".to_vec()));
        assert!(second.from_cache && second.stale, "a failed refetch must serve the stale cached copy, marked as such");
    }

    #[test]
    fn a_body_that_does_not_match_its_meta_reads_back_as_a_miss() {
        // Simulates the torn-pair case `write`'s doc comment describes: a body lands, but the
        // meta next to it belongs to a different generation (stale, or from a failed write).
        let home = TempDir::new().unwrap();
        let cache = ResponseCache { home: home.path().to_path_buf() };
        let module = ModuleId::new("test");
        let t0 = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap().to_utc();
        let meta = CacheMeta {
            url: "http://a".to_owned(),
            status: 200,
            etag: None,
            last_modified: None,
            max_age_secs: None,
            cached_at: t0,
            body_len: 2,
        };
        cache.write(&module, "http://a", &meta, b"ok", u64::MAX);
        assert_eq!(cache.read(&module, "http://a").unwrap().1, b"ok".to_vec());

        // Overwrite just the body, as a torn write would -- the meta's `body_len` no longer
        // matches.
        let (_meta_path, body_path) = cache.paths(&module, "http://a");
        std::fs::write(&body_path, b"a much longer stale body").unwrap();
        assert!(cache.read(&module, "http://a").is_none(), "a body/meta length mismatch must read back as a miss");
    }

    #[tokio::test]
    async fn the_concurrency_permit_is_held_through_the_body_read_not_just_the_headers() {
        // Each connection is handled on its own thread the instant it's accepted -- the server
        // itself must not be the thing serializing the two requests, only the backend's
        // per-host semaphore may do that. `/a` gets a slow body (headers + one byte, a real
        // sleep, then the rest); the server records when `/b` actually reached it. With
        // `default_concurrent = 1` on one shared host, `/b` must not reach the server until
        // `/a`'s body (not just its headers) has been fully read.
        let server = TestServer::start_concurrent(2, |request| {
            if request.contains(" /a ") {
                vec![
                    ("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\no", Duration::ZERO),
                    ("k", Duration::from_millis(200)),
                ]
            } else {
                vec![(OK, Duration::ZERO)]
            }
        });

        let mut cfg = generous_config();
        cfg.default_concurrent = 1;
        let (b, _home) = backend(cfg, Clock::system());
        let module = ModuleId::new("test");

        let (url_a, url_b) = (server.url("/a"), server.url("/b"));
        let (ra, rb) = tokio::join!(b.get(&module, &url_a, None), b.get(&module, &url_b, None));
        assert_eq!(ra.unwrap().body, b"ok".to_vec());
        assert_eq!(rb.unwrap().body, b"ok".to_vec());

        let b_arrived = server.arrived("/b").expect("the /b request never reached the server");
        assert!(
            b_arrived >= Duration::from_millis(150),
            "the /b request reached the server after only {b_arrived:?} -- the per-host \
             concurrency permit must be held until /a's body is fully read, not released once \
             /a's headers come back"
        );
    }

    /// Keeps #134 fixed: only `TestServer` opens a listener in these tests, so every response
    /// goes through `send` and says `Connection: close`. A test with its own listener missed
    /// that once and flaked on macOS CI.
    #[test]
    fn only_test_server_listens() {
        let needle = ["TcpListener", "::bind("].concat();
        let listeners = include_str!("http_backend.rs").matches(needle.as_str()).count();
        assert_eq!(
            listeners, 1,
            "a test opens its own listener: use TestServer::start or TestServer::start_concurrent, \
             so its responses say Connection: close (#134)"
        );
    }

    #[test]
    fn eviction_removes_the_oldest_pair_first_and_never_leaves_an_orphan() {
        let home = TempDir::new().unwrap();
        let cache = ResponseCache { home: home.path().to_path_buf() };
        let module = ModuleId::new("test");
        let meta = |url: &str, at: DateTime<Utc>| CacheMeta {
            url: url.to_owned(),
            status: 200,
            etag: None,
            last_modified: None,
            max_age_secs: None,
            cached_at: at,
            body_len: 10_000,
        };
        let t0 = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap().to_utc();
        // The cap comfortably fits one pair (body + its meta.json) but not two.
        let cap = 10_500;

        cache.write(&module, "http://a", &meta("http://a", t0), &[0u8; 10_000], cap);
        std::thread::sleep(Duration::from_millis(10)); // distinct mtimes to order eviction by
        cache.write(&module, "http://b", &meta("http://b", t0), &[0u8; 10_000], cap);

        assert!(cache.read(&module, "http://a").is_none(), "the older pair must be evicted");
        assert!(cache.read(&module, "http://b").is_some(), "the newer pair must survive");
        let (meta_path, body_path) = cache.paths(&module, "http://a");
        assert!(
            !meta_path.exists() && !body_path.exists(),
            "eviction must remove both halves of a pair, never just one"
        );
    }

    #[test]
    fn token_bucket_refills_over_time_and_reports_a_bounded_wait() {
        let t0 = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap().to_utc();
        let mut bucket = TokenBucket::new(2.0, t0); // capacity 2.0
        assert_eq!(bucket.try_take(t0), Ok(()));
        assert_eq!(bucket.try_take(t0), Ok(()));
        // Bucket is now empty; immediately retrying must report a wait, not succeed.
        let wait = bucket.try_take(t0).unwrap_err();
        assert!(wait > Duration::ZERO && wait <= Duration::from_secs(1));
        // After a full second at rate 2/s, at least one token is available again.
        assert_eq!(bucket.try_take(t0 + chrono::Duration::seconds(1)), Ok(()));
    }

    // ------------------------------------------------------- ADR 0036: credentials

    fn credential(module: &str, host: &str, header: &str) -> CredentialBinding {
        CredentialBinding { module: ModuleId::new(module), host: host.to_owned(), header: header.to_owned() }
    }

    #[tokio::test]
    async fn an_unconfigured_credential_name_is_rejected() {
        let (b, _home) = backend(generous_config(), Clock::system());
        let err = b.get(&ModuleId::new("test"), "http://example.test/a", Some("nope")).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn a_credential_bound_to_a_different_module_is_rejected() {
        let credentials =
            HashMap::from([("my-cred".to_owned(), credential("fetchers", "example.test:80", "Authorization"))]);
        let (b, _home) = backend_with_credentials(generous_config(), Clock::system(), credentials, HashMap::new());
        let err =
            b.get(&ModuleId::new("some-other-module"), "http://example.test/a", Some("my-cred")).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn a_credential_with_no_resolvable_secret_is_unavailable() {
        let credentials =
            HashMap::from([("my-cred".to_owned(), credential("test", "example.test:80", "Authorization"))]);
        let (b, _home) = backend_with_credentials(generous_config(), Clock::system(), credentials, HashMap::new());
        let err = b.get(&ModuleId::new("test"), "http://example.test/a", Some("my-cred")).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Unavailable);
    }

    #[tokio::test]
    async fn a_request_to_a_host_the_credential_is_not_bound_to_fails_loudly_not_silently_unauthenticated() {
        // The old behaviour sent the request with no header and handed back whatever the API
        // said -- indistinguishable from a bad token, which is exactly how a mis-typed host
        // (or one written without its port) wasted an afternoon (#257's review). Nothing is
        // stubbed here: if this ever sends a request again, it fails with "no stub for", not
        // the clear binding error.
        let credentials =
            HashMap::from([("my-cred".to_owned(), credential("test", "api.example.com:443", "Authorization"))]);
        let secrets = HashMap::from([("my-cred".to_owned(), "Bearer s3cr3t".to_owned())]);
        let (b, _home) = backend_with_credentials(generous_config(), Clock::system(), credentials, secrets);

        let err = b.get(&ModuleId::new("test"), "https://other.example.com/a", Some("my-cred")).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert!(err.message.contains("api.example.com:443"), "names the bound host: {}", err.message);
        assert!(err.message.contains("other.example.com:443"), "names the asked-for host: {}", err.message);
    }

    #[tokio::test]
    async fn an_unloadable_secrets_file_is_quoted_on_the_credentialed_request_it_breaks() {
        // A broken secrets.toml no longer stops the daemon (ADR 0036 §3); the reason travels
        // to the one place it matters instead.
        let credentials =
            HashMap::from([("my-cred".to_owned(), credential("test", "api.example.com:443", "Authorization"))]);
        let home = TempDir::new().unwrap();
        let b = RealHttpBackend::new(
            home.path().to_path_buf(),
            generous_config(),
            Clock::system(),
            credentials,
            Arc::new(TestSecrets(HashMap::new())),
            Some("/home/me/.config/shimmer/secrets.toml: must be mode 0600 (found 644)".to_owned()),
        )
        .unwrap();

        let err = b.get(&ModuleId::new("test"), "https://api.example.com/a", Some("my-cred")).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Unavailable);
        assert!(err.message.contains("mode 0600"), "quotes the real reason: {}", err.message);
    }

    #[tokio::test]
    async fn a_resolved_credential_attaches_its_header_on_the_bound_host() {
        let server = TestServer::start(vec![OK]);
        let url = server.url("/a");
        let host = host_key(&reqwest::Url::parse(&url).unwrap()).unwrap();
        let credentials = HashMap::from([("my-cred".to_owned(), credential("test", &host, "Authorization"))]);
        let secrets = HashMap::from([("my-cred".to_owned(), "Bearer s3cr3t".to_owned())]);
        let (b, _home) = backend_with_credentials(generous_config(), Clock::system(), credentials, secrets);

        let resp = b.get(&ModuleId::new("test"), &url, Some("my-cred")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(server.last_request_sent("Authorization: Bearer s3cr3t"));
    }

    #[tokio::test]
    async fn a_credential_header_is_not_sent_to_a_different_host_on_redirect() {
        let b_server = TestServer::start(vec![OK]);
        let redirect = format!("HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\n\r\n", b_server.url("/b"));
        let a_server = TestServer::start(vec![Box::leak(redirect.into_boxed_str())]);
        let a_url = a_server.url("/a");
        let a_host = host_key(&reqwest::Url::parse(&a_url).unwrap()).unwrap();
        // The credential is bound to `a_server`'s host only -- `b_server` is a different host
        // (a different port counts as different, ADR 0027 §3), so the header must never reach it.
        let credentials = HashMap::from([("my-cred".to_owned(), credential("test", &a_host, "Authorization"))]);
        let secrets = HashMap::from([("my-cred".to_owned(), "Bearer s3cr3t".to_owned())]);
        let (backend, _home) = backend_with_credentials(generous_config(), Clock::system(), credentials, secrets);

        let resp = backend.get(&ModuleId::new("test"), &a_url, Some("my-cred")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(a_server.last_request_sent("Authorization: Bearer s3cr3t"), "the bound host must still get the header");
        assert!(
            !b_server.last_request_sent("Authorization"),
            "a cross-host redirect target must never see the credential header"
        );
    }

    #[tokio::test]
    async fn a_credentialed_response_is_never_served_from_or_written_to_the_cache() {
        // Two responses scripted: if either call were served from the cache, the server would
        // only ever see one request.
        let server = TestServer::start(vec![OK, OK]);
        let url = server.url("/a");
        let host = host_key(&reqwest::Url::parse(&url).unwrap()).unwrap();
        let credentials = HashMap::from([("my-cred".to_owned(), credential("test", &host, "Authorization"))]);
        let secrets = HashMap::from([("my-cred".to_owned(), "Bearer s3cr3t".to_owned())]);
        let (b, _home) = backend_with_credentials(generous_config(), Clock::system(), credentials, secrets);
        let module = ModuleId::new("test");

        let first = b.get(&module, &url, Some("my-cred")).await.unwrap();
        let second = b.get(&module, &url, Some("my-cred")).await.unwrap();
        assert!(!first.from_cache && !second.from_cache);
        assert_eq!(server.request_count(), 2, "a credentialed request must never be served from the cache");
    }

    /// A hand-rolled `tracing::Subscriber` recording each event's fields as debug-formatted
    /// text -- no new dev-dependency just to prove a secret value never reaches a log line.
    struct CapturingSubscriber {
        messages: Arc<Mutex<Vec<String>>>,
    }

    struct MessageVisitor<'a>(&'a mut String);

    impl tracing::field::Visit for MessageVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write;
            let _ = write!(self.0, " {}={:?}", field.name(), value);
        }
    }

    impl tracing::Subscriber for CapturingSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut line = String::new();
            event.record(&mut MessageVisitor(&mut line));
            self.messages.lock().unwrap().push(line);
        }
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    #[tokio::test]
    async fn the_secret_value_never_appears_in_a_log_line() {
        let server = TestServer::start(vec![OK, OK]);
        let url = server.url("/a");
        let host = host_key(&reqwest::Url::parse(&url).unwrap()).unwrap();
        let credentials = HashMap::from([("my-cred".to_owned(), credential("test", &host, "Authorization"))]);
        let secrets = HashMap::from([("my-cred".to_owned(), "s3cr3t-value".to_owned())]);
        let (b, _home) = backend_with_credentials(generous_config(), Clock::system(), credentials, secrets);

        let messages = Arc::new(Mutex::new(Vec::new()));
        let subscriber = CapturingSubscriber { messages: messages.clone() };
        let resp = {
            let _guard = tracing::subscriber::set_default(subscriber);
            // `tracing` decides once per log line whether anyone listens, and remembers it. A test
            // on another thread can reach "http: sending" first, before this listener exists, and
            // it's remembered as unheard (this failed about 1 run in 4). So: reach the line once,
            // have tracing decide again with this listener there, and only then look.
            b.get(&ModuleId::new("test"), &server.url("/warm-up"), None).await.unwrap();
            tracing::callsite::rebuild_interest_cache();
            messages.lock().unwrap().clear();
            b.get(&ModuleId::new("test"), &url, Some("my-cred")).await.unwrap()
        };

        assert_eq!(resp.status, 200);
        let joined = messages.lock().unwrap().join("\n");
        assert!(!joined.contains("s3cr3t-value"), "the secret value leaked into a log line: {joined}");
        assert!(
            joined.contains("authenticated=true"),
            "expected the debug log to record that a credential was attached, without its value: {joined}"
        );
    }

    /// Manual, opt-in proof that ADR 0036's credential passthrough works against a real
    /// authenticated API, not just the `TestServer` fixtures above. Never runs automatically
    /// (`#[ignore]`), and skips cleanly with no token at all if `GMAIL_TEST_TOKEN` isn't set.
    ///
    /// To run: get a short-lived OAuth2 access token with the `gmail.readonly` scope (e.g.
    /// from https://developers.google.com/oauthplayground -- find "Gmail API v1", check that
    /// scope, authorize, then "Exchange authorization code for tokens"), then:
    ///
    ///   GMAIL_TEST_TOKEN="ya29...." cargo test -p shimmer-daemon --lib \
    ///     a_real_gmail_api_call_succeeds_with_the_configured_credential -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "hits the real Gmail API; needs GMAIL_TEST_TOKEN (see doc comment)"]
    async fn a_real_gmail_api_call_succeeds_with_the_configured_credential() {
        let Ok(token) = std::env::var("GMAIL_TEST_TOKEN") else {
            eprintln!(
                "skipping: set GMAIL_TEST_TOKEN to a gmail.readonly OAuth2 access token to run this \
                 (see this test's doc comment for how to get one)"
            );
            return;
        };
        let host = "gmail.googleapis.com:443".to_owned();
        let credentials = HashMap::from([("gmail-test".to_owned(), credential("test", &host, "Authorization"))]);
        let secrets = HashMap::from([("gmail-test".to_owned(), format!("Bearer {token}"))]);
        let (b, _home) = backend_with_credentials(generous_config(), Clock::system(), credentials, secrets);

        let resp = b
            .get(&ModuleId::new("test"), "https://gmail.googleapis.com/gmail/v1/users/me/profile", Some("gmail-test"))
            .await
            .unwrap();

        assert_eq!(resp.status, 200, "gmail api call failed: {}", String::from_utf8_lossy(&resp.body));
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(body.get("emailAddress").is_some(), "unexpected profile response shape: {body}");
        println!("ADR 0036 credential passthrough verified live against the Gmail API for {}", body["emailAddress"]);
    }
}
