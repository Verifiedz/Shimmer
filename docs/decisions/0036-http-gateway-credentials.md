# 0036. Credentials for the HTTP gateway: a named header on `HttpBackend::get`

Status: proposed · Raised by Dev B · Needs sign-off: Dev A (changes `core`) · No protocol
change · No version bump

## Context

ADR 0027 (the real `HttpBackend`) deliberately shipped with no way for a caller to attach a
request header, and named this as its own open question: "Request headers on
`HttpBackend::get`... deferred until a real consumer defines the actual need." ADR 0028 (the
`fetchers` module, in progress on a separate branch, not yet merged) hit exactly that need and
re-flagged it in its own "Not done here": a key-gated API source (an ATS, a retailer's
developer API, anything that isn't a fully public endpoint) cannot be built at all today,
because nothing can send an `Authorization` header through `ctx.http`.

Issue #59 collected two separate, related asks once this was chased down (comments by
Verifiedz):

1. **Header passthrough** — let a `Source` (or any future network-capable module) send one
   named header, so a key-gated endpoint becomes reachable at all.
2. **Credential storage** — where the secret value itself lives, so it never ends up sitting
   in plaintext in `config.toml` (which is meant to be git-committable, CLAUDE.md §7) or in any
   other file a user might commit, share, or screen-share by accident. Scoped deliberately: tier
   1 (static tokens, e.g. an API key) in scope; tier 2 (pasted session cookies) optional; tier 3
   (automated login) explicitly out of scope.

This ADR covers both, landing ahead of its first real caller — the same posture ADR 0027 itself
took ("Nothing consumes this gateway yet"). The actual consumer is `fetchers`' `json-api`/`rss`
kinds, once that module merges; nothing here depends on `fetchers` existing, and nothing in
`fetchers` needs to change to use this once it does.

## Decision

### 1. The one `core` change: a named credential on `HttpBackend::get`

```rust
#[async_trait]
pub trait HttpBackend: Send + Sync {
    /// `credential`, when given, names a `[credentials.<name>]` entry from `config.toml`
    /// (daemon-side only — `core` never parses config) that this backend resolves and
    /// attaches as a header. The caller never sees the secret value, and never says which
    /// header or which host — both are fixed by the credential's own configuration, not by
    /// whatever URL the caller happens to pass this time (§3).
    async fn get(&self, module: &ModuleId, url: &str, credential: Option<&str>) -> Result<HttpResponse>;
}
```

No new type in `core` — `credential` is a plain name, resolved entirely daemon-side. This is
deliberately *not* "pass me arbitrary headers": a fully generic header passthrough reopens
cache-`Vary` design (ADR 0027 §3 already punted this: "no `Vary` support, since no request
headers exist on the trait to vary by") for every possible header, when the only real want is
one way to authenticate. Scoping to "one named credential" is CLAUDE.md's "general before
specific" applied narrowly on purpose: build the mechanism the actual request needs, not the
mechanism a hypothetical future request might.

`HttpGateway` (unchanged for every existing and future unauthenticated caller):

```rust
impl HttpGateway {
    pub async fn get(&self, url: &str) -> Result<HttpResponse> {
        self.backend.get(&self.module, url, None).await
    }
    pub async fn get_with_credential(&self, url: &str, credential: &str) -> Result<HttpResponse> {
        self.backend.get(&self.module, url, Some(credential)).await
    }
}
```

`RealHttpBackend`, `DisabledHttp`, `StubHttp` (the three existing implementors, all in-repo) are
the only things that must change for the signature edit — no external consumer exists.

### 2. Credential binding lives in daemon config, not in the caller's hands

A module cannot name its own header or host for a credential — only which *configured*
credential it wants, by name. The binding is `config.toml`, parsed daemon-side (no `core`
change, confirming Verifiedz's own issue note: "credentials bound to host and module in
config.toml with no core change"):

```toml
[credentials.my-api-key]
module = "fetchers"                # the only module allowed to use this credential
host = "api.example.com:443"       # the only host it is ever attached to, including a
                                   # redirect target (§4) -- host:port, same bucketing
                                   # RealHttpBackend already uses for rate limits (ADR 0027 §3)
header = "Authorization"           # the header name this credential fills in
# the secret VALUE is never here -- resolved by name through the SecretStore (§3)
```

**`host` is normalised at load, and a mismatch is loud.** It is matched against
`RealHttpBackend`'s own `host:port` bucketing, so a bare `api.example.com` would have matched
nothing and the credential would have silently never attached — the request going out
unauthenticated and coming back with an API's own `401`, indistinguishable from a bad token
(#257's review; this cost real debugging time before it was caught). Two things close that:

- A `host` with no port is normalised to `:443` at config load. An explicit port is kept as
  written, so `:80` or `:8443` still work when spelled out. A `host` that is really a URL, or
  carries a non-numeric port, is an `invalid_params` error at load.
- A credentialed request whose **first** hop's host doesn't match the binding is
  `invalid_params`, naming both hosts, rather than a request sent without the header. Only
  *subsequent* redirect hops fall back to silently dropping the header, which is the whole
  point of §4 — a cross-host redirect must degrade, not error.

`header` is checked against RFC 7230's `token` charset at load for the same reason: a typo
should be a clear startup error, not a request that fails deep inside a fetch.

`RealHttpBackend::get` rejects a `credential` name with no matching `[credentials.*]` entry, or
one whose `module` doesn't match the actual caller, as `invalid_params` — a config/call-site
mistake, not a transient condition. This means a module cannot, by constructing a different
`AuthHeader`-shaped argument, redirect a credential meant for one API to a different host or a
different header: the daemon enforces the binding, the module only ever supplies a name.

### 3. Where the secret value itself lives: `SecretStore`

```rust
// crates/daemon/src/secrets.rs — daemon-only, core never imports this
pub trait SecretStore: Send + Sync {
    /// The raw value for this credential name, if this store has it. Never logged by any
    /// caller — see §6.
    fn resolve(&self, name: &str) -> Option<String>;
}
```

Two tier-1 backends, tried in order (first `Some` wins), matching Verifiedz's own outline:

- **`FileSecretStore`** — a flat `name = "value"` TOML map in a file **outside `$SHIMMER_HOME`**
  (that directory is meant to be copyable/git-committable, CLAUDE.md §1.4/§7; a secret must
  never be able to end up inside it even by a user's innocent `cp -r`). Location follows the
  exact convention the CLI already uses for its own out-of-`$SHIMMER_HOME` config
  (`crates/cli/src/packs/settings.rs`'s `cli.toml`, ADR 0013 §3):
  `$XDG_CONFIG_HOME/shimmer/secrets.toml`, else `~/.config/shimmer/secrets.toml`. Loaded once at
  daemon startup, eagerly parsed (same "read once at startup" posture `Config::load` already
  has) — a malformed file fails the daemon's startup loudly, rather than failing silently, lazily,
  per credential, on whatever request first needs it. **On unix, a file that exists but is not
  mode `0600` fails to load** ("world- or group-readable secrets.toml" is a misconfiguration
  worth stopping for, not silently trusting) — checked once at load, not on every `resolve`.
  Windows has no equivalent POSIX permission bit; the check is `cfg(unix)`-only, a known,
  accepted gap, the same shape ADR 0010's own `setsid`/process-group handling already has.
- **`EnvSecretStore`** — `SHIMMER_SECRET_<NAME>`, name uppercased and non-alphanumeric
  characters replaced with `_` (so `my-api-key` reads `SHIMMER_SECRET_MY_API_KEY`). Useful for
  CI, ephemeral setups, or anyone who already manages secrets via their shell environment.

Chained: `FileSecretStore` first (explicit, user-managed, the one a future `shimmer secrets
set` would write to), falling back to `EnvSecretStore`. Neither backend is required to exist —
a daemon with no `secrets.toml` and no matching env var simply has no value for that name, which
`RealHttpBackend::get` reports as `unavailable`.

**The secret is the *full header value*, not the bare token.** Whatever string the store
resolves is sent verbatim as the header's value, so an `Authorization` credential's value is
`"Bearer ya29...."`, including the scheme word. This is deliberate — it is what lets the same
mechanism serve `X-Api-Key: abc123` with no special-casing — but it is also the single easiest
thing to get wrong (writing the bare token gives an opaque `401`), so it belongs in the
`secrets.toml` example and in any `shimmer secrets set` help text:

```toml
# ~/.config/shimmer/secrets.toml, mode 0600
my-api-key = "Bearer ya29...."     # the whole header value, scheme word included
```

**Both stores are read once, at daemon startup.** Editing `secrets.toml` or exporting a
`SHIMMER_SECRET_*` variable therefore needs the daemon restarted (`shimmer shutdown`, then any
command) before it takes effect — the file is parsed into memory at start, and a process's
environment is fixed at spawn. Worth stating plainly because it is surprising in exactly the
moment it bites: rotating an expired token and retrying without a restart keeps using the old
one. Two consequences: a future `shimmer secrets set` should restart or signal the daemon
itself, and once `shimmer startup` (#242) launches the daemon at login, a `SHIMMER_SECRET_*`
exported in an interactive shell will not reach it at all — that path needs the file store.

**A `secrets.toml` that exists but cannot be loaded is never fatal.** It is logged, remembered,
and quoted on any credentialed request that then finds no value (`credential 'x' is
unavailable: ...: must be mode 0600 (found 644)`). Failing the daemon's whole startup over it —
taking `records`, `workspaces` and `notify` down for an optional network feature — is wildly
disproportionate (#257's review); failing loudly at the one request that actually needed the
secret puts the error where the person can act on it.

### 4. Attached per-hop, never cross-host — collapses two rules into one

ADR 0027's `fetch()` already loops over redirect hops, each resolved through that hop's own
host's rate limiter (§3's "a cross-host redirect goes through the new host's limiter"). The
credential header is attached on a hop **only when that hop's `host_key` exactly equals the
binding's configured `host`** — checked fresh on every hop, not just the first. This is not a
special "strip auth on redirect" rule bolted on afterward: it falls out for free from "a
credential's binding names the one host it may ever be sent to," the same way the rate limiter
already re-keys per hop. A same-host redirect (rare, but not impossible) keeps sending the
header; a cross-host redirect silently stops sending it, exactly the behavior a credential
leak protection needs.

### 5. Credentialed responses are never cached

`RealHttpBackend::get` skips the on-disk response cache entirely — both read and write — whenever
`credential.is_some()`. Simpler and more conservative than extending `ResponseCache`'s key or
`CacheMeta`'s shape to account for which credential produced a given cached body (what ADR 0027
called "no `Vary` support" for headers in general): a credential can be rotated or revoked at
any time, and a cached body from before that rotation must never outlive it. The cost is that a
key-gated source always pays a real network round trip; given §7's existing per-run item caps
and rate limiting already bound how often a `fetchers` tick calls out, this is not a meaningful
regression for the shape of module this is built for.

### 6. Redaction

No log statement in `RealHttpBackend` ever interpolates a resolved secret value — only the
credential's *name* (already non-secret, since it's a key in `config.toml`) appears in any
`tracing::*!` call. Tested directly: a test captures `tracing`'s output around a credentialed
request and asserts the literal secret value never appears in it, the same spirit as ADR 0027
§9's existing test list but new to this change since nothing before this handled secret
material at all.

### 7. Capability scoping: unchanged

Credentials are a sub-feature of `ctx.http`, which is already gated on the `"network"`
capability (ADR 0027 §4). No new capability string, no `Manifest` change — a module that can
reach the network at all can ask for a named credential; whether that credential actually exists
and is bound to that module is `config.toml`'s decision (§2), checked per-call.

## Not done here

- **The CLI surface** (`shimmer secrets set <name>`, prompting on stdin so the value never
  appears in shell history or `ps`, writing into `FileSecretStore`'s file with `0600`
  permissions) — no module needs it yet (`fetchers` hasn't merged), and building it now would
  mean designing against an imagined caller instead of `fetchers`' actual `SourceConfig` shape.
  Lands with whichever module first needs it, the same way ADR 0027's own gateway landed before
  `fetchers` used it for real.
- **Tier 2 (pasted session cookies)** — out of scope, per the issue's own scoping. Nothing here
  blocks it; a cookie is just a different header name on the same `credential` mechanism, should
  it ever be wanted.
- **Tier 3 (automated login)** — explicitly out of scope, per the issue. **This is a real limit
  on the first consumer, not a theoretical one:** the Gmail API (the source this mechanism was
  built and live-tested against) issues OAuth2 access tokens that expire in about an hour, and
  refreshing one is squarely the "automated login" tier ruled out here. So a Gmail source on
  this mechanism is a *manual-use* source — fine for `fetchers test gmail` and for a run you
  are sitting in front of, useless as an unattended daily trigger, since the token will be dead
  long before the next firing. Any mail source that wants to be unattended needs a refresh-token
  flow, which needs its own ADR and probably its own `SecretStore` backend (one that can
  *write* a rotated token back). Static, non-expiring keys — an ATS board's API key, a
  retailer's developer key — are the shapes this tier actually serves well.
- **A distinct "credential expired" failure** (e.g. a `401`/`403` on a credentialed request
  reported differently from an ordinary `http_error`) — this is a *caller's* interpretation of an
  ordinary in-band status code, exactly like every other 4xx `RealHttpBackend` already passes
  through uninterpreted (ADR 0027 §3, ADR 0028 §6). Nothing here prevents a future `fetchers`
  from special-casing a credentialed request's `401` as its own `reason` in
  `fetchers.fetch.failed` — that decision belongs to that module, not this layer.
- **OS keychain backend** — the user's own outline named it "later," not for this tier-1 pass.
  `SecretStore` is a plain trait specifically so a third backend is additive whenever that is
  built.
- **Windows permission checking** on `secrets.toml` — no POSIX mode bit exists to check; left as
  a known gap (§3).
