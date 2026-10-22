//! The `json-api` kind (ADR 0028 §15): any JSON endpoint that returns an array of job-shaped
//! objects, configured entirely by a field-path mapping rather than a bespoke `Source` impl
//! -- the same "point at data, not code" idea `rss` already established for feeds. Covers an
//! ATS's public job-board API (e.g. Greenhouse's `{"jobs": [...]}`) or a company's own public
//! job-search endpoint (e.g. Amazon's paginated `{"jobs": [...]}` with relative urls) with
//! zero new Rust per site. Does not cover an HTML page (`weworkremotely` stays a hand-written
//! `Method::Scrape` source) or an endpoint needing auth headers or a POST body -- those still
//! need their own `Source`.

use async_trait::async_trait;
use serde_json::{Map, Value};
use shimmer_core::{Ctx, Error, Result};

use crate::http;
use crate::kind::{self, Kind, KindConfig};
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
    /// A named credential (ADR 0036, ADR 0028 §4) to attach to every request, for an endpoint
    /// that needs an auth header. `None` for every public, unauthenticated board.
    credential: Option<String>,
}

/// The `json-api` kind (ADR 0028 §15): a declarative, field-path-mapped source for any
/// endpoint that answers with a JSON array of job-shaped objects.
pub struct JsonApiKind;

const JSON_API_FIELDS: &[&str] = &[
    "url",
    "items_path",
    "id_field",
    "title_field",
    "url_field",
    "url_base",
    "pagination",
    "page_size",
    "active_field",
    "credential",
];

impl Kind for JsonApiKind {
    fn name(&self) -> &'static str {
        "json-api"
    }

    fn method(&self) -> Method {
        Method::Api
    }

    fn parse(&self, params: &Map<String, Value>) -> std::result::Result<Box<dyn KindConfig>, String> {
        kind::reject_unknown(params, JSON_API_FIELDS)?;
        let url = kind::required_url(params, "url")?;
        let items_path = kind::optional_str(params, "items_path")?.unwrap_or_default();
        let id_field = kind::required_str(params, "id_field")?;
        let title_field = kind::required_str(params, "title_field")?;
        let url_field = kind::required_str(params, "url_field")?;
        let url_base = kind::optional_str(params, "url_base")?;
        let active_field = kind::optional_str(params, "active_field")?;
        let credential = kind::optional_str(params, "credential")?;
        let pagination = match params.get("pagination").and_then(Value::as_str).unwrap_or("none") {
            "none" => Pagination::None,
            "offset" => {
                let page_size = match params.get("page_size").and_then(Value::as_u64) {
                    Some(n) if n > 0 => n as u32,
                    _ => return Err("pagination \"offset\" needs page_size > 0".into()),
                };
                if !url.contains("{offset}") {
                    return Err("pagination \"offset\" needs a literal '{offset}' placeholder in url".into());
                }
                Pagination::Offset { page_size }
            }
            other => return Err(format!("unknown pagination \"{other}\" (known: none, offset)")),
        };
        Ok(Box::new(JsonApiConfig {
            url,
            items_path,
            id_field,
            title_field,
            url_field,
            url_base,
            pagination,
            active_field,
            credential,
        }))
    }
}

#[derive(Debug)]
struct JsonApiConfig {
    url: String,
    items_path: String,
    id_field: String,
    title_field: String,
    url_field: String,
    url_base: Option<String>,
    pagination: Pagination,
    active_field: Option<String>,
    credential: Option<String>,
}

impl KindConfig for JsonApiConfig {
    fn build(&self, id: String) -> std::sync::Arc<dyn Source> {
        std::sync::Arc::new(JsonApi {
            id,
            url: self.url.clone(),
            items_path: self.items_path.clone(),
            id_field: self.id_field.clone(),
            title_field: self.title_field.clone(),
            url_field: self.url_field.clone(),
            url_base: self.url_base.clone(),
            pagination: self.pagination,
            active_field: self.active_field.clone(),
            credential: self.credential.clone(),
        })
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

        let resp = match &self.credential {
            Some(c) => http::require_ok(http::get_with_credential(ctx, &url, c).await?, &url)?,
            None => http::require_ok(http::get(ctx, &url).await?, &url)?,
        };
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
            let Some(url) = join_url(self.url_base.as_deref(), &raw_url) else { continue };
            items.push(RawItem { source_id, title, url, raw_data: item.clone() });
        }

        // Fewer than a full page means this was the last one -- the same "ask again only if
        // the last answer was full" signal a page-number scheme would use, just expressed as
        // a running offset instead.
        //
        // Measured on the **raw array**, not on `items` (#126's review, B4): items get dropped
        // for being inactive or missing a mapped field, so one closed posting on an otherwise
        // full page used to read as "short page, we're done" and silently abandon the rest of
        // a board.
        let next_cursor = page_size.filter(|&n| array.len() as u32 >= n).map(|n| (offset + n).to_string());
        Ok(Page { items, next_cursor, ..Page::default() })
    }
}

/// Joins a `url_base` to an item's own url field. String concatenation turned
/// `"https://www.amazon.jobs"` plus `"en/jobs/1"` into `"https://www.amazon.jobsen/jobs/1"`
/// (#126's review, B13), so this goes through a real URL join, which handles the missing or
/// doubled separator either way. `None` means the result isn't a usable url and the item is
/// skipped rather than emitted with a broken link. With no `url_base`, the field must already
/// be an absolute url.
fn join_url(base: Option<&str>, raw: &str) -> Option<String> {
    match base {
        None => Some(raw.to_owned()),
        Some(base) => {
            // A base without a trailing slash would otherwise have its last segment replaced
            // by `join`, turning `https://x.test/jobs` + `1` into `https://x.test/1`.
            let base = if base.ends_with('/') { base.to_owned() } else { format!("{base}/") };
            let joined = url::Url::parse(&base).ok()?.join(raw.trim_start_matches('/')).ok()?;
            Some(joined.to_string())
        }
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
    use serde_json::json;
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
        JsonApi {
            id: id.to_string(),
            url: url.to_string(),
            items_path: "jobs".to_string(),
            id_field: "id".to_string(),
            title_field: "title".to_string(),
            url_field: "absolute_url".to_string(),
            url_base: None,
            pagination: Pagination::None,
            active_field: None,
            credential: None,
        }
    }

    const GREENHOUSE_PAGE: &str = r#"{"jobs": [
        {"id": 111, "title": "Backend Engineer", "absolute_url": "https://boards.greenhouse.io/acme/jobs/111", "location": {"name": "Remote"}},
        {"id": 222, "title": "Full Stack Engineer", "absolute_url": "https://boards.greenhouse.io/acme/jobs/222", "location": {"name": "NYC"}}
    ]}"#;

    #[tokio::test]
    async fn one_dropped_item_on_a_full_page_does_not_end_pagination() {
        // #126's review, B4: "last page" was decided from the items left *after* dropping
        // inactive/thin ones, so one closed posting on an otherwise full page read as a short
        // page and silently abandoned the rest of the board.
        let env = TestEnv::new("fetchers");
        let source = JsonApi {
            id: "acme".into(),
            url: "https://acme.test/jobs?offset={offset}".into(),
            items_path: String::new(),
            id_field: "id".into(),
            title_field: "title".into(),
            url_field: "url".into(),
            url_base: None,
            pagination: Pagination::Offset { page_size: 2 },
            active_field: Some("active".into()),
            credential: None,
        };
        // A full page (2 items, page_size 2) where one is inactive: one item survives, but the
        // *page* was full, so there must be a next cursor.
        env.http.responses.lock().unwrap().insert(
            "https://acme.test/jobs?offset=0".into(),
            ok(&json!([
                {"id": "1", "title": "Closed role", "url": "https://acme.test/1", "active": false},
                {"id": "2", "title": "Open role", "url": "https://acme.test/2", "active": true},
            ])
            .to_string()),
        );

        let page = source.fetch_page(&env.ctx, None).await.unwrap();
        assert_eq!(page.items.len(), 1, "the closed posting is still dropped");
        assert_eq!(page.next_cursor.as_deref(), Some("2"), "but the full raw page must keep paging");
    }

    #[test]
    fn url_base_without_a_trailing_slash_still_makes_a_valid_url() {
        // #126's review, B13: string concatenation gave "https://www.amazon.jobsen/jobs/1".
        assert_eq!(
            join_url(Some("https://www.amazon.jobs"), "en/jobs/1").unwrap(),
            "https://www.amazon.jobs/en/jobs/1"
        );
        // A trailing slash on the base, a leading slash on the path, and both, all agree.
        assert_eq!(join_url(Some("https://x.test/"), "a/1").unwrap(), "https://x.test/a/1");
        assert_eq!(join_url(Some("https://x.test"), "/a/1").unwrap(), "https://x.test/a/1");
        assert_eq!(join_url(Some("https://x.test/"), "/a/1").unwrap(), "https://x.test/a/1");
        // A base that keeps its own path doesn't lose its last segment.
        assert_eq!(join_url(Some("https://x.test/jobs"), "1").unwrap(), "https://x.test/jobs/1");
        // No base means the field must already be absolute; a junk base is no url at all.
        assert_eq!(join_url(None, "https://x.test/a").unwrap(), "https://x.test/a");
        assert!(join_url(Some("not a url"), "a").is_none());
    }

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
        let api = JsonApi {
            id: "acme-lever".into(),
            url: "https://example.test/postings".into(),
            items_path: String::new(),
            id_field: "id".into(),
            title_field: "text".into(),
            url_field: "hostedUrl".into(),
            url_base: None,
            pagination: Pagination::None,
            active_field: None,
            credential: None,
        };

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
        let api = JsonApi {
            id: "amazon-swe".into(),
            url: "https://example.test/search.json?offset={offset}".into(),
            items_path: "jobs".into(),
            id_field: "id".into(),
            title_field: "title".into(),
            url_field: "job_path".into(),
            url_base: Some("https://example.test".into()),
            pagination: Pagination::Offset { page_size: 50 },
            active_field: None,
            credential: None,
        };

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
        let api = JsonApi {
            id: "paged".into(),
            url: "https://example.test/jobs?offset={offset}".into(),
            items_path: "jobs".into(),
            id_field: "id".into(),
            title_field: "title".into(),
            url_field: "url".into(),
            url_base: None,
            pagination: Pagination::Offset { page_size: 2 },
            active_field: None,
            credential: None,
        };

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
        let api = JsonApi {
            id: "acme-jobs".into(),
            url: "https://example.test/jobs".into(),
            items_path: "jobs".into(),
            id_field: "id".into(),
            title_field: "title".into(),
            url_field: "absolute_url".into(),
            url_base: None,
            pagination: Pagination::None,
            active_field: Some("active".into()),
            credential: None,
        };

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

    fn params(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn json_api_kind_parses_every_field_including_offset_pagination_and_credential() {
        let cfg = JsonApiKind
            .parse(&params(json!({
                "url": "https://example.test/jobs?offset={offset}",
                "items_path": "jobs", "id_field": "id", "title_field": "title", "url_field": "url",
                "url_base": "https://example.test", "pagination": "offset", "page_size": 50,
                "active_field": "active", "credential": "my-key",
            })))
            .unwrap();
        let source = cfg.build("acme".into());
        assert_eq!(source.id(), "acme");
        assert_eq!(source.method(), Method::Api);
    }

    #[test]
    fn json_api_kind_defaults_items_path_and_pagination_when_absent() {
        let cfg = JsonApiKind
            .parse(&params(
                json!({"url": "https://x.test/jobs", "id_field": "id", "title_field": "title", "url_field": "url"}),
            ))
            .unwrap();
        cfg.build("acme".into()); // doesn't panic with every optional field left out
    }

    #[test]
    fn json_api_kind_rejects_missing_required_fields_bad_pagination_and_unknown_fields() {
        let base = json!({"url": "https://x.test/jobs", "id_field": "id", "title_field": "title", "url_field": "url"});
        for missing in ["id_field", "title_field", "url_field"] {
            let mut fields = base.clone();
            fields.as_object_mut().unwrap().remove(missing);
            let err = JsonApiKind.parse(&params(fields)).unwrap_err();
            assert!(err.contains(missing), "missing {missing}: {err}");
        }

        let mut offset_no_size = base.clone();
        offset_no_size.as_object_mut().unwrap().insert("pagination".into(), json!("offset"));
        let err = JsonApiKind.parse(&params(offset_no_size)).unwrap_err();
        assert!(err.contains("page_size"), "{err}");

        let mut offset_no_placeholder = base.clone();
        offset_no_placeholder.as_object_mut().unwrap().insert("pagination".into(), json!("offset"));
        offset_no_placeholder.as_object_mut().unwrap().insert("page_size".into(), json!(50));
        let err = JsonApiKind.parse(&params(offset_no_placeholder)).unwrap_err();
        assert!(err.contains("{offset}"), "{err}");

        let mut bad_pagination = base.clone();
        bad_pagination.as_object_mut().unwrap().insert("pagination".into(), json!("cursor"));
        let err = JsonApiKind.parse(&params(bad_pagination)).unwrap_err();
        assert!(err.contains("cursor"), "{err}");

        let mut typo = base;
        typo.as_object_mut().unwrap().insert("id_fieldd".into(), json!("id"));
        let err = JsonApiKind.parse(&params(typo)).unwrap_err();
        assert!(err.contains("id_fieldd"), "{err}");
    }

    #[tokio::test]
    async fn a_configured_credential_is_attached_to_every_request() {
        let env = TestEnv::new("fetchers");
        stub(&env, "https://example.test/jobs", ok(r#"{"jobs": []}"#));
        let api = JsonApi {
            id: "acme".into(),
            url: "https://example.test/jobs".into(),
            items_path: "jobs".into(),
            id_field: "id".into(),
            title_field: "title".into(),
            url_field: "url".into(),
            url_base: None,
            pagination: Pagination::None,
            active_field: None,
            credential: Some("my-key".into()),
        };

        api.fetch_page(&env.ctx, None).await.unwrap();
        let calls = env.http.calls.lock().unwrap();
        assert_eq!(calls.last(), Some(&("https://example.test/jobs".to_string(), Some("my-key".to_string()))));
    }
}
