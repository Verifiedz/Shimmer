//! The `rss` kind (ADR 0028 §2a): any RSS 0.x/1.0/2.0 or Atom feed, configured by url rather
//! than compiled in. One instance per source file with its own
//! `kind = "rss"`, built by [`crate::source::resolve`] -- never part of the fixed
//! [`crate::source::registry`].

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use shimmer_core::{Ctx, Error, Result};

use crate::http;
use crate::kind::{self, Kind, KindConfig};
use crate::source::{Method, Page, RawItem, Source};

pub struct Rss {
    id: String,
    url: String,
    credential: Option<String>,
}

impl Rss {
    pub fn new(id: String, url: String, credential: Option<String>) -> Self {
        Self { id, url, credential }
    }
}

/// The `rss` kind (ADR 0028 §3): one required field, `url`; an optional `credential` (ADR
/// 0036, ADR 0028 §4) for a feed that needs an auth header.
pub struct RssKind;

const RSS_FIELDS: &[&str] = &["url", "credential"];

impl Kind for RssKind {
    fn name(&self) -> &'static str {
        "rss"
    }

    fn method(&self) -> Method {
        Method::Api
    }

    fn parse(&self, params: &Map<String, Value>) -> std::result::Result<Box<dyn KindConfig>, String> {
        kind::reject_unknown(params, RSS_FIELDS)?;
        let url = kind::required_url(params, "url")?;
        let credential = kind::optional_str(params, "credential")?;
        Ok(Box::new(RssConfig { url, credential }))
    }
}

#[derive(Debug)]
struct RssConfig {
    url: String,
    credential: Option<String>,
}

impl KindConfig for RssConfig {
    fn build(&self, id: String) -> std::sync::Arc<dyn Source> {
        std::sync::Arc::new(Rss::new(id, self.url.clone(), self.credential.clone()))
    }
}

#[async_trait]
impl Source for Rss {
    fn id(&self) -> &str {
        &self.id
    }

    fn method(&self) -> Method {
        // A feed url is a known, fetcher-maintained endpoint -- the same reasoning as
        // hn-whoishiring's Firebase API (ADR 0028 §10) -- not a crawled site, so no
        // robots.txt check applies (ADR 0028 §2a).
        Method::Api
    }

    async fn fetch_page(&self, ctx: &Ctx, cursor: Option<&str>) -> Result<Page> {
        // A feed has no pagination concept (ADR 0028 §2a): everything comes back in one
        // page. `fetch_page` is still only ever called with `cursor: None` by the host
        // (it never follows a `next_cursor` this impl never returns), but guard it anyway
        // rather than silently re-fetching if that assumption ever changes elsewhere.
        if cursor.is_some() {
            return Ok(Page::default());
        }

        let resp = match &self.credential {
            Some(c) => http::require_ok(http::get_with_credential(ctx, &self.url, c).await?, &self.url)?,
            None => http::require_ok(http::get(ctx, &self.url).await?, &self.url)?,
        };
        // feed-rs fills in any entry id the feed itself left blank (`assign_missing_ids` ->
        // `generate_id`): a hash of the entry's link + title when both exist, or a random
        // UUID when it has neither. A hash keyed on title changes the dedup key the moment
        // someone edits a post's title, and a random UUID changes it on every single parse
        // -- either way `entry.id` is no longer a stable dedup key, which is the one thing
        // `source_id` (ADR 0028 §4) is required to be. Overriding the generator to derive
        // solely from the link keeps it stable across both title edits and reparses; an
        // entry with no link at all then gets an empty id, which is handled by skipping it
        // below rather than by falling back to a non-id value.
        let parser = feed_rs::parser::Builder::new()
            .id_generator(|links: &[feed_rs::model::Link], _, _| {
                links.first().map(|l| l.href.clone()).unwrap_or_default()
            })
            .build();
        let feed = parser
            .parse(resp.body.as_slice())
            .map_err(|e| Error::module_error(format!("rss: could not parse feed at {}: {e}", self.url)))?;

        // Zero entries is not treated as a failure the way an HTML selector matching
        // nothing is (`html::select_or_fail`) -- a feed can legitimately have no items yet
        // (a brand-new blog), so there is no "structure changed" signal to read into it.
        // An entry can still end up with an empty id here: the feed itself may supply an
        // explicit `<id>`/`<guid>` (in which case our override above is never even called,
        // per `assign_missing_ids`), but an empty *explicit* id is as useless a dedup key
        // as a missing one, and an entry with neither a feed-supplied id nor a link has
        // nothing stable to key on either way -- both are dropped rather than emitted with
        // an empty source_id or empty url.
        let items = feed
            .entries
            .into_iter()
            .filter_map(|entry| {
                if entry.id.trim().is_empty() {
                    return None;
                }
                let url = entry.links.first().map(|l| l.href.clone()).unwrap_or_default();
                if url.trim().is_empty() {
                    return None;
                }
                let title = entry.title.map(|t| t.content).unwrap_or_else(|| url.clone());
                Some(RawItem {
                    source_id: entry.id,
                    title,
                    url,
                    raw_data: json!({
                        "summary": entry.summary.map(|t| t.content),
                        "published": entry.published,
                    }),
                })
            })
            .collect();

        Ok(Page { items, ..Page::default() })
    }
}

#[cfg(test)]
mod tests {
    use shimmer_core::testing::TestEnv;
    use shimmer_core::{ErrorCode, HttpResponse};

    use super::*;

    const ATOM_FEED: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Example Blog</title>
  <id>urn:uuid:feed-1</id>
  <updated>2026-10-01T00:00:00Z</updated>
  <entry>
    <id>urn:uuid:entry-1</id>
    <title>First post</title>
    <link href="https://example.test/posts/1"/>
    <summary>The first post's summary.</summary>
    <updated>2026-10-01T00:00:00Z</updated>
  </entry>
  <entry>
    <id>urn:uuid:entry-2</id>
    <title>Second post</title>
    <link href="https://example.test/posts/2"/>
    <updated>2026-10-02T00:00:00Z</updated>
  </entry>
</feed>"#;

    const EMPTY_ATOM_FEED: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Empty Blog</title>
  <id>urn:uuid:feed-empty</id>
  <updated>2026-10-01T00:00:00Z</updated>
</feed>"#;

    /// One entry with no `<id>` of its own, so feed-rs would otherwise invent one --
    /// `v1` and `v2` keep the same link but differ only in `<title>`, to check that the
    /// generated dedup key tracks the link rather than the title.
    fn no_id_feed_with_title(title: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Example Blog</title>
  <id>urn:uuid:feed-1</id>
  <updated>2026-10-01T00:00:00Z</updated>
  <entry>
    <title>{title}</title>
    <link href="https://example.test/posts/1"/>
    <updated>2026-10-01T00:00:00Z</updated>
  </entry>
</feed>"#
        )
    }

    /// One entry with neither an `<id>` nor a `<link>` -- feed-rs's own default generator
    /// would fall back to a random UUID here, which is exactly the unstable-dedup-key case
    /// this module's override exists to avoid.
    const NO_ID_NO_LINK_FEED: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Example Blog</title>
  <id>urn:uuid:feed-1</id>
  <updated>2026-10-01T00:00:00Z</updated>
  <entry>
    <title>Untethered post</title>
    <updated>2026-10-01T00:00:00Z</updated>
  </entry>
</feed>"#;

    /// An entry with an explicit `<id>` but no `<link>` -- the feed itself supplied an id
    /// (so the override generator is never consulted), but there is still no url to show
    /// the user.
    const EXPLICIT_ID_NO_LINK_FEED: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Example Blog</title>
  <id>urn:uuid:feed-1</id>
  <updated>2026-10-01T00:00:00Z</updated>
  <entry>
    <id>urn:uuid:entry-1</id>
    <title>Linkless post</title>
    <updated>2026-10-01T00:00:00Z</updated>
  </entry>
</feed>"#;

    fn ok(body: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: body.as_bytes().to_vec(),
            from_cache: false,
            stale: false,
            retry_after_secs: None,
        }
    }

    fn stub(env: &TestEnv, url: &str, resp: HttpResponse) {
        env.http.responses.lock().unwrap().insert(url.to_owned(), resp);
    }

    #[test]
    fn id_is_whatever_it_was_constructed_with_and_method_is_api() {
        let rss = Rss::new("my-team-blog".into(), "https://example.test/feed.xml".into(), None);
        assert_eq!(rss.id(), "my-team-blog");
        assert_eq!(rss.method(), Method::Api);
    }

    #[tokio::test]
    async fn both_entries_parse_with_their_dedup_key_title_and_url() {
        let env = TestEnv::new("fetchers");
        stub(&env, "https://example.test/feed.xml", ok(ATOM_FEED));
        let rss = Rss::new("my-team-blog".into(), "https://example.test/feed.xml".into(), None);

        let page = rss.fetch_page(&env.ctx, None).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].source_id, "urn:uuid:entry-1");
        assert_eq!(page.items[0].title, "First post");
        assert_eq!(page.items[0].url, "https://example.test/posts/1");
        assert_eq!(page.items[0].raw_data["summary"], "The first post's summary.");
        assert_eq!(page.items[1].source_id, "urn:uuid:entry-2");
        assert!(page.next_cursor.is_none(), "a feed is always a single page");
    }

    #[tokio::test]
    async fn an_entry_with_no_feed_supplied_id_gets_a_dedup_key_derived_from_its_link() {
        let env = TestEnv::new("fetchers");
        stub(&env, "https://example.test/feed.xml", ok(&no_id_feed_with_title("Original title")));
        let rss = Rss::new("my-team-blog".into(), "https://example.test/feed.xml".into(), None);

        let page = rss.fetch_page(&env.ctx, None).await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].source_id, "https://example.test/posts/1");
    }

    #[tokio::test]
    async fn editing_the_title_of_a_link_only_entry_does_not_change_its_dedup_key() {
        // The bug this override exists to fix: feed-rs's own default generator hashes
        // link + title together, so an edited title alone would otherwise mint a brand
        // new id and the item would be wrongly re-emitted as new.
        let env = TestEnv::new("fetchers");
        let rss = Rss::new("my-team-blog".into(), "https://example.test/feed.xml".into(), None);

        stub(&env, "https://example.test/feed.xml", ok(&no_id_feed_with_title("Original title")));
        let first = rss.fetch_page(&env.ctx, None).await.unwrap();

        stub(&env, "https://example.test/feed.xml", ok(&no_id_feed_with_title("Edited title")));
        let second = rss.fetch_page(&env.ctx, None).await.unwrap();

        assert_eq!(first.items[0].source_id, second.items[0].source_id);
    }

    #[tokio::test]
    async fn an_entry_with_neither_a_feed_id_nor_a_link_is_skipped_not_given_a_random_id() {
        // feed-rs's own default generator falls back to a random UUID here, which would
        // make this item look "new" on every single parse -- the opposite of dedup.
        let env = TestEnv::new("fetchers");
        stub(&env, "https://example.test/feed.xml", ok(NO_ID_NO_LINK_FEED));
        let rss = Rss::new("my-team-blog".into(), "https://example.test/feed.xml".into(), None);

        let page = rss.fetch_page(&env.ctx, None).await.unwrap();
        assert!(page.items.is_empty());
    }

    #[tokio::test]
    async fn an_entry_with_an_explicit_id_but_no_link_is_skipped() {
        let env = TestEnv::new("fetchers");
        stub(&env, "https://example.test/feed.xml", ok(EXPLICIT_ID_NO_LINK_FEED));
        let rss = Rss::new("my-team-blog".into(), "https://example.test/feed.xml".into(), None);

        let page = rss.fetch_page(&env.ctx, None).await.unwrap();
        assert!(page.items.is_empty(), "no link means no usable url, even with an explicit id");
    }

    #[tokio::test]
    async fn zero_entries_is_an_empty_page_not_a_failure() {
        let env = TestEnv::new("fetchers");
        stub(&env, "https://example.test/feed.xml", ok(EMPTY_ATOM_FEED));
        let rss = Rss::new("my-team-blog".into(), "https://example.test/feed.xml".into(), None);

        let page = rss.fetch_page(&env.ctx, None).await.unwrap();
        assert!(page.items.is_empty());
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn a_second_page_is_never_fetched() {
        let env = TestEnv::new("fetchers");
        // Deliberately no stub for the url -- if this impl ever tried to re-fetch for a
        // cursor it never hands out, the test would fail with "no stub for...".
        let rss = Rss::new("my-team-blog".into(), "https://example.test/feed.xml".into(), None);
        let page = rss.fetch_page(&env.ctx, Some("anything")).await.unwrap();
        assert!(page.items.is_empty());
    }

    #[tokio::test]
    async fn malformed_xml_is_a_classifiable_module_error() {
        let env = TestEnv::new("fetchers");
        stub(&env, "https://example.test/feed.xml", ok("not a feed at all"));
        let rss = Rss::new("my-team-blog".into(), "https://example.test/feed.xml".into(), None);

        let err = rss.fetch_page(&env.ctx, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
        assert!(err.message.contains("could not parse feed"), "{}", err.message);
    }

    fn params(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn rss_kind_parses_a_url_and_an_optional_credential() {
        let cfg = RssKind.parse(&params(json!({"url": "https://example.test/feed.xml"}))).unwrap();
        let source = cfg.build("my-team-blog".into());
        assert_eq!(source.id(), "my-team-blog");
        assert_eq!(source.method(), Method::Api);

        let cfg =
            RssKind.parse(&params(json!({"url": "https://example.test/feed.xml", "credential": "my-key"}))).unwrap();
        cfg.build("my-team-blog".into()); // doesn't panic; credential plumbing checked below
    }

    #[test]
    fn rss_kind_rejects_a_missing_or_malformed_url_and_any_unknown_field() {
        assert!(RssKind.parse(&params(json!({}))).unwrap_err().contains("url"));
        assert!(RssKind.parse(&params(json!({"url": "not-a-url"}))).unwrap_err().contains("url"));
        assert!(RssKind.parse(&params(json!({"url": "ftp://x.test/f.xml"}))).unwrap_err().contains("url"));
        assert!(RssKind.parse(&params(json!({"url": "https://x.test/f.xml", "credential": "  "}))).is_err());
        let err = RssKind.parse(&params(json!({"url": "https://x.test/f.xml", "items_path": "jobs"}))).unwrap_err();
        assert!(err.contains("items_path"), "{err}");
    }

    #[tokio::test]
    async fn a_configured_credential_is_attached_to_every_request() {
        let env = TestEnv::new("fetchers");
        stub(&env, "https://example.test/feed.xml", ok(EMPTY_ATOM_FEED));
        let rss = Rss::new("my-team-blog".into(), "https://example.test/feed.xml".into(), Some("my-key".into()));

        rss.fetch_page(&env.ctx, None).await.unwrap();
        let calls = env.http.calls.lock().unwrap();
        assert_eq!(calls.last(), Some(&("https://example.test/feed.xml".to_string(), Some("my-key".to_string()))));
    }
}
