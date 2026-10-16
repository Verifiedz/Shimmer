//! The `json-api` kind (ADR 0028 §15): any JSON endpoint that returns an array of job-shaped
//! objects, configured entirely by a field-path mapping rather than a bespoke `Source` impl
//! -- the same "point at data, not code" idea `rss` already established for feeds. Covers an
//! ATS's public job-board API (e.g. Greenhouse's `{"jobs": [...]}`) or a company's own public
//! job-search endpoint (e.g. Amazon's paginated `{"jobs": [...]}` with relative urls) with
//! zero new Rust per site. Does not cover an HTML page (`weworkremotely` stays a hand-written
//! `Method::Scrape` source) or an endpoint needing auth headers or a POST body -- those still
//! need their own `Source`.

use async_trait::async_trait;
use serde_json::Value;
use shimmer_core::{Ctx, Error, Result};

use crate::http;
use crate::source::{Method, Page, RawItem, Source};

/// How to walk through more than one page. `rss`-shaped feeds and most ATS boards return
/// everything in one response (`None`); a paginated search endpoint like Amazon's needs
/// `Offset`, which the `Source` drives itself by substituting a literal `{offset}` placeholder
/// in the configured url -- `FetchersConfig::from_value` has already checked that placeholder
/// is actually present before a `kind = "json-api"` entry with `pagination = "offset"` can
/// pass load-time validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pagination {
    None,
    Offset { page_size: u32 },
}

/// One `[modules.fetchers.sources.<id>]` entry's field-path mapping (ADR 0028 §15), built once
/// by `crate::source::resolve` from validated config -- never constructed directly from raw,
/// unvalidated input.
pub struct JsonApi {
    id: String,
    url: String,
    /// Dot-separated path of object keys from the response root to the items array; empty
    /// means the response root is already that array (e.g. Lever's bare-array response,
    /// unlike Greenhouse's `{"jobs": [...]}`).
    items_path: String,
    id_field: String,
    title_field: String,
    url_field: String,
    /// Prepended to `url_field`'s value when present -- for an API (like Amazon's) whose own
    /// url field is a site-relative path, not already absolute.
    url_base: Option<String>,
    pagination: Pagination,
    /// The field marking an item still open; `false` there drops the item before dedup ever
    /// sees it. `None` means no such field is configured, so nothing is dropped on this
    /// basis.
    active_field: Option<String>,
}

impl JsonApi {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        url: String,
        items_path: String,
        id_field: String,
        title_field: String,
        url_field: String,
        url_base: Option<String>,
        pagination: Pagination,
        active_field: Option<String>,
    ) -> Self {
        Self { id, url, items_path, id_field, title_field, url_field, url_base, pagination, active_field }
    }
}

#[async_trait]
impl Source for JsonApi {
    fn id(&self) -> &str {
        &self.id
    }

    fn method(&self) -> Method {
        // A url the user pointed us at deliberately -- the same reasoning as `rss` and
        // `hn-whoishiring` (ADR 0028 §10) -- not a crawled page, so no robots.txt check
        // applies.
        Method::Api
    }

    async fn fetch_page(&self, ctx: &Ctx, cursor: Option<&str>) -> Result<Page> {
        let page_size = match self.pagination {
            Pagination::Offset { page_size } => Some(page_size),
            Pagination::None => None,
        };
        let offset: u32 = match (page_size, cursor) {
            (Some(_), Some(c)) => c.parse().map_err(|_| Error::invalid_params(format!("bad cursor '{c}'")))?,
            _ => 0,
        };
        let url =
            if page_size.is_some() { self.url.replace("{offset}", &offset.to_string()) } else { self.url.clone() };

        let resp = http::require_ok(http::get(ctx, &url).await?, &url)?;
        let body: Value = serde_json::from_slice(&resp.body)
            .map_err(|e| Error::module_error(format!("json-api: could not parse response from {url}: {e}")))?;
        let array = select_array(&body, &self.items_path).ok_or_else(|| {
            Error::module_error(format!(
                "json-api: '{}' is not an array in the response from {url}",
                if self.items_path.is_empty() { "<response root>" } else { &self.items_path }
            ))
        })?;

        let mut items = Vec::new();
        for item in array {
            if let Some(field) = &self.active_field {
                if item.get(field.as_str()) == Some(&Value::Bool(false)) {
                    continue;
                }
            }
            let Some(source_id) = field_as_string(item, &self.id_field) else { continue };
            let Some(title) = field_as_string(item, &self.title_field) else { continue };
            let Some(raw_url) = field_as_string(item, &self.url_field) else { continue };
            let url = match &self.url_base {
                Some(base) => format!("{base}{raw_url}"),
                None => raw_url,
            };
            items.push(RawItem { source_id, title, url, raw_data: item.clone() });
        }

        // Fewer than a full page means this was the last one -- the same "ask again only if
        // the last answer was full" signal a page-number scheme would use, just expressed as
        // a running offset instead.
        let next_cursor = page_size.filter(|&n| items.len() as u32 >= n).map(|n| (offset + n).to_string());
        Ok(Page { items, next_cursor })
    }
}

/// Walks a dot-separated path of object keys from `root`, then expects the value found there
/// to be an array. `path` empty means `root` itself must already be the array.
fn select_array<'a>(root: &'a Value, path: &str) -> Option<&'a Vec<Value>> {
    let mut current = root;
    if !path.is_empty() {
        for key in path.split('.') {
            current = current.get(key)?;
        }
    }
    current.as_array()
}

/// A job-shaped field's value as a string, coercing a JSON number (e.g. Greenhouse's own
/// numeric `id`) the same way a string field already is. `None` for anything else (missing,
/// null, an object or array) -- the caller treats that as "skip this one item" rather than
/// guessing at a value that was never really there.
fn field_as_string(item: &Value, field: &str) -> Option<String> {
    match item.get(field)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use shimmer_core::testing::TestEnv;
    use shimmer_core::{ErrorCode, HttpResponse};

    use super::*;

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

    fn greenhouse(id: &str, url: &str) -> JsonApi {
        JsonApi::new(
            id.to_string(),
            url.to_string(),
            "jobs".to_string(),
            "id".to_string(),
            "title".to_string(),
            "absolute_url".to_string(),
            None,
            Pagination::None,
            None,
        )
    }

    const GREENHOUSE_PAGE: &str = r#"{"jobs": [
        {"id": 111, "title": "Backend Engineer", "absolute_url": "https://boards.greenhouse.io/acme/jobs/111", "location": {"name": "Remote"}},
        {"id": 222, "title": "Full Stack Engineer", "absolute_url": "https://boards.greenhouse.io/acme/jobs/222", "location": {"name": "NYC"}}
    ]}"#;

    #[test]
    fn id_is_whatever_it_was_constructed_with_and_method_is_api() {
        let api = greenhouse("figma-jobs", "https://example.test/jobs");
        assert_eq!(api.id(), "figma-jobs");
        assert_eq!(api.method(), Method::Api);
    }

    #[tokio::test]
    async fn a_greenhouse_shaped_page_parses_with_a_numeric_id_coerced_to_a_string() {
        let env = TestEnv::new("fetchers");
        stub(&env, "https://example.test/jobs", ok(GREENHOUSE_PAGE));
        let api = greenhouse("figma-jobs", "https://example.test/jobs");

        let page = api.fetch_page(&env.ctx, None).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].source_id, "111");
        assert_eq!(page.items[0].title, "Backend Engineer");
        assert_eq!(page.items[0].url, "https://boards.greenhouse.io/acme/jobs/111");
        assert_eq!(page.items[0].raw_data["location"]["name"], "Remote");
        assert!(page.next_cursor.is_none(), "no pagination configured means a single page");
    }

    #[tokio::test]
    async fn a_bare_array_response_is_read_when_items_path_is_empty() {
        const LEVER_PAGE: &str = r#"[
            {"id": "abc-1", "text": "Site Reliability Engineer", "hostedUrl": "https://jobs.lever.co/acme/abc-1"}
        ]"#;
        let env = TestEnv::new("fetchers");
        stub(&env, "https://example.test/postings", ok(LEVER_PAGE));
        let api = JsonApi::new(
            "acme-lever".into(),
            "https://example.test/postings".into(),
            String::new(),
            "id".into(),
            "text".into(),
            "hostedUrl".into(),
            None,
            Pagination::None,
            None,
        );

        let page = api.fetch_page(&env.ctx, None).await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].source_id, "abc-1");
        assert_eq!(page.items[0].title, "Site Reliability Engineer");
    }

    #[tokio::test]
    async fn a_relative_url_field_is_prefixed_with_url_base() {
        const AMAZON_PAGE: &str = r#"{"jobs": [{"id": "req-1", "title": "SDE", "job_path": "/en/jobs/1/sde"}]}"#;
        let env = TestEnv::new("fetchers");
        stub(&env, "https://example.test/search.json?offset=0", ok(AMAZON_PAGE));
        let api = JsonApi::new(
            "amazon-swe".into(),
            "https://example.test/search.json?offset={offset}".into(),
            "jobs".into(),
            "id".into(),
            "title".into(),
            "job_path".into(),
            Some("https://example.test".into()),
            Pagination::Offset { page_size: 50 },
            None,
        );

        let page = api.fetch_page(&env.ctx, None).await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].url, "https://example.test/en/jobs/1/sde");
    }

    #[tokio::test]
    async fn offset_pagination_substitutes_the_running_offset_and_stops_on_a_short_page() {
        let env = TestEnv::new("fetchers");
        let page_fn = |ids: &[u32]| {
            let jobs: Vec<Value> = ids
                .iter()
                .map(|i| serde_json::json!({"id": i, "title": format!("Job {i}"), "url": format!("https://x/{i}")}))
                .collect();
            serde_json::json!({"jobs": jobs}).to_string()
        };
        stub(&env, "https://example.test/jobs?offset=0", ok(&page_fn(&[1, 2])));
        stub(&env, "https://example.test/jobs?offset=2", ok(&page_fn(&[3])));
        let api = JsonApi::new(
            "paged".into(),
            "https://example.test/jobs?offset={offset}".into(),
            "jobs".into(),
            "id".into(),
            "title".into(),
            "url".into(),
            None,
            Pagination::Offset { page_size: 2 },
            None,
        );

        let first = api.fetch_page(&env.ctx, None).await.unwrap();
        assert_eq!(first.items.len(), 2);
        assert_eq!(first.next_cursor, Some("2".to_string()), "a full page means there may be more");

        let second = api.fetch_page(&env.ctx, first.next_cursor.as_deref()).await.unwrap();
        assert_eq!(second.items.len(), 1);
        assert!(second.next_cursor.is_none(), "a short page means this was the last one");
    }

    #[tokio::test]
    async fn active_field_false_drops_the_item_but_missing_or_true_pass_through() {
        const PAGE: &str = r#"{"jobs": [
            {"id": 1, "title": "Closed role", "absolute_url": "https://x/1", "active": false},
            {"id": 2, "title": "Open role", "absolute_url": "https://x/2", "active": true},
            {"id": 3, "title": "No active field at all", "absolute_url": "https://x/3"}
        ]}"#;
        let env = TestEnv::new("fetchers");
        stub(&env, "https://example.test/jobs", ok(PAGE));
        let api = JsonApi::new(
            "acme-jobs".into(),
            "https://example.test/jobs".into(),
            "jobs".into(),
            "id".into(),
            "title".into(),
            "absolute_url".into(),
            None,
            Pagination::None,
            Some("active".into()),
        );

        let page = api.fetch_page(&env.ctx, None).await.unwrap();
        let ids: Vec<&str> = page.items.iter().map(|i| i.source_id.as_str()).collect();
        assert_eq!(ids, ["2", "3"], "only the item explicitly marked inactive is dropped");
    }

    #[tokio::test]
    async fn an_item_missing_a_mapped_field_is_skipped_not_a_page_failure() {
        const PAGE: &str = r#"{"jobs": [
            {"id": 1, "title": "Has everything", "absolute_url": "https://x/1"},
            {"id": 2, "absolute_url": "https://x/2"}
        ]}"#;
        let env = TestEnv::new("fetchers");
        stub(&env, "https://example.test/jobs", ok(PAGE));
        let api = greenhouse("acme-jobs", "https://example.test/jobs");

        let page = api.fetch_page(&env.ctx, None).await.unwrap();
        assert_eq!(page.items.len(), 1, "the item with no title must be skipped, not fail the page");
        assert_eq!(page.items[0].source_id, "1");
    }

    #[tokio::test]
    async fn items_path_pointing_at_something_that_is_not_an_array_is_a_classifiable_failure() {
        let env = TestEnv::new("fetchers");
        stub(&env, "https://example.test/jobs", ok(r#"{"jobs": "not an array"}"#));
        let api = greenhouse("acme-jobs", "https://example.test/jobs");

        let err = api.fetch_page(&env.ctx, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
        assert!(err.message.contains("not an array"), "{}", err.message);
    }

    #[tokio::test]
    async fn items_path_pointing_at_a_missing_key_is_also_a_classifiable_failure() {
        let env = TestEnv::new("fetchers");
        stub(&env, "https://example.test/jobs", ok(r#"{"postings": []}"#));
        let api = greenhouse("acme-jobs", "https://example.test/jobs");

        let err = api.fetch_page(&env.ctx, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
    }

    #[tokio::test]
    async fn malformed_json_is_a_classifiable_parse_failure() {
        let env = TestEnv::new("fetchers");
        stub(&env, "https://example.test/jobs", ok("not json at all"));
        let api = greenhouse("acme-jobs", "https://example.test/jobs");

        let err = api.fetch_page(&env.ctx, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
        assert!(err.message.contains("could not parse response"), "{}", err.message);
    }
}
