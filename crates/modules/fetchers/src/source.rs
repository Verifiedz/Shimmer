//! The extension point for a job board, feed, or any other place `fetchers` reaches
//! outward to (ADR 0028 §2). One `impl Source` per site, compiled in under [`crate::sources`]
//! -- `.dev/check-deps.py` bans a module depending on another module, so there is nowhere
//! else for one to live (CLAUDE.md §8: "about forty lines," Rust, not data, unlike a
//! `records` collection).

use async_trait::async_trait;
use serde_json::Value;
use shimmer_core::{Ctx, Error, Result};

use crate::schedule::FetchersConfig;

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
    /// instance's id comes from `config.toml`, not a compile-time literal.
    fn id(&self) -> &str;
    fn method(&self) -> Method;
    /// One page, given the previous page's `next_cursor` (`None` for the first page).
    async fn fetch_page(&self, ctx: &Ctx, cursor: Option<&str>) -> Result<Page>;
}

/// The fixed, compiled-in sources (ADR 0028 §10). Not a plugin registry -- adding one of
/// these is adding a line here. A `kind`-less config entry for one of these ids adjusts it
/// (ADR 0028 §2a) -- it never replaces or disables its presence here, and [`resolve`] and
/// [`find`] can always build one by id with zero config at all, same as before. Whether one
/// shows up *unprompted* in `fetchers.list`/`fetchers.status` is a separate question
/// [`known`] answers differently (ADR 0028 §19).
pub fn registry() -> Vec<std::sync::Arc<dyn Source>> {
    vec![
        std::sync::Arc::new(crate::sources::hn::HnWhoIsHiring),
        std::sync::Arc::new(crate::sources::wwr::WeWorkRemotely),
    ]
}

pub fn find(id: &str) -> Option<std::sync::Arc<dyn Source>> {
    registry().into_iter().find(|s| s.id() == id)
}

/// The two-layered lookup ADR 0028 §2a describes: a configured `kind` builds a brand-new
/// instance from that config entry's own settings; no `kind` (or no config entry at all)
/// falls back to the fixed [`registry`]. `FetchersConfig::from_value` has already validated
/// every entry's `kind` and its required fields at load time, so a config-shape error here
/// would mean that validation has a gap, not that this run should report one -- hence
/// `Error::internal`, not `invalid_params`, on the "known kind but missing its own field"
/// branches.
pub fn resolve(ctx: &Ctx, id: &str) -> Result<Option<std::sync::Arc<dyn Source>>> {
    resolve_from(&FetchersConfig::load(ctx)?, id)
}

fn resolve_from(config: &FetchersConfig, id: &str) -> Result<Option<std::sync::Arc<dyn Source>>> {
    match config.sources.get(id).and_then(|c| c.kind.as_deref()) {
        Some("rss") => {
            let url = config.sources.get(id).and_then(|c| c.url.as_deref()).ok_or_else(|| {
                Error::internal(format!("sources.{id}: kind \"rss\" has no url despite passing load-time validation"))
            })?;
            Ok(Some(std::sync::Arc::new(crate::sources::rss::Rss::new(id.to_string(), url.to_string()))))
        }
        Some("json-api") => {
            let cfg = config.sources.get(id).ok_or_else(|| {
                Error::internal(format!(
                    "sources.{id}: kind \"json-api\" has no config despite passing load-time validation"
                ))
            })?;
            let missing = |field: &str| {
                Error::internal(format!(
                    "sources.{id}: kind \"json-api\" has no {field} despite passing load-time validation"
                ))
            };
            let url = cfg.url.clone().ok_or_else(|| missing("url"))?;
            let id_field = cfg.id_field.clone().ok_or_else(|| missing("id_field"))?;
            let title_field = cfg.title_field.clone().ok_or_else(|| missing("title_field"))?;
            let url_field = cfg.url_field.clone().ok_or_else(|| missing("url_field"))?;
            let items_path = cfg.items_path.clone().unwrap_or_default();
            let pagination = match cfg.pagination.as_deref().unwrap_or("none") {
                "offset" => {
                    let page_size = cfg.page_size.ok_or_else(|| missing("page_size"))?;
                    crate::sources::json_api::Pagination::Offset { page_size }
                }
                _ => crate::sources::json_api::Pagination::None,
            };
            Ok(Some(std::sync::Arc::new(crate::sources::json_api::JsonApi::new(
                id.to_string(),
                url,
                items_path,
                id_field,
                title_field,
                url_field,
                cfg.url_base.clone(),
                pagination,
                cfg.active_field.clone(),
            ))))
        }
        Some(other) => {
            Err(Error::internal(format!("sources.{id}: unknown kind \"{other}\" despite passing load-time validation")))
        }
        None => Ok(find(id)),
    }
}

/// Every source `fetchers.list`/`fetchers.status` surface *unprompted*: a fixed [`registry`]
/// source only once the user has given it a `[modules.fetchers.sources.<id>]` entry of its
/// own (even a bare, disabled one), plus one instance per configured `kind` entry (ADR 0028
/// §2a) -- never the whole fixed registry regardless of interest (ADR 0028 §19). This is
/// *not* the universe [`resolve`] can build something for -- that still falls back to the
/// fixed registry by id with zero config at all, so `fetchers test hn-whoishiring` keeps
/// working for anyone who already knows the id; this only governs what shows up for someone
/// who never asked.
pub fn known(ctx: &Ctx) -> Result<Vec<std::sync::Arc<dyn Source>>> {
    let config = FetchersConfig::load(ctx)?;
    let mut sources: Vec<_> = registry().into_iter().filter(|s| config.sources.contains_key(s.id())).collect();
    for (id, cfg) in &config.sources {
        if cfg.kind.is_some() {
            if let Some(s) = resolve_from(&config, id)? {
                sources.push(s);
            }
        }
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use shimmer_core::testing::TestEnv;
    use shimmer_core::ModuleConfig;

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
    fn the_registry_has_every_reference_source_and_only_known_ids_resolve() {
        let all = registry();
        let ids: Vec<_> = all.iter().map(|s| s.id()).collect();
        assert_eq!(ids, vec!["hn-whoishiring", "weworkremotely"]);
        assert!(find("hn-whoishiring").is_some());
        assert!(find("weworkremotely").is_some());
        assert!(find("not-a-real-source").is_none());
    }

    #[test]
    fn known_includes_nothing_from_the_fixed_registry_with_no_config_at_all() {
        // CLAUDE.md §1's "general before specific" -- a user with no interest in job hunting
        // must not see hn-whoishiring/weworkremotely show up unprompted just because they're
        // compiled in (ADR 0028 §19).
        let env = TestEnv::new("fetchers");
        let ids: Vec<_> = known(&env.ctx).unwrap().iter().map(|s| s.id().to_string()).collect();
        assert_eq!(ids, Vec::<String>::new());
        // Explicit invocation by id is a separate, unaffected path.
        assert!(resolve(&env.ctx, "hn-whoishiring").unwrap().is_some());
    }

    #[test]
    fn known_includes_a_fixed_source_only_once_it_has_its_own_entry() {
        let mut env = TestEnv::new("fetchers");
        env.ctx.config = ModuleConfig::new(serde_json::json!({
            "sources": {
                "hn-whoishiring": {"enabled": false, "interval": "daily"},
                "my-team-blog": {"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/feed.xml"},
            }
        }));
        let ids: Vec<_> = known(&env.ctx).unwrap().iter().map(|s| s.id().to_string()).collect();
        assert_eq!(ids, vec!["hn-whoishiring", "my-team-blog"], "weworkremotely was never configured");
    }

    #[test]
    fn resolve_builds_a_json_api_instance_from_its_own_config_entry() {
        let mut env = TestEnv::new("fetchers");
        env.ctx.config = ModuleConfig::new(serde_json::json!({
            "sources": {
                "figma-jobs": {
                    "enabled": true, "interval": "daily", "kind": "json-api",
                    "url": "https://boards-api.greenhouse.io/v1/boards/figma/jobs",
                    "items_path": "jobs", "id_field": "id", "title_field": "title", "url_field": "absolute_url",
                },
            }
        }));
        let source = resolve(&env.ctx, "figma-jobs").unwrap().expect("a configured json-api kind must resolve");
        assert_eq!(source.id(), "figma-jobs");
        assert_eq!(source.method(), Method::Api);

        let ids: Vec<_> = known(&env.ctx).unwrap().iter().map(|s| s.id().to_string()).collect();
        assert_eq!(ids, vec!["figma-jobs"]);
    }
}
