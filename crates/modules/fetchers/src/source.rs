//! The extension point for a job board, feed, or any other place `fetchers` reaches
//! outward to (ADR 0028 §2). One `impl Source` per site, compiled in under [`crate::sources`]
//! -- `.dev/check-deps.py` bans a module depending on another module, so there is nowhere
//! else for one to live (CLAUDE.md §8: "about forty lines," Rust, not data, unlike a
//! `records` collection).

use async_trait::async_trait;
use serde_json::Value;
use shimmer_core::{Ctx, Error, Result};

use crate::schedule::Sources;

/// The host loops [`Source::fetch_page`] at most this many times per run, following
/// `next_cursor` -- a `Source` never loops itself.
pub const MAX_PAGES: u32 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// A known, source-maintained endpoint. No robots.txt check (ADR 0028 §7) -- there is
    /// nothing to crawl.
    Api,
    /// A crawled page. The `Source` itself must check the target host's robots.txt (via
    /// [`crate::robots::allowed`]) before fetching -- the host has no way to know a scrape
    /// target's URL ahead of time to check it centrally.
    Scrape,
}

/// One item a page of a `Source` returned, before dedup.
#[derive(Clone, Debug, PartialEq)]
pub struct RawItem {
    /// The dedup key (ADR 0028 §4) -- stable and unique within this source, never a content
    /// hash.
    pub source_id: String,
    pub title: String,
    pub url: String,
    /// Becomes `fetchers.item.found`'s `raw_data`, capped by the caller (ADR 0028 §3) --
    /// a `Source` returns it uncapped.
    pub raw_data: Value,
}

/// Longest title and url kept. A title is already a short summary everywhere it is built; a
/// url past ~2 KiB is past what servers and browsers accept anyway.
const MAX_TITLE_CHARS: usize = 500;
const MAX_URL_CHARS: usize = 2048;

impl RawItem {
    /// Everything a `Source` returns is **untrusted remote input** on its way into the event
    /// log, and from there into automations, `notify` and `records` (#126's review, §4 and
    /// B7/B8). One gate, here, so it covers every source that exists and every one added
    /// later, rather than each `Source` impl having to remember:
    ///
    /// - A `url` that isn't `http`/`https` drops the whole item. `javascript:alert(...)` as an
    ///   item url was reaching events, and whatever eventually opens a url (a browser step, a
    ///   rule) trusts it.
    /// - Control characters are stripped from the title, the url, and every string inside
    ///   `raw_data`. A title carrying `ESC]0;pwned BEL ESC[2J` reached events raw, and while
    ///   `fetchers`' own CLI strips them, `render::cell` and `notify`'s own line rendering do
    ///   not -- so a rule doing `notify.send` with `{title}` put remote escape sequences on
    ///   the user's terminal.
    /// - Title and url are length-capped, so one pathological page can't push a single item
    ///   past anything downstream expects.
    pub fn sanitized(mut self) -> Option<Self> {
        let url = strip_control(&self.url);
        if !matches!(url::Url::parse(&url), Ok(u) if u.scheme() == "http" || u.scheme() == "https") {
            return None;
        }
        self.url = truncate(&url, MAX_URL_CHARS);
        self.title = truncate(&strip_control(&self.title), MAX_TITLE_CHARS);
        self.raw_data = strip_control_in(&self.raw_data);
        Some(self)
    }
}

/// Drops control characters (C0, DEL and C1) while keeping every printable character,
/// including non-ASCII ones -- a title in Japanese or with an em dash must survive untouched.
/// Tab and newline go too: these are single-line fields in a table, an event payload and a
/// notification body.
fn strip_control(s: &str) -> String {
    s.chars().filter(|c| !c.is_control() && !matches!(c, '\u{7f}'..='\u{9f}')).collect()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max).collect()
    }
}

/// [`strip_control`] applied to every string anywhere inside a `raw_data` value, keys included
/// -- a key is as able to carry an escape sequence as a value, and both end up rendered.
fn strip_control_in(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(strip_control(s)),
        Value::Array(items) => Value::Array(items.iter().map(strip_control_in).collect()),
        Value::Object(fields) => {
            Value::Object(fields.iter().map(|(k, v)| (strip_control(k), strip_control_in(v))).collect())
        }
        other => other.clone(),
    }
}

#[derive(Debug, Default)]
pub struct Page {
    pub items: Vec<RawItem>,
    /// `None` means there is nothing more to page through.
    pub next_cursor: Option<String>,
    /// Ids this page actually fetched and then had nothing to show for -- a deleted, dead or
    /// text-less HN comment, say. The orchestration marks them seen exactly like a kept item
    /// (#126's review, B6), so they are never re-fetched; without this they were downloaded
    /// again on every run, forever, each time eating into the page's own budget. A `Source`
    /// with nothing to report here just leaves it empty.
    pub examined_only: Vec<String>,
}

#[async_trait]
pub trait Source: Send + Sync {
    /// Also `fetchers.item.found`'s `source_name` and `data/fetchers/<id>/...`'s directory
    /// name. Borrowed from `&self`, not `&'static` (ADR 0028 §2a) -- a configured-kind
    /// instance's id comes from its own source file, not a compile-time literal.
    fn id(&self) -> &str;
    fn method(&self) -> Method;
    /// One page, given the previous page's `next_cursor` (`None` for the first page).
    async fn fetch_page(&self, ctx: &Ctx, cursor: Option<&str>) -> Result<Page>;
}

/// Builds `id`'s `Source` from its own file's `kind` (ADR 0028 §10: no fixed, compiled-in
/// registry any more -- every source, `hn-whoishiring`/`wwr` included, is a configured `kind`
/// instance). Pure -- `sources` is whatever `Sources::load` already read and validated
/// (CLAUDE.md §12 rule 10), so a config-shape error here would mean that validation has a
/// gap, not that this call should report one -- hence `Error::internal`, not
/// `invalid_params`, on the "known kind but missing its own field" branches.
pub fn resolve(sources: &Sources, id: &str) -> Result<Option<std::sync::Arc<dyn Source>>> {
    let Some(cfg) = sources.sources.get(id) else { return Ok(None) };
    let kind = crate::kind::kind(&cfg.kind).ok_or_else(|| {
        Error::internal(format!("sources.{id}: unknown kind \"{}\" despite passing load-time validation", cfg.kind))
    })?;
    let parsed = kind.parse(&cfg.kind_params).map_err(|m| {
        Error::internal(format!("sources.{id}: kind \"{}\" {m} despite passing load-time validation", cfg.kind))
    })?;
    Ok(Some(parsed.build(id.to_string())))
}

/// Every source `fetchers.list`/`fetchers.show` could ever resolve -- one instance per
/// configured source (ADR 0028 §10), skipping an id whose own file has a problem (it's
/// reported in `problems` instead, not surfaced as a `Source`).
pub fn known(sources: &Sources) -> Vec<std::sync::Arc<dyn Source>> {
    sources.sources.keys().filter_map(|id| resolve(sources, id).ok().flatten()).collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use shimmer_core::testing::TestEnv;

    use crate::schedule::test_support::write_source;
    use crate::schedule::Sources;

    use super::*;

    fn item(title: &str, url: &str, raw: Value) -> RawItem {
        RawItem { source_id: "1".into(), title: title.into(), url: url.into(), raw_data: raw }
    }

    #[test]
    fn a_non_http_url_drops_the_whole_item() {
        // #126's review, B7: `javascript:alert(document.cookie)` reached events as an item url,
        // and whatever eventually opens a url trusts it.
        for url in [
            "javascript:alert(document.cookie)",
            "data:text/html,<script>x</script>",
            "file:///etc/passwd",
            "",
            "not a url",
        ] {
            assert!(item("Job", url, json!({})).sanitized().is_none(), "should be dropped: {url}");
        }
        for url in ["http://example.test/a", "https://example.test/a?b=c#d"] {
            assert!(item("Job", url, json!({})).sanitized().is_some(), "should be kept: {url}");
        }
    }

    #[test]
    fn control_characters_never_reach_an_event_from_a_title_or_raw_data() {
        // #126's review, B8: a title of "\x1b]0;pwned\x07\x1b[2JJob" reached events raw, and
        // `render::cell` and notify's own rendering print it as-is -- so a rule doing
        // notify.send with {title} put escape sequences on the user's terminal.
        let hostile = "\u{1b}]0;pwned\u{7}\u{1b}[2JJob";
        let clean = item(hostile, "https://example.test/a", json!({"snippet": hostile, "nested": {"x": [hostile]}}))
            .sanitized()
            .unwrap();
        assert_eq!(clean.title, "]0;pwned[2JJob", "printable text stays, control bytes go");
        assert!(!clean.title.contains('\u{1b}') && !clean.title.contains('\u{7}'));
        assert!(!clean.raw_data.to_string().contains('\u{1b}'), "raw_data too: {}", clean.raw_data);
        assert!(!clean.raw_data["nested"]["x"][0].as_str().unwrap().contains('\u{1b}'), "at any depth");
    }

    #[test]
    fn ordinary_non_ascii_text_survives_sanitising_untouched() {
        let kept = item("Rust エンジニア — Remote", "https://example.test/a", json!({})).sanitized().unwrap();
        assert_eq!(kept.title, "Rust エンジニア — Remote");
    }

    #[test]
    fn a_pathological_title_or_url_is_length_capped() {
        let long_title = "x".repeat(MAX_TITLE_CHARS + 500);
        let capped = item(&long_title, "https://example.test/a", json!({})).sanitized().unwrap();
        assert_eq!(capped.title.chars().count(), MAX_TITLE_CHARS);

        let long_url = format!("https://example.test/{}", "a".repeat(MAX_URL_CHARS + 500));
        let capped = item("Job", &long_url, json!({})).sanitized().unwrap();
        assert_eq!(capped.url.chars().count(), MAX_URL_CHARS);
    }

    #[test]
    fn resolve_returns_none_for_an_unconfigured_id() {
        // ADR 0028 §10 -- there is no fixed, compiled-in registry any more; an id resolves
        // only once it has its own config entry naming a kind.
        let env = TestEnv::new("fetchers");
        let sources = Sources::load(&env.ctx).unwrap();
        assert!(resolve(&sources, "hn-whoishiring").unwrap().is_none());
        assert!(known(&sources).is_empty());
    }

    #[test]
    fn known_and_resolve_both_need_a_configured_kind() {
        let env = TestEnv::new("fetchers");
        write_source(&env, "my-hn", json!({"enabled": true, "interval": "daily", "kind": "hn-whoishiring"}));
        write_source(
            &env,
            "my-team-blog",
            json!({"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/feed.xml"}),
        );
        let sources = Sources::load(&env.ctx).unwrap();
        let ids: Vec<_> = known(&sources).iter().map(|s| s.id().to_string()).collect();
        assert_eq!(ids, vec!["my-hn", "my-team-blog"]);
        assert!(resolve(&sources, "my-hn").unwrap().is_some());
        assert!(resolve(&sources, "not-configured").unwrap().is_none());
    }

    #[test]
    fn resolve_builds_a_json_api_instance_from_its_own_config_entry() {
        let env = TestEnv::new("fetchers");
        write_source(
            &env,
            "figma-jobs",
            json!({
                "enabled": true, "interval": "daily", "kind": "json-api",
                "url": "https://boards-api.greenhouse.io/v1/boards/figma/jobs",
                "items_path": "jobs", "id_field": "id", "title_field": "title", "url_field": "absolute_url",
            }),
        );
        let sources = Sources::load(&env.ctx).unwrap();
        let source = resolve(&sources, "figma-jobs").unwrap().expect("a configured json-api kind must resolve");
        assert_eq!(source.id(), "figma-jobs");
        assert_eq!(source.method(), Method::Api);

        let ids: Vec<_> = known(&sources).iter().map(|s| s.id().to_string()).collect();
        assert_eq!(ids, vec!["figma-jobs"]);
    }
}
