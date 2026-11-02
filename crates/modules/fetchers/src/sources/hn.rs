//! Hacker News "Who is hiring?" via its official, public, unauthenticated Firebase API
//! (ADR 0028 §10) -- no robots.txt applies; this is an API host, not a crawled site, and
//! the API is explicitly meant for third-party clients.
//!
//! Flow: `user/whoishiring.json` for the submitted story ids (newest first) -> the latest
//! one's `item/<id>.json` for its `kids` (top-level comments, each a job posting) -> one
//! `item/<id>.json` fetch per comment. Each `fetch_page` call re-fetches the thread to get
//! `kids` again rather than caching it itself (the trait is stateless across calls, see
//! [`crate::source::Source`]) -- cheap after the first call, since `ctx.http`'s own gateway
//! cache (ADR 0027) serves the repeat within its TTL.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use shimmer_core::{Ctx, Error, ErrorCode, Result};

use crate::http;
use crate::source::{Method, Page, RawItem, Source};

const BASE: &str = "https://hacker-news.firebaseio.com/v0";
/// Genuinely-new (not-already-seen) comment items fetched per `fetch_page` call -- already-
/// seen kids are skipped for free and don't count against this (see `fetch_page`'s own doc
/// comment). Deliberately modest -- the per-run new-item cap (`dedup::MAX_NEW_PER_RUN`,
/// enforced by the orchestration that drives this trait) is what actually bounds a whole run;
/// this just keeps one page's own fan-out small.
const PAGE_SIZE: usize = 20;
/// How many of the newest `submitted` entries [`latest_thread`] is willing to check before
/// giving up -- the sibling "who is hiring" / "who wants to be hired" / "freelancer" threads
/// are always posted within a few submissions of each other, so this is a generous bound, not
/// a tight one, and keeps the lookup from walking a user's entire post history.
const MAX_THREAD_CANDIDATES: usize = 5;
/// Every "who is hiring" meta-thread's title starts with exactly this (verified against HN's
/// own archive) -- distinguishes it from the "who wants to be hired?" and "freelancer?"
/// threads `user/whoishiring.json` also lists, often right next to it (PR #126 review).
const WHO_IS_HIRING_PREFIX: &str = "Ask HN: Who is hiring?";

pub struct HnWhoIsHiring {
    id: String,
}

impl HnWhoIsHiring {
    pub fn new(id: String) -> Self {
        Self { id }
    }
}

/// The `hn-whoishiring` kind (ADR 0028 §10): no fields of its own, same posture as this
/// source itself -- it needs nothing beyond `enabled`/`interval`/`filter`, every source's own
/// envelope fields.
pub struct HnKind;

impl crate::kind::Kind for HnKind {
    fn name(&self) -> &'static str {
        "hn-whoishiring"
    }

    fn method(&self) -> Method {
        Method::Api
    }

    fn parse(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> std::result::Result<Box<dyn crate::kind::KindConfig>, String> {
        crate::kind::reject_unknown(params, &[])?;
        Ok(Box::new(HnConfig))
    }
}

#[derive(Debug)]
struct HnConfig;

impl crate::kind::KindConfig for HnConfig {
    fn build(&self, id: String) -> std::sync::Arc<dyn Source> {
        std::sync::Arc::new(HnWhoIsHiring::new(id))
    }
}

#[derive(Deserialize)]
struct UserItem {
    #[serde(default)]
    submitted: Vec<u64>,
}

#[derive(Deserialize)]
struct HnItem {
    id: u64,
    #[serde(default)]
    kids: Vec<u64>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    by: Option<String>,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    dead: bool,
    /// Only populated on a `story` item (a thread, not a comment) -- used solely by
    /// [`latest_thread`] to tell the three sibling "who is hiring" meta-threads apart.
    #[serde(default)]
    title: Option<String>,
}

#[async_trait]
impl Source for HnWhoIsHiring {
    fn id(&self) -> &str {
        &self.id
    }

    fn method(&self) -> Method {
        Method::Api
    }

    /// A page's "offset" (encoded in `next_cursor`) is a position in `thread.kids`, not a
    /// count of items returned -- it advances past every kid *examined*, whether that kid was
    /// skipped (already in this source's own `SeenSet`, so dedup would have discarded it
    /// anyway) or genuinely fetched. Skipping costs no HTTP call at all, so a long-since-seen
    /// run of early comments is free to walk past; only a kid not yet in the seen set counts
    /// against [`PAGE_SIZE`]. This is what lets a run eventually reach comment 500+ of a big
    /// thread instead of being stuck re-examining the same ~200 oldest comments every time
    /// (PR #126 review) -- the old behaviour advanced the cursor by a blind `+PAGE_SIZE`
    /// regardless of how many of those were already seen.
    async fn fetch_page(&self, ctx: &Ctx, cursor: Option<&str>) -> Result<Page> {
        let (thread_id, offset) = match cursor {
            Some(c) => parse_cursor(c)?,
            None => (latest_thread(ctx).await?, 0),
        };
        let thread_url = format!("{BASE}/item/{thread_id}.json");
        let thread = item(ctx, thread_id).await?.ok_or_else(|| {
            Error::module_error(format!("parse_empty: {thread_url}: thread was purged, nothing to page through"))
        })?;
        let kids = thread.kids;

        // A read-only peek at this source's own dedup state -- never touched or written here,
        // that stays `module.rs`'s job in its one transaction (ADR 0028 §4). Knowing it lets
        // this loop skip a seen kid without ever fetching its details.
        let seen = crate::dedup::load(ctx, self.id())?;

        let mut items = Vec::new();
        let mut examined_only = Vec::new();
        let mut idx = offset.min(kids.len());
        let mut examined = 0usize;
        while idx < kids.len() && examined < PAGE_SIZE {
            if ctx.cancel.is_cancelled() {
                return Err(Error::new(ErrorCode::ModuleError, "cancelled"));
            }
            let kid = kids[idx];
            idx += 1;
            if !seen.is_new(&kid.to_string()) {
                continue; // already seen -- no HTTP call, and doesn't use up this page's budget
            }
            examined += 1;
            // A kid that is examined and yields nothing (purged, deleted, dead, or text-less)
            // is reported as `examined` so the orchestration marks it seen (#126's review,
            // B6). Without that it was re-downloaded on every single run, forever, and ate
            // into this page's budget each time as the thread aged.
            let Some(comment) = item(ctx, kid).await? else {
                examined_only.push(kid.to_string());
                continue;
            };
            if comment.deleted || comment.dead {
                examined_only.push(comment.id.to_string());
                continue;
            }
            let Some(text) = comment.text else {
                examined_only.push(comment.id.to_string());
                continue;
            };
            items.push(RawItem {
                source_id: comment.id.to_string(),
                title: summarize(&text),
                url: format!("https://news.ycombinator.com/item?id={}", comment.id),
                raw_data: json!({"by": comment.by, "text": text}),
            });
        }
        let next_cursor = (idx < kids.len()).then(|| format!("{thread_id}:{idx}"));
        Ok(Page { items, next_cursor, examined_only })
    }
}

/// Picks the real "who is hiring?" thread out of `user/whoishiring.json`'s `submitted` list,
/// which also contains the "who wants to be hired?" and "freelancer?" sibling threads posted
/// around the same time -- `submitted.first()` alone can silently pick either of those
/// instead (PR #126 review).
async fn latest_thread(ctx: &Ctx) -> Result<u64> {
    let url = format!("{BASE}/user/whoishiring.json");
    let resp = http::require_ok(http::get(ctx, &url).await?, &url)?;
    let user: UserItem = parse_json(&resp.body, &url)?;
    for &id in user.submitted.iter().take(MAX_THREAD_CANDIDATES) {
        if let Some(thread) = item(ctx, id).await? {
            if thread.title.as_deref().is_some_and(|t| t.starts_with(WHO_IS_HIRING_PREFIX)) {
                return Ok(id);
            }
        }
    }
    Err(Error::module_error(format!(
        "parse_empty: {url}: no thread titled \"{WHO_IS_HIRING_PREFIX}\" found among the newest \
         {MAX_THREAD_CANDIDATES} submitted threads"
    )))
}

/// `Ok(None)` for a purged item -- HN's API returns the literal JSON value `null` (not a
/// 404) for one, and that must not fail an entire page over a single removed comment (PR
/// #126 review). Checked as a generic [`serde_json::Value`] first, rather than attempting
/// `HnItem` directly and treating any deserialize error as "it was null," so a response that
/// is malformed in some other way still surfaces as the `parse_empty` it actually is.
async fn item(ctx: &Ctx, id: u64) -> Result<Option<HnItem>> {
    let url = format!("{BASE}/item/{id}.json");
    let resp = http::require_ok(http::get(ctx, &url).await?, &url)?;
    let value: serde_json::Value = parse_json(&resp.body, &url)?;
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value).map(Some).map_err(|e| Error::module_error(format!("parse_empty: {url}: {e}")))
}

fn parse_json<T: for<'de> Deserialize<'de>>(body: &[u8], url: &str) -> Result<T> {
    serde_json::from_slice(body).map_err(|e| Error::module_error(format!("parse_empty: {url}: {e}")))
}

fn parse_cursor(c: &str) -> Result<(u64, usize)> {
    let bad = || Error::invalid_params(format!("bad cursor '{c}'"));
    let (thread, offset) = c.split_once(':').ok_or_else(bad)?;
    Ok((thread.parse().map_err(|_| bad())?, offset.parse().map_err(|_| bad())?))
}

/// A short, human-readable label for a job posting comment, since HN comments carry no
/// title of their own -- the first non-empty line of its text, HTML-unescaped and stripped
/// of the `<p>` paragraph breaks the API's text field uses in place of real markup,
/// truncated to a sane length.
fn summarize(text: &str) -> String {
    // HN's `text` is HTML: `<p>` for paragraph breaks, real tags like `<a href=...>`, and every
    // entity form (`&amp;`, `&#x27;`, `&quot;`, `&#x2F;`). Hand-decoding four of them left
    // `&#x27;Remote&#x27;` in titles, which is both ugly and defeats the title filter, since
    // `--contains remote` no longer matches the stored text (#126's review, B15). `scraper` is
    // already a dependency, so parse it as the HTML it is and take the text.
    let with_breaks = text.replace("<p>", "\n");
    let fragment = scraper::Html::parse_fragment(&with_breaks);
    let decoded: String = fragment.root_element().text().collect();
    let first_line = decoded.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let truncated: String = first_line.chars().take(120).collect();
    if truncated.is_empty() {
        "(untitled)".to_owned()
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use shimmer_core::testing::TestEnv;
    use shimmer_core::{ErrorCode, HttpResponse};

    use super::*;

    const USER: &str = include_str!("../../tests/fixtures/hn/user_whoishiring.json");
    const USER_EMPTY: &str = include_str!("../../tests/fixtures/hn/user_whoishiring_empty.json");
    const THREAD: &str = include_str!("../../tests/fixtures/hn/thread_45000001.json");
    const COMMENT_10: &str = include_str!("../../tests/fixtures/hn/comment_45000010.json");
    const COMMENT_11_DELETED: &str = include_str!("../../tests/fixtures/hn/comment_45000011.json");
    const COMMENT_12: &str = include_str!("../../tests/fixtures/hn/comment_45000012.json");
    const MALFORMED: &str = include_str!("../../tests/fixtures/hn/malformed.json");

    fn ok(body: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: body.as_bytes().to_vec(),
            from_cache: false,
            stale: false,
            retry_after_secs: None,
        }
    }

    fn stub(env: &TestEnv, path: &str, resp: HttpResponse) {
        env.http.responses.lock().unwrap().insert(format!("{BASE}{path}"), resp);
    }

    fn stub_full_thread(env: &TestEnv) {
        stub(env, "/user/whoishiring.json", ok(USER));
        stub(env, "/item/45000001.json", ok(THREAD));
        stub(env, "/item/45000010.json", ok(COMMENT_10));
        stub(env, "/item/45000011.json", ok(COMMENT_11_DELETED));
        stub(env, "/item/45000012.json", ok(COMMENT_12));
    }

    #[tokio::test]
    async fn the_first_page_skips_deleted_comments_and_covers_the_whole_short_thread() {
        let env = TestEnv::new("fetchers");
        stub_full_thread(&env);

        let page = HnWhoIsHiring::new("hn-whoishiring".into()).fetch_page(&env.ctx, None).await.unwrap();
        // 3 kids, well under PAGE_SIZE, so this is the whole thread in one page: 10 and 12
        // are valid, 11 is deleted and must not become an item.
        assert_eq!(page.items.len(), 2, "45000011 is deleted and must not become an item");
        assert_eq!(page.items[0].source_id, "45000010");
        assert_eq!(page.items[0].url, "https://news.ycombinator.com/item?id=45000010");
        assert!(page.items[0].title.contains("Acme Corp"), "{}", page.items[0].title);
        assert_eq!(page.items[1].source_id, "45000012");
        assert!(page.next_cursor.is_none(), "the whole thread fit in one page");
    }

    #[test]
    fn id_is_whatever_it_was_constructed_with_and_method_is_api() {
        let hn = HnWhoIsHiring::new("my-hn".into());
        assert_eq!(hn.id(), "my-hn");
        assert_eq!(hn.method(), Method::Api);
    }

    #[test]
    fn hn_kind_takes_no_fields_of_its_own() {
        use crate::kind::Kind;
        assert_eq!(HnKind.name(), "hn-whoishiring");
        assert_eq!(HnKind.method(), Method::Api);
        assert!(HnKind.parse(&serde_json::Map::new()).is_ok());
        let mut extra = serde_json::Map::new();
        extra.insert("nope".into(), json!(true));
        assert!(HnKind.parse(&extra).is_err());
    }

    #[tokio::test]
    async fn a_resumed_cursor_does_not_re_fetch_the_latest_thread_lookup() {
        let env = TestEnv::new("fetchers");
        // Deliberately NOT stubbing /user/whoishiring.json: if the resumed-cursor path tried
        // to look the latest thread up again, this call would fail with "no stub for...".
        stub(&env, "/item/45000001.json", ok(THREAD));
        stub(&env, "/item/45000012.json", ok(COMMENT_12));

        let page = HnWhoIsHiring::new("hn-whoishiring".into()).fetch_page(&env.ctx, Some("45000001:2")).await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].source_id, "45000012");
        assert!(page.next_cursor.is_none(), "every kid has now been paged through");
    }

    #[tokio::test]
    async fn no_stub_at_all_is_a_transport_failure_not_a_module_error() {
        let env = TestEnv::new("fetchers"); // nothing stubbed
        let err = HnWhoIsHiring::new("hn-whoishiring".into()).fetch_page(&env.ctx, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Unavailable);
    }

    #[tokio::test]
    async fn a_404_response_is_a_classifiable_http_error() {
        let env = TestEnv::new("fetchers");
        stub(
            &env,
            "/user/whoishiring.json",
            HttpResponse { status: 404, body: Vec::new(), from_cache: false, stale: false, retry_after_secs: None },
        );
        let err = HnWhoIsHiring::new("hn-whoishiring".into()).fetch_page(&env.ctx, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
        assert!(err.message.starts_with("http_error:"), "{}", err.message);
    }

    #[tokio::test]
    async fn malformed_json_is_a_classifiable_parse_failure() {
        let env = TestEnv::new("fetchers");
        stub(&env, "/user/whoishiring.json", ok(MALFORMED));
        let err = HnWhoIsHiring::new("hn-whoishiring".into()).fetch_page(&env.ctx, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
        assert!(err.message.starts_with("parse_empty:"), "{}", err.message);
    }

    #[tokio::test]
    async fn no_submitted_threads_at_all_is_also_a_classifiable_parse_failure() {
        let env = TestEnv::new("fetchers");
        stub(&env, "/user/whoishiring.json", ok(USER_EMPTY));
        let err = HnWhoIsHiring::new("hn-whoishiring".into()).fetch_page(&env.ctx, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
        assert!(err.message.starts_with("parse_empty:"), "{}", err.message);
    }

    #[tokio::test]
    async fn a_comment_fetched_and_found_empty_is_reported_so_it_is_never_fetched_again() {
        // #126's review, B6: a deleted/dead/text-less comment was downloaded, dropped, and
        // never marked seen -- so it was downloaded again on every single run, forever, eating
        // into the page's budget each time as the thread aged.
        let env = TestEnv::new("fetchers");
        stub_full_thread(&env);

        let page = HnWhoIsHiring::new("hn-whoishiring".into()).fetch_page(&env.ctx, None).await.unwrap();
        assert!(
            page.examined_only.contains(&"45000011".to_string()),
            "the deleted comment must be reported as examined: {:?}",
            page.examined_only
        );
        // And it is not smuggled in as an item.
        assert!(page.items.iter().all(|i| i.source_id != "45000011"));
    }

    #[test]
    fn summarize_decodes_every_entity_form_and_drops_tags() {
        // #126's review, B15: only &amp; &gt; &lt; and <p> were handled, so HN titles kept
        // `&#x27;`, `&quot;`, `&#x2F;` and raw <a href=...> markup -- ugly, and it broke the
        // title filter, since `--contains remote` no longer matched `&#x27;Remote&#x27;`.
        assert_eq!(summarize("<p>&#x27;Remote&#x27; Rust role"), "'Remote' Rust role");
        assert_eq!(summarize("<p>Acme &amp; Co &quot;hiring&quot;"), "Acme & Co \"hiring\"");
        assert_eq!(summarize("<p>Backend&#x2F;Infra"), "Backend/Infra");
        assert_eq!(summarize("<p>See <a href=\"https://x.test\">our site</a> now"), "See our site now");
    }

    #[test]
    fn summarize_strips_markup_and_truncates() {
        assert_eq!(summarize("<p>Acme Corp | Remote<p>We build things."), "Acme Corp | Remote");
        assert_eq!(summarize(""), "(untitled)");
        let long = "x".repeat(500);
        assert_eq!(summarize(&long).chars().count(), 120);
    }

    #[tokio::test]
    async fn latest_thread_skips_a_sibling_thread_and_picks_the_real_hiring_one() {
        let env = TestEnv::new("fetchers");
        // submitted[0] is the "who wants to be hired?" sibling thread, posted around the same
        // time as the real one -- a plain `submitted.first()` would pick this one instead.
        stub(&env, "/user/whoishiring.json", ok(&json!({"submitted": [45000099, 45000001]}).to_string()));
        stub(
            &env,
            "/item/45000099.json",
            ok(&json!({
                "id": 45000099, "type": "story",
                "title": "Ask HN: Who wants to be hired? (October 2026)", "kids": [],
            })
            .to_string()),
        );
        stub(&env, "/item/45000001.json", ok(THREAD));
        stub(&env, "/item/45000010.json", ok(COMMENT_10));
        stub(&env, "/item/45000011.json", ok(COMMENT_11_DELETED));
        stub(&env, "/item/45000012.json", ok(COMMENT_12));

        let page = HnWhoIsHiring::new("hn-whoishiring".into()).fetch_page(&env.ctx, None).await.unwrap();
        assert_eq!(page.items.len(), 2, "must have picked 45000001, the real hiring thread, not the sibling");
    }

    #[tokio::test]
    async fn none_of_the_checked_candidates_matching_is_a_classifiable_parse_failure() {
        let env = TestEnv::new("fetchers");
        stub(&env, "/user/whoishiring.json", ok(&json!({"submitted": [45000099]}).to_string()));
        stub(
            &env,
            "/item/45000099.json",
            ok(&json!({"id": 45000099, "type": "story", "title": "Ask HN: Freelancer? Seeking freelancer?", "kids": []})
                .to_string()),
        );
        let err = HnWhoIsHiring::new("hn-whoishiring".into()).fetch_page(&env.ctx, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
        assert!(err.message.starts_with("parse_empty:"), "{}", err.message);
    }

    #[tokio::test]
    async fn already_seen_kids_are_skipped_without_fetching_their_details() {
        let env = TestEnv::new("fetchers");
        stub(&env, "/user/whoishiring.json", ok(USER));
        stub(&env, "/item/45000001.json", ok(THREAD));
        // 45000010 and 45000011 are already marked seen by a previous run -- deliberately NOT
        // stubbing either: if the implementation fetched their details anyway, this would
        // fail with "no stub for...".
        let mut seen = crate::dedup::SeenSet::default();
        seen.touch("45000010", env.ctx.clock.now());
        seen.touch("45000011", env.ctx.clock.now());
        env.ctx.store.write(&crate::dedup::path("hn-whoishiring"), seen.to_toml()).unwrap();
        stub(&env, "/item/45000012.json", ok(COMMENT_12));

        let page = HnWhoIsHiring::new("hn-whoishiring".into()).fetch_page(&env.ctx, None).await.unwrap();
        assert_eq!(page.items.len(), 1, "only the not-yet-seen kid must have been fetched");
        assert_eq!(page.items[0].source_id, "45000012");
        assert!(page.next_cursor.is_none(), "every kid, seen or not, has now been examined");
    }

    #[tokio::test]
    async fn a_purged_comment_returning_null_is_skipped_not_a_run_failure() {
        let env = TestEnv::new("fetchers");
        stub(&env, "/user/whoishiring.json", ok(USER));
        stub(&env, "/item/45000001.json", ok(THREAD));
        stub(&env, "/item/45000010.json", ok("null"));
        stub(&env, "/item/45000011.json", ok(COMMENT_11_DELETED));
        stub(&env, "/item/45000012.json", ok(COMMENT_12));

        let page = HnWhoIsHiring::new("hn-whoishiring".into()).fetch_page(&env.ctx, None).await.unwrap();
        assert_eq!(
            page.items.len(),
            1,
            "the purged (null) and deleted comments must both be skipped, not fail the page"
        );
        assert_eq!(page.items[0].source_id, "45000012");
    }

    #[tokio::test]
    async fn a_null_thread_item_is_a_hard_error_not_a_silently_empty_page() {
        let env = TestEnv::new("fetchers");
        stub(&env, "/item/45000001.json", ok("null"));
        let err =
            HnWhoIsHiring::new("hn-whoishiring".into()).fetch_page(&env.ctx, Some("45000001:0")).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
        assert!(err.message.starts_with("parse_empty:"), "{}", err.message);
    }

    #[tokio::test]
    async fn item_returns_none_for_a_purged_items_null_body() {
        let env = TestEnv::new("fetchers");
        stub(&env, "/item/999.json", ok("null"));
        assert!(item(&env.ctx, 999).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn fetch_page_bails_as_soon_as_it_sees_cancellation() {
        let env = TestEnv::new("fetchers");
        stub_full_thread(&env);
        env.ctx.cancel.cancel();

        let err = HnWhoIsHiring::new("hn-whoishiring".into()).fetch_page(&env.ctx, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
        assert!(err.message.contains("cancelled"), "{}", err.message);
    }
}
