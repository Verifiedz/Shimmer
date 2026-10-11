# 0027. The real HTTP gateway: `RealHttpBackend`

Status: accepted (M6, #109) · Raised by Dev A · Signed off: Dev A, Dev B (CLAUDE.md §4, changes
`core`) · No protocol change

## Context

`Ctx.http` (`crates/core/src/ctx.rs:58`) and the `HttpBackend` trait
(`crates/core/src/http.rs:17-21`) already exist, mirroring `LaunchBackend`/`Launcher`
(ADR 0010) exactly — a thin capability trait in `core`, a real implementation meant to live in
the daemon. But every module's `ctx.http` is wired to `DisabledHttp`
(`crates/daemon/src/backend.rs:53-62`), which fails every call closed. Until a real backend
exists, nothing can reach the network — including the eventual `fetchers` module (§8, M6),
which is Dev B's territory (`modules/*`), not this ADR's.

This ADR covers exactly the Dev A slice: the real daemon-side `HttpBackend`, landing ahead of
its first real caller, the same way `RealLaunchBackend` landed before the workspaces module
used it for real. Nothing consumes this gateway yet.

## Decision — Dev A's side

### 1. The capability

The trait and `Ctx` field are not new — only the real implementation is. `RealHttpBackend`
(`crates/daemon`) implements `HttpBackend`, replacing `DisabledHttp` for any module that opts
in (§4 below).

### 2. Interface — the one `core` change

`HttpResponse` (`crates/core/src/http.rs:11-15`) today carries only `status: u16` and
`body: Vec<u8>` — no way to tell a caller a response was cached, served stale, or
rate-limited. It gains three fields:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub from_cache: bool,
    pub stale: bool,
    pub retry_after_secs: Option<u64>,
}
```

Nothing else about the trait changes. `HttpBackend::get(&self, module: &ModuleId, url: &str)`
keeps its signature — see §3's "What doesn't need a bigger interface" for why cancellation,
revalidation and redirects don't need one either, and why request headers (Authorization,
cookies, anything custom) deliberately stay out of scope (§6, "Not done here").

The module doc at `crates/core/src/http.rs:1-2` currently says "Rate limiting, caching and
backoff live behind the backend" — backoff does not; retry is always the caller's decision
(§11.2, and see §3 below), same principle this ADR applies to the gateway. Reworded in the
same change.

### 3. Behaviour

**Rate limiting.** A token bucket per `host:port` (not bare host — a different port is a
different budget, even on the same host; in practice almost always 443, this is just precise
about what "per-host" means, CLAUDE.md §6.1). A `tokio::sync::Semaphore` per host caps
concurrency. State is in-memory, resets on daemon restart — a courtesy to remote hosts, not a
durability concern, same posture the queue/scheduler take toward state that doesn't need to
survive a restart. A `429`/`503` records a cooldown for that host from its `Retry-After`
header (read internally, never itself surfaced as a header — there is no header type on this
trait at all). A later request to a cooling-down host waits, bounded by a configured cap
(`max_cooldown_wait_s`); past that cap it returns the last-known `429`/`503` immediately, with
`retry_after_secs` set to what remains, rather than blocking indefinitely or hammering the
host. **The gateway never retries** — exactly one response or one `Err` per `get` call,
mirroring §11.2: "the queue does not retry; retry is the module's decision," made with
`ctx.retry_with_backoff` (`crates/core/src/ctx.rs:123-147`) around the gateway call, not inside
it.

**Cache.** `200` responses only. Keyed on module id + the full URL string (verbatim, including
the query string) — no `Vary` support, since no request headers exist on the trait to vary by.
Two files per entry — `<hash>.meta.json` and `<hash>.body` — both written through the existing
`write_atomic` (`crates/store/src/atomic.rs:12-28`, already generic enough for this: it already
writes `$SHIMMER_HOME/.gitignore` and the daemon's own `config.toml`, no namespace plumbing
needed). A crash between the two writes leaves an incomplete pair, read back as a miss — fine,
since `cache/http/` is explicitly "safe to delete" (CLAUDE.md §7) and already a reserved,
gitignored name (`crates/store/src/lib.rs:28-36`). Freshness: `Cache-Control: max-age` if
present, else a configured default TTL. Past that, `RealHttpBackend` revalidates on its own
initiative with `If-None-Match`/`If-Modified-Since` built from its own cache metadata — see
below for why this needs no interface change. `stale-if-error`: a failed refetch or
revalidation with a cached copy available serves that copy with `stale: true` set, visible to
the caller. Size cap with oldest-first eviction, checked synchronously after each write. Max
response size enforced while streaming the body, aborting once the running total exceeds the
cap — never buffers an unbounded body even if a server lies about `Content-Length`. Cache
entries live under `cache/http/<module>/` — one module's cache is never visible to another's
lookup for the identical URL. A cache write failure is never fatal to the request it was
caching for — same best-effort posture as step-log pruning (#74/#105,
`crates/daemon/src/launcher.rs:216-221`).

**Scheme, redirects, timeout.** Only `http`/`https`; anything else is rejected
(`invalid_params`) before any network, cache or rate-limit work happens. Redirects: the
client's automatic following is disabled, and `RealHttpBackend` loops manually, capped at 10
hops, sending each hop's target host through that host's own limiter before the request goes
out — satisfies "a cross-host redirect goes through the new host's limiter" literally, which
automatic following could not. Default timeout from config, set once on the shared client.

**What doesn't need a bigger interface.** Three things looked like they might, and don't:

- *Cancellation* — `HttpBackend::get` has no `cancel` parameter (`LaunchBackend::run` does).
  Standard Rust future cancellation already covers it: a module races the call against
  `ctx.cancel` itself (`tokio::select! { res = ctx.http.get(url) => .., _ = ctx.cancel.cancelled() => .. }`),
  the same shape `retry_with_backoff` already uses internally. `RealHttpBackend`'s internal
  rate-limit wait is a plain `.await`, cancel-safe by construction; it never polls `ctx.cancel`
  itself.
- *Revalidation* — needs an outbound `If-None-Match`/`If-Modified-Since` header, but
  `RealHttpBackend` builds that request itself, from its own cache metadata, on its own
  initiative. The caller never needs to supply it.
- *Redirects* — entirely internal to `RealHttpBackend`'s own loop, as above.

### 4. Capability scoping

`crates/daemon/src/core.rs:94` builds every module's `ctx.http` unconditionally today — no
gate at all, unlike the launcher's (`core.rs:103-106`, gated on `"process"` in
`Manifest.capabilities`). This adds the same gate for `"network"`, immediately after the
launcher's:

```rust
if e.manifest.capabilities.iter().any(|c| c == "network") {
    ctx.http = HttpGateway::new(id.clone(), real_http.clone());
}
```

Not `#[cfg(unix)]` — HTTP is platform-uniform, unlike process spawning. `Manifest.capabilities`
is already free-form `Vec<String>` (`crates/core/src/manifest.rs:16-18`: "declared, not yet
enforced"), so the string itself needs no `core` change. Every module without `"network"`
declared keeps `DisabledHttp`, unchanged from today. Checked directly: `records`
(`crates/modules/records/src/lib.rs:82`) declares `capabilities: vec![]` and never touches
`ctx.http` anywhere — this change is a no-op for every module that exists today.

### 5. Storage

`cache/http/<module>/`, under the already-reserved, gitignored `cache/` (CLAUDE.md §7,
`crates/store/src/lib.rs:28-36`). No move of anything that exists today — this directory has
never been written to.

### 6. Testing fakes

None needed. `StubHttp` (`crates/core/src/testing.rs:94-105`) already exists, is wired into
every module test via `TestEnv`, and is sufficient for module-level tests as-is — canned
exact-URL responses, no real networking, no caching or rate-limiting to simulate since modules
never see that layer. Unlike ADR 0010 (which added `FakeLauncher`), this ADR adds no new fake.

### 7. Platforms

Cross-platform, not `cfg(unix)`-gated. Client: `reqwest`, `default-features = false`, features
`["rustls-tls-webpki-roots"]` — pure-Rust TLS via a bundled Mozilla root store, no system TLS
library (OpenSSL or otherwise) on either CI leg. No HTTP or TLS crate exists anywhere in the
workspace today, so this is a new dependency, not a swap. Known tradeoff: a corporate MITM
proxy's injected CA is not trusted, since the bundled store is fixed rather than reading the
OS's. Accepted for now; revisit if it ever matters.

### 8. Atomicity and failure

Cache files go through `write_atomic` (§3). A cache write failure never fails the request it
was caching for — logged and swallowed, same as every other best-effort housekeeping path in
this codebase. The gateway itself never retries (§3) — symmetric with §11.2's "the queue does
not retry."

### 9. Tests Dev A will write

Rate limiter: bucket refill and per-host isolation, concurrency cap, a `429`'s cooldown
recorded and honored on the next call, the bounded wait is actually bounded — all driven by
`Clock::fake`/`Clock::advance` (`crates/core/src/clock.rs:23,35`), never a real sleep. Cache:
round-trip, TTL expiry triggers a refetch, a cached `ETag` produces an `If-None-Match` on the
next request (asserted against what a local test server actually received), size-cap eviction
removes the oldest first, an oversized response aborts without buffering it all, two different
module ids never see each other's cache for the identical URL. Redirects: a local server
redirects to a second local server on a different port, and that second "host" went through
its own limiter. Scheme rejection. Capability gating: a module declaring `"network"` gets a
real gateway, one that doesn't keeps `DisabledHttp` — mirrors
`crates/daemon/tests/launcher_wiring.rs`'s
`without_the_capability_ctx_launcher_stays_the_default_unavailable_stub`. The local test server
itself is hand-rolled against `tokio::net::TcpListener::bind("127.0.0.1:0")` — no new
dev-dependency, matching how `crates/mockd` hand-rolls its own Unix-socket test harness rather
than pulling in a framework. No test reaches a real network.

## Dev B's side

When `fetchers` (§8, M6) is built: declare `"network"` in its manifest; build `Source`,
scheduling and dedup on top (the host module's own job per §8's table — this ADR's gateway
owns only rate limiting and caching, nothing else). If a `Source` turns out to need request
headers (an API key, say), that is a new interface question for whoever builds it to raise as
its own proposal — not decided here.

## Not done here

- Request headers on `HttpBackend::get` — Authorization, cookies, anything custom. The trait
  has no way for a caller to supply any today, so the requirement that an authenticated
  request is never cached without the module opting in is vacuously true for now: nothing
  authenticated can be requested through this gateway yet. Deferred until a real consumer
  defines the actual need — the same reasoning CLAUDE.md §5 gives for deferring `ViewSpec`
  until there is a renderer to extract it from.
- The `fetchers` module itself — Dev B's, not started here.
