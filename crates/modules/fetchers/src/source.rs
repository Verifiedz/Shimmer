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

#[derive(Debug)]
pub struct Page {
    pub items: Vec<RawItem>,
    /// `None` means there is nothing more to page through.
    pub next_cursor: Option<String>,
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
/// these is adding a line here. Unrelated to, and unaffected by, `config.toml`: each is
/// always present whether or not it has a `[modules.fetchers.sources.*]` entry at all
/// (ADR 0028 §2a -- a `kind`-less config entry only ever adjusts one of these by id, never
/// replaces or disables its presence here).
pub fn registry() -> Vec<std::sync::Arc<dyn Source>> {
    vec![
        std::sync::Arc::new(crate::sources::hn::HnWhoIsHiring),
        std::sync::Arc::new(crate::sources::wwr::WeWorkRemotely),
        std::sync::Arc::new(crate::sources::jobbank::JobBankCanada),
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
            ))))
        }
        Some(other) => {
            Err(Error::internal(format!("sources.{id}: unknown kind \"{other}\" despite passing load-time validation")))
        }
        None => Ok(find(id)),
    }
}

/// Every source the daemon currently knows about: the fixed [`registry`], plus one instance
/// per configured `kind` entry (ADR 0028 §2a). What `fetchers.list`/`fetchers.status` show,
/// and the universe [`resolve`] can ever return something for.
pub fn known(ctx: &Ctx) -> Result<Vec<std::sync::Arc<dyn Source>>> {
    let config = FetchersConfig::load(ctx)?;
    let mut sources = registry();
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
    use shimmer_core::testing::TestEnv;
    use shimmer_core::ModuleConfig;

    use super::*;

    #[test]
    fn the_registry_has_every_reference_source_and_only_known_ids_resolve() {
        let all = registry();
        let ids: Vec<_> = all.iter().map(|s| s.id()).collect();
        assert_eq!(ids, vec!["hn-whoishiring", "weworkremotely", "jobbank-canada"]);
        assert!(find("hn-whoishiring").is_some());
        assert!(find("weworkremotely").is_some());
        assert!(find("jobbank-canada").is_some());
        assert!(find("not-a-real-source").is_none());
    }

    #[test]
    fn known_includes_the_fixed_registry_with_no_config_at_all() {
        let env = TestEnv::new("fetchers");
        let ids: Vec<_> = known(&env.ctx).unwrap().iter().map(|s| s.id().to_string()).collect();
        assert_eq!(ids, vec!["hn-whoishiring", "weworkremotely", "jobbank-canada"]);
    }

    #[test]
    fn known_adds_every_configured_kind_instance_on_top_of_the_fixed_registry() {
        let mut env = TestEnv::new("fetchers");
        env.ctx.config = ModuleConfig::new(serde_json::json!({
            "sources": {
                "my-team-blog": {"enabled": true, "interval": "hourly", "kind": "rss", "url": "https://example.test/feed.xml"},
            }
        }));
        let ids: Vec<_> = known(&env.ctx).unwrap().iter().map(|s| s.id().to_string()).collect();
        assert_eq!(ids, vec!["hn-whoishiring", "weworkremotely", "jobbank-canada", "my-team-blog"]);
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
        assert_eq!(ids, vec!["hn-whoishiring", "weworkremotely", "jobbank-canada", "figma-jobs"]);
    }
}
