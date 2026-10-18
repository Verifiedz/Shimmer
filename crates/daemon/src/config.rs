//! `config.toml`: lane overrides, per-module sections, and general settings. Read once at
//! startup; a missing file means defaults.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use shimmer_core::{Error, LocalTimezone, ModuleId, Result};

use crate::http_backend::{CredentialBinding, HostLimits, HttpConfig};

#[derive(Debug, Default)]
pub struct Config {
    /// `[lanes] fetchers = 4`
    pub lanes: HashMap<String, usize>,
    /// `[modules.records] ...`, handed to that module as its `ModuleConfig`.
    pub modules: HashMap<String, Value>,
    /// `[general] local_timezone = "America/New_York"` (ADR 0009). Defaults to UTC.
    pub local_timezone: LocalTimezone,
    /// `[http]` (ADR 0027): the real gateway's rate limits, cache and timeout.
    pub http: HttpConfig,
    /// `[credentials.<name>]` (ADR 0036 §2): which module and host a named credential is
    /// bound to. The secret value itself never lives here -- resolved separately, by name,
    /// through a `SecretStore` (`crates/daemon/src/secrets.rs`).
    pub credentials: HashMap<String, CredentialBinding>,
    /// `[desktop] show = false` (ADR 0032 §2): log desktop notes instead of showing them, for
    /// machines with no screen and for tests.
    pub hide_desktop: bool,
}

impl Config {
    /// An explicit `[general] local_timezone` in `config.toml` wins. Otherwise this
    /// best-effort detects the system zone and writes it back to `config.toml` — so it is
    /// resolved once, inspectable, and hand-editable afterward, not silently re-detected on
    /// every start — falling back to UTC with a logged warning if detection fails.
    pub fn load(home: &Path) -> Result<Self> {
        let path = home.join("config.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        let mut cfg = Self::parse(&text)?;
        let explicit = toml::from_str::<toml::Table>(&text)
            .ok()
            .and_then(|root| root.get("general")?.get("local_timezone").cloned())
            .is_some();
        if !explicit {
            match detect_system_timezone() {
                Some((tz, name)) => {
                    cfg.local_timezone = tz;
                    if let Err(e) = persist_detected_timezone(&path, &text, &name) {
                        tracing::warn!(error = %e, path = %path.display(), "could not save detected local_timezone to config.toml");
                    }
                }
                None => {
                    // Do not persist "UTC" here: that would freeze it into config.toml as if
                    // it were the user's explicit choice, and it would never be re-detected
                    // even after whatever made detection fail (a bad TZ value, a missing
                    // /etc/localtime) is fixed. `cfg.local_timezone` already defaults to UTC.
                    tracing::warn!(
                        "could not detect the system timezone; local_timezone defaults to UTC \
                         (set [general] local_timezone in config.toml to override)"
                    );
                }
            }
        }
        Ok(cfg)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let root: toml::Table = toml::from_str(text).map_err(|e| Error::invalid_params(format!("config.toml: {e}")))?;
        let mut cfg = Self::default();
        if let Some(lanes) = root.get("lanes") {
            let table =
                lanes.as_table().ok_or_else(|| Error::invalid_params("config.toml: [lanes] must be a table"))?;
            for (name, v) in table {
                let n = v.as_integer().filter(|n| *n >= 1).ok_or_else(|| {
                    Error::invalid_params(format!("config.toml: lanes.{name} must be an integer >= 1"))
                })?;
                cfg.lanes.insert(name.clone(), n as usize);
            }
        }
        if let Some(modules) = root.get("modules").and_then(|m| m.as_table()) {
            for (name, section) in modules {
                let json = serde_json::to_value(section)
                    .map_err(|e| Error::invalid_params(format!("config.toml: modules.{name}: {e}")))?;
                cfg.modules.insert(name.clone(), json);
            }
        }
        if let Some(name) = root.get("general").and_then(|g| g.get("local_timezone")).and_then(|v| v.as_str()) {
            cfg.local_timezone = LocalTimezone::parse(name).map_err(|_| {
                Error::invalid_params(format!("config.toml: general.local_timezone: unknown IANA timezone '{name}'"))
            })?;
        }
        if let Some(http) = root.get("http") {
            cfg.http = parse_http(http)?;
        }
        if let Some(credentials) = root.get("credentials").and_then(|c| c.as_table()) {
            for (name, entry) in credentials {
                cfg.credentials.insert(name.clone(), parse_credential(name, entry)?);
            }
        }
        if let Some(desktop) = root.get("desktop") {
            let table =
                desktop.as_table().ok_or_else(|| Error::invalid_params("config.toml: [desktop] must be a table"))?;
            for (key, v) in table {
                match (key.as_str(), v.as_bool()) {
                    ("show", Some(show)) => cfg.hide_desktop = !show,
                    ("show", None) => {
                        return Err(Error::invalid_params("config.toml: desktop.show must be true or false"))
                    }
                    (other, _) => {
                        return Err(Error::invalid_params(format!(
                            "config.toml: desktop.{other} isn't a setting (only show)"
                        )))
                    }
                }
            }
        }
        Ok(cfg)
    }
}

/// `[credentials.<name>]` (ADR 0036 §2): `module`, `host` and `header` are all required --
/// a credential with no module to restrict it to, or no host to restrict it to, would be
/// usable anywhere by anything, which defeats the entire point of binding it.
fn parse_credential(name: &str, entry: &toml::Value) -> Result<CredentialBinding> {
    let table = entry
        .as_table()
        .ok_or_else(|| Error::invalid_params(format!("config.toml: credentials.{name} must be a table")))?;
    let field =
        |key: &str| {
            table.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty()).ok_or_else(|| {
                Error::invalid_params(format!("config.toml: credentials.{name} needs a non-empty {key}"))
            })
        };
    let header = field("header")?;
    if !is_header_name(header) {
        return Err(Error::invalid_params(format!(
            "config.toml: credentials.{name}: header '{header}' is not a valid header name"
        )));
    }
    Ok(CredentialBinding {
        module: ModuleId::new(field("module")?),
        host: normalized_host(name, field("host")?)?,
        header: header.to_owned(),
    })
}

/// A binding's `host` is matched against `RealHttpBackend`'s own `host:port` bucketing, so a
/// bare `api.example.com` would never match anything and the credential would silently never
/// attach (#257's review). Normalise it here instead: a missing port becomes `:443`, since a
/// credentialed API over plain `http` is not a thing we want to make convenient by default.
/// An explicit port is kept as written, so `:80` or `:8443` still work when spelled out.
fn normalized_host(name: &str, host: &str) -> Result<String> {
    let bad = |why: &str| Error::invalid_params(format!("config.toml: credentials.{name}: host '{host}' {why}"));
    if host.contains('/') {
        return Err(bad("must be a host, optionally with a port -- not a url"));
    }
    match host.rsplit_once(':') {
        Some((h, port)) => {
            if h.is_empty() {
                return Err(bad("has no host before its port"));
            }
            port.parse::<u16>().map(|_| host.to_owned()).map_err(|_| bad("has a port that is not a number"))
        }
        None => Ok(format!("{host}:443")),
    }
}

/// RFC 7230's `token` charset, which is what `reqwest` will accept as a header name anyway --
/// checked here so a typo is a clear config error at startup rather than a request that fails
/// deep inside a fetch.
fn is_header_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c))
}

fn number(v: &toml::Value) -> Option<f64> {
    v.as_float().or_else(|| v.as_integer().map(|n| n as f64))
}

/// `[http]` (ADR 0027). Every key is optional and falls back to `HttpConfig::default()`'s
/// value; `[http.host.<name>]` entries need both fields together, so a host override is
/// never half-specified.
fn parse_http(http: &toml::Value) -> Result<HttpConfig> {
    let table = http.as_table().ok_or_else(|| Error::invalid_params("config.toml: [http] must be a table"))?;
    let mut cfg = HttpConfig::default();
    let positive = |key: &str, v: &toml::Value| {
        number(v)
            .filter(|n| *n > 0.0)
            .ok_or_else(|| Error::invalid_params(format!("config.toml: http.{key} must be a positive number")))
    };
    let non_negative_secs = |key: &str, v: &toml::Value| {
        v.as_integer()
            .filter(|n| *n >= 0)
            .map(|n| Duration::from_secs(n as u64))
            .ok_or_else(|| Error::invalid_params(format!("config.toml: http.{key} must be an integer >= 0")))
    };
    if let Some(v) = table.get("default_requests_per_sec") {
        cfg.default_requests_per_sec = positive("default_requests_per_sec", v)?;
    }
    if let Some(v) = table.get("default_concurrent") {
        cfg.default_concurrent = v
            .as_integer()
            .filter(|n| *n >= 1)
            .ok_or_else(|| Error::invalid_params("config.toml: http.default_concurrent must be an integer >= 1"))?
            as usize;
    }
    if let Some(v) = table.get("timeout_s") {
        cfg.timeout = non_negative_secs("timeout_s", v)?;
    }
    if let Some(v) = table.get("max_cooldown_wait_s") {
        cfg.max_cooldown_wait = non_negative_secs("max_cooldown_wait_s", v)?;
    }
    if let Some(v) = table.get("cache_default_ttl_s") {
        cfg.cache_default_ttl = non_negative_secs("cache_default_ttl_s", v)?;
    }
    if let Some(v) = table.get("cache_max_bytes") {
        cfg.cache_max_bytes = v
            .as_integer()
            .filter(|n| *n >= 0)
            .ok_or_else(|| Error::invalid_params("config.toml: http.cache_max_bytes must be an integer >= 0"))?
            as u64;
    }
    if let Some(v) = table.get("max_response_bytes") {
        cfg.max_response_bytes = v
            .as_integer()
            .filter(|n| *n >= 0)
            .ok_or_else(|| Error::invalid_params("config.toml: http.max_response_bytes must be an integer >= 0"))?
            as u64;
    }
    if let Some(hosts) = table.get("host").and_then(|h| h.as_table()) {
        for (host, overrides) in hosts {
            let o = overrides
                .as_table()
                .ok_or_else(|| Error::invalid_params(format!("config.toml: http.host.{host} must be a table")))?;
            let requests_per_sec = o
                .get("requests_per_sec")
                .ok_or_else(|| Error::invalid_params(format!("config.toml: http.host.{host} needs requests_per_sec")))
                .and_then(|v| positive(&format!("host.{host}.requests_per_sec"), v))?;
            let concurrent = o.get("concurrent").and_then(|v| v.as_integer()).filter(|n| *n >= 1).ok_or_else(|| {
                Error::invalid_params(format!("config.toml: http.host.{host}.concurrent must be an integer >= 1"))
            })? as usize;
            cfg.per_host.insert(host.clone(), HostLimits { requests_per_sec, concurrent });
        }
    }
    Ok(cfg)
}

/// `TZ`, else the `/etc/localtime` symlink target (Linux/macOS), else `None` (caller decides
/// how to fall back). `TZ` is tried first but, if it does not parse as an IANA name (a POSIX
/// TZ spec with an explicit DST rule, like `"EST5EDT4,M3.2.0,M11.1.0"`), we still fall through
/// to `/etc/localtime` rather than giving up immediately — an odd `TZ` value should not shadow
/// a perfectly good symlink.
fn detect_system_timezone() -> Option<(LocalTimezone, String)> {
    let tz_env = std::env::var("TZ").ok();
    let localtime = localtime_symlink_name();
    detect_from_candidates(tz_env.into_iter().chain(localtime))
}

fn localtime_symlink_name() -> Option<String> {
    std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|target| target.to_str()?.split("zoneinfo/").nth(1).map(str::to_string))
}

/// Tries each candidate name in order, returning the first that parses as an IANA zone. A
/// leading `:` (glibc's "this is a filename/zone name, not a POSIX spec" `TZ` convention, as
/// in `":America/New_York"`) is stripped before parsing. Split out from
/// [`detect_system_timezone`] so the fallback-order and colon-stripping logic is unit
/// testable without touching the real environment or `/etc/localtime`.
fn detect_from_candidates(candidates: impl Iterator<Item = String>) -> Option<(LocalTimezone, String)> {
    candidates
        .filter_map(|raw| {
            let name = raw.strip_prefix(':').unwrap_or(&raw);
            LocalTimezone::parse(name).ok().map(|tz| (tz, name.to_string()))
        })
        .next()
}

/// Adds `local_timezone = "<name>"` under an existing `[general]` table, or appends a new
/// one, leaving the rest of the file untouched (no reformatting, no comment loss).
fn persist_detected_timezone(path: &Path, existing: &str, name: &str) -> Result<()> {
    let addition = format!("local_timezone = \"{name}\"\n");
    let new_text = if let Some(insert_at) = general_table_header_end(existing) {
        // The header line may be the file's last line with no trailing `\n` (so `insert_at`
        // lands right after the `]`, not on a fresh line) — in that case the insertion needs
        // its own leading `\n`, or the key glues onto the header and produces invalid TOML.
        let addition = if existing[..insert_at].ends_with('\n') { addition } else { format!("\n{addition}") };
        let mut s = existing.to_string();
        s.insert_str(insert_at, &addition);
        s
    } else if existing.trim().is_empty() {
        format!("[general]\n{addition}")
    } else {
        format!("{existing}\n[general]\n{addition}")
    };
    shimmer_store::write_atomic(path, new_text.as_bytes()).map_err(Error::from)
}

/// Byte offset right after the `[general]` table header line, or `None` if the file has no
/// such table. Matches a line whose content — ignoring surrounding whitespace, a trailing
/// `# comment`, and whitespace just inside the brackets (`[ general ]` is the same table as
/// `[general]` in TOML, #98) — is exactly a `[general]` header, not merely a line that
/// mentions the text `"[general]"` somewhere (e.g. inside a `#`-comment or a string value),
/// which a plain substring search would wrongly match.
fn general_table_header_end(existing: &str) -> Option<usize> {
    let mut offset = 0;
    for line in existing.split_inclusive('\n') {
        offset += line.len();
        let without_comment = line.split('#').next().unwrap_or("").trim();
        let inner = without_comment.strip_prefix('[').and_then(|s| s.strip_suffix(']'));
        if inner.map(str::trim) == Some("general") {
            return Some(offset);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_notes_can_be_logged_instead_of_shown() {
        assert!(!Config::parse("").unwrap().hide_desktop, "shown by default");
        assert!(Config::parse("[desktop]\nshow = false\n").unwrap().hide_desktop);
        assert!(Config::parse("[desktop]\nshow = \"no\"\n").unwrap_err().message.contains("true or false"));
        assert!(Config::parse("[desktop]\nsound = true\n").unwrap_err().message.contains("only show"));
    }

    #[test]
    fn parses_lanes_and_module_sections() {
        let cfg = Config::parse("[lanes]\nfetchers = 2\n[modules.records]\npage_size = 50\n").unwrap();
        assert_eq!(cfg.lanes["fetchers"], 2);
        assert_eq!(cfg.modules["records"]["page_size"], 50);
    }

    #[test]
    fn rejects_zero_concurrency() {
        assert!(Config::parse("[lanes]\nfetchers = 0\n").is_err());
    }

    #[test]
    fn parses_an_explicit_local_timezone() {
        let cfg = Config::parse("[general]\nlocal_timezone = \"America/New_York\"\n").unwrap();
        assert_eq!(cfg.local_timezone.name(), "America/New_York");
    }

    #[test]
    fn defaults_local_timezone_to_utc() {
        assert_eq!(Config::parse("").unwrap().local_timezone, LocalTimezone::UTC);
    }

    #[test]
    fn parses_the_http_section_including_a_per_host_override() {
        let text = "[http]\ndefault_requests_per_sec = 5\ndefault_concurrent = 3\ntimeout_s = 10\n\
                     max_cooldown_wait_s = 15\ncache_default_ttl_s = 120\ncache_max_bytes = 1000\n\
                     max_response_bytes = 2000\n\n[http.host.\"jobs.example.com\"]\nrequests_per_sec = 1\nconcurrent = 1\n";
        let cfg = Config::parse(text).unwrap();
        assert_eq!(cfg.http.default_requests_per_sec, 5.0);
        assert_eq!(cfg.http.default_concurrent, 3);
        assert_eq!(cfg.http.timeout, std::time::Duration::from_secs(10));
        assert_eq!(cfg.http.max_cooldown_wait, std::time::Duration::from_secs(15));
        assert_eq!(cfg.http.cache_default_ttl, std::time::Duration::from_secs(120));
        assert_eq!(cfg.http.cache_max_bytes, 1000);
        assert_eq!(cfg.http.max_response_bytes, 2000);
        let host = cfg.http.per_host.get("jobs.example.com").unwrap();
        assert_eq!((host.requests_per_sec, host.concurrent), (1.0, 1));
    }

    #[test]
    fn missing_http_section_keeps_every_default() {
        let cfg = Config::parse("").unwrap();
        let defaults = HttpConfig::default();
        assert_eq!(cfg.http.default_requests_per_sec, defaults.default_requests_per_sec);
        assert_eq!(cfg.http.default_concurrent, defaults.default_concurrent);
        assert!(cfg.http.per_host.is_empty());
    }

    #[test]
    fn a_host_override_needs_both_fields_together() {
        let text = "[http.host.\"jobs.example.com\"]\nrequests_per_sec = 1\n";
        let e = Config::parse(text).unwrap_err();
        assert!(e.message.contains("concurrent"), "{}", e.message);
    }

    #[test]
    fn rejects_a_non_positive_rate() {
        assert!(Config::parse("[http]\ndefault_requests_per_sec = 0\n").is_err());
    }

    #[test]
    fn parses_a_credentials_section() {
        let text = "[credentials.my-api-key]\nmodule = \"fetchers\"\nhost = \"api.example.com:443\"\nheader = \"Authorization\"\n";
        let cfg = Config::parse(text).unwrap();
        let cred = &cfg.credentials["my-api-key"];
        assert_eq!(cred.module, ModuleId::new("fetchers"));
        assert_eq!(cred.host, "api.example.com:443");
        assert_eq!(cred.header, "Authorization");
    }

    #[test]
    fn a_credential_host_without_a_port_is_normalised_rather_than_silently_never_matching() {
        // `host` is matched against RealHttpBackend's own `host:port` bucketing, so a bare host
        // used to match nothing at all and the credential silently never attached (#257 review).
        let text = "[credentials.k]\nmodule = \"fetchers\"\nhost = \"api.example.com\"\nheader = \"Authorization\"\n";
        assert_eq!(Config::parse(text).unwrap().credentials["k"].host, "api.example.com:443");
    }

    #[test]
    fn an_explicit_credential_port_is_kept_as_written() {
        for (written, expected) in
            [("api.example.com:80", "api.example.com:80"), ("api.example.com:8443", "api.example.com:8443")]
        {
            let text = format!("[credentials.k]\nmodule = \"f\"\nhost = \"{written}\"\nheader = \"Authorization\"\n");
            assert_eq!(Config::parse(&text).unwrap().credentials["k"].host, expected);
        }
    }

    #[test]
    fn a_credential_host_that_is_a_url_or_has_a_bad_port_is_rejected() {
        for host in ["https://api.example.com", "api.example.com/v1", "api.example.com:https", "api.example.com:"] {
            let text = format!("[credentials.k]\nmodule = \"f\"\nhost = \"{host}\"\nheader = \"Authorization\"\n");
            let e = Config::parse(&text).unwrap_err();
            assert_eq!(e.code, shimmer_core::ErrorCode::InvalidParams, "host: {host}");
            assert!(e.message.contains("host"), "host {host}: {}", e.message);
        }
    }

    #[test]
    fn a_credential_header_that_is_not_a_header_name_is_rejected_at_load() {
        for header in ["Authorization: Bearer", "X Api Key", "has\nnewline"] {
            let text = format!(
                "[credentials.k]\nmodule = \"f\"\nhost = \"a.test:443\"\nheader = \"{}\"\n",
                header.escape_default()
            );
            let e = Config::parse(&text).unwrap_err();
            assert_eq!(e.code, shimmer_core::ErrorCode::InvalidParams, "header: {header}");
        }
        // The shapes a real API actually uses all pass.
        for header in ["Authorization", "X-Api-Key", "x-goog-api-key"] {
            let text = format!("[credentials.k]\nmodule = \"f\"\nhost = \"a.test:443\"\nheader = \"{header}\"\n");
            Config::parse(&text).unwrap_or_else(|e| panic!("header {header} should be valid: {}", e.message));
        }
    }

    #[test]
    fn a_credential_missing_a_required_field_is_rejected() {
        let e =
            Config::parse("[credentials.my-api-key]\nmodule = \"fetchers\"\nheader = \"Authorization\"\n").unwrap_err();
        assert!(e.message.contains("host"), "{}", e.message);
    }

    #[test]
    fn rejects_an_unknown_local_timezone() {
        let e = Config::parse("[general]\nlocal_timezone = \"Not/AZone\"\n").unwrap_err();
        assert!(e.message.contains("Not/AZone"));
    }

    #[test]
    fn persist_appends_a_general_table_when_absent() {
        let text = "[lanes]\nfetchers = 2\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        persist_detected_timezone(&path, text, "America/New_York").unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let cfg = Config::parse(&written).unwrap();
        assert_eq!(cfg.lanes["fetchers"], 2, "existing content must survive");
        assert_eq!(cfg.local_timezone.name(), "America/New_York");
    }

    #[test]
    fn persist_inserts_into_an_existing_general_table() {
        let text = "[general]\nsome_other_key = 1\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        persist_detected_timezone(&path, text, "UTC").unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("some_other_key = 1"));
        assert_eq!(Config::parse(&written).unwrap().local_timezone, LocalTimezone::UTC);
    }

    #[test]
    fn persist_ignores_a_comment_line_that_merely_mentions_general() {
        // A naive substring search for "[general]" would match inside this comment and
        // insert there instead of appending a real table.
        let text = "# see [general] section below\n[lanes]\nfetchers = 2\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        persist_detected_timezone(&path, text, "UTC").unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("# see [general] section below\n[lanes]\nfetchers = 2\n"));
        let cfg = Config::parse(&written).unwrap();
        assert_eq!(cfg.lanes["fetchers"], 2, "existing content must survive");
        assert_eq!(cfg.local_timezone, LocalTimezone::UTC);
    }

    #[test]
    fn persist_inserts_after_a_general_header_with_a_trailing_comment() {
        let text = "[general]  # settings\nsome_other_key = 1\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        persist_detected_timezone(&path, text, "UTC").unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let idx_header = written.find("[general]").unwrap();
        let idx_local = written.find("local_timezone").unwrap();
        let idx_other = written.find("some_other_key").unwrap();
        assert!(idx_header < idx_local && idx_local < idx_other, "must insert right after the header line");
        assert_eq!(Config::parse(&written).unwrap().local_timezone, LocalTimezone::UTC);
    }

    #[test]
    fn persist_inserts_after_a_general_header_with_no_trailing_newline() {
        // The header is the file's last line with no trailing `\n` — a naive insert glues
        // the new key onto the `[general]` line, producing invalid TOML (#9).
        let text = "[lanes]\nfetchers = 2\n[general]";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        persist_detected_timezone(&path, text, "America/New_York").unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("[general]local_timezone"), "must not glue onto the header line");
        let cfg = Config::parse(&written).unwrap();
        assert_eq!(cfg.lanes["fetchers"], 2, "existing content must survive");
        assert_eq!(cfg.local_timezone.name(), "America/New_York");
    }

    #[test]
    fn persist_inserts_into_a_general_header_with_spaces_inside_the_brackets() {
        // `[ general ]` is the same TOML table as `[general]`. Before #98's fix, this was
        // not recognized, so a second `[general]` table got appended -- and TOML rejects a
        // table declared twice, so the daemon refused to start on the next run.
        let text = "[lanes]\nfetchers = 2\n[ general ]\nsome_other_key = 1\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        persist_detected_timezone(&path, text, "America/New_York").unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written.matches("general").count(), 1, "must not declare [general] a second time");
        let cfg = Config::parse(&written).unwrap();
        assert_eq!(cfg.lanes["fetchers"], 2, "existing content must survive");
        assert_eq!(cfg.local_timezone.name(), "America/New_York");
    }

    #[test]
    fn detect_falls_through_an_unparseable_tz_to_the_next_candidate() {
        // Not an IANA zone name (a POSIX-style TZ spec with an explicit DST rule, say) —
        // must not shadow a candidate later in the list that does parse.
        let candidates = vec!["Not/AZone".to_string(), "America/New_York".to_string()];
        let found = detect_from_candidates(candidates.into_iter()).unwrap();
        assert_eq!(found.1, "America/New_York");
    }

    #[test]
    fn detect_strips_a_leading_colon_from_a_tz_value() {
        let found = detect_from_candidates(std::iter::once(":America/New_York".to_string())).unwrap();
        assert_eq!(found.1, "America/New_York");
    }

    #[test]
    fn detect_returns_none_when_no_candidate_parses() {
        assert!(detect_from_candidates(std::iter::once("Not/AZone".to_string())).is_none());
    }
}
