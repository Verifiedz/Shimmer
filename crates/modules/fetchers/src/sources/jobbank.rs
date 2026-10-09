//! Job Bank Canada (jobbank.gc.ca), scraped (ADR 0028 §10) -- the federal government's job
//! board, covering the whole country rather than remote-only roles (companion to
//! `weworkremotely`, which is remote-only). Verified live on 2026-10-08: `robots.txt` is
//! `User-agent: *` / `Crawl-delay: 5`, no `Disallow` at all; no login wall on search
//! results; selectors below captured against the real
//! `/jobsearch/jobsearch?searchstring=software+engineer&locationstring=Canada` page that
//! day (see `tests/fixtures/jobbank/page1.html` for a trimmed snapshot). A listing's own
//! link carries a session id (`;jsessionid=...`) that changes on every fetch, so the dedup
//! key and canonical URL are built from the listing's numeric posting id instead (the
//! `article` element's own `id` attribute), confirmed live to resolve on its own with no
//! session id needed.
//!
//! Pagination walks the site's own `page=` query parameter for one search term -- the
//! configured `search_term` (ADR 0028 §16), or `DEFAULT_SEARCH_TERM` when none is set. The
//! default is broad enough to surface "software developer" and "full-stack" postings too,
//! confirmed live. The results container (`#results-list-content`) is present even past the
//! last page (confirmed live against `page=50` on a 3-page result set), so a page with zero
//! listings ends pagination cleanly; only a missing container is treated as a structure
//! change (ADR 0028 §7).

use async_trait::async_trait;
use serde_json::json;
use shimmer_core::{Ctx, Error, Result};

use crate::schedule::FetchersConfig;
use crate::source::{Method, Page, RawItem, Source};
use crate::{html, http, robots};

const BASE: &str = "https://www.jobbank.gc.ca";
/// Used when `[modules.fetchers.sources.jobbank-canada]` sets no `search_term` of its own
/// (ADR 0028 §16) -- the only query this source ever ran before that became configurable.
const DEFAULT_SEARCH_TERM: &str = "software engineer";

pub struct JobBankCanada;

#[async_trait]
impl Source for JobBankCanada {
    fn id(&self) -> &str {
        "jobbank-canada"
    }

    fn method(&self) -> Method {
        Method::Scrape
    }

    async fn fetch_page(&self, ctx: &Ctx, cursor: Option<&str>) -> Result<Page> {
        let page = match cursor {
            Some(c) => c.parse::<u32>().map_err(|_| Error::invalid_params(format!("bad cursor '{c}'")))?,
            None => 1,
        };
        // Blank-after-trim (not just absent) also means "use the default" -- an empty
        // search_term = "" in config.toml is as meaningless as no search_term key at all,
        // and must not become a literal empty query to the real site.
        let configured = FetchersConfig::load(ctx)?.sources.get(self.id()).and_then(|c| c.search_term.clone());
        let search_term = match configured.as_deref().map(str::trim) {
            Some(t) if !t.is_empty() => t,
            _ => DEFAULT_SEARCH_TERM,
        };
        // Built with `url::Url`'s own query encoder, not a hand-formatted string, so a
        // configured term with spaces or punctuation is encoded correctly rather than
        // producing a malformed request the way naive string interpolation would once the
        // term stopped being a fixed, pre-encoded literal.
        let mut url = url::Url::parse(&format!("{BASE}/jobsearch/jobsearch")).expect("a fixed, valid base url");
        url.query_pairs_mut()
            .append_pair("searchstring", search_term)
            .append_pair("locationstring", "Canada")
            .append_pair("page", &page.to_string());
        let url = url.to_string();

        // Method::Scrape's own obligation (ADR 0028 §7): the host has no way to know this
        // URL ahead of time to check it centrally, so the source checks it itself.
        if !robots::allowed(ctx, &url).await? {
            return Err(Error::module_error(format!("robots.txt disallows fetching {url}")));
        }

        let resp = http::require_ok(http::get(ctx, &url).await?, &url)?;
        let doc = html::parse(&resp.body);
        // Present on every page, even past the last one -- so this only fails on a real
        // redesign, never on legitimately running out of results.
        let container = html::select_or_fail(&doc, "#results-list-content", "job search results")?[0];
        let listings = html::select_all(container, "article[id^='article-']")?;

        let mut items = Vec::new();
        for listing in listings {
            let Some(id) = listing.value().attr("id").and_then(|id| id.strip_prefix("article-")) else { continue };
            let Some(title) = html::text_of(listing, ".noctitle")? else { continue };
            let company = html::text_of(listing, ".business")?;
            let location = html::text_of(listing, ".location")?.map(|l| collapse(strip_label(&l, "Location")));
            let label = match &company {
                Some(c) => format!("{c} -- {title}"),
                None => title.clone(),
            };
            items.push(RawItem {
                source_id: id.to_owned(),
                title: label,
                url: format!("{BASE}/jobsearch/jobposting/{id}"),
                raw_data: json!({"title": title, "company": company, "location": location}),
            });
        }

        let next_cursor = (!items.is_empty()).then(|| (page + 1).to_string());
        Ok(Page { items, next_cursor })
    }
}

/// `text_of`'s text pulls in a visually-hidden label span (e.g. `<span class="wb-inv">
/// Location</span>`) along with the real value -- harmless for a sighted user, noise for us.
/// `text_of` already trims outer whitespace, so the label is the literal start of the string.
fn strip_label<'a>(text: &'a str, label: &str) -> &'a str {
    text.strip_prefix(label).unwrap_or(text)
}

/// Collapses the page's own indentation whitespace between a label and its value down to
/// single spaces, e.g. "\n\t\tOttawa (ON)\n\t" -> "Ottawa (ON)".
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use shimmer_core::testing::TestEnv;
    use shimmer_core::{ErrorCode, HttpResponse, ModuleConfig};

    use super::*;

    const PAGE1: &str = include_str!("../../tests/fixtures/jobbank/page1.html");
    const EMPTY_PAGE: &str = include_str!("../../tests/fixtures/jobbank/empty_page.html");
    const EMPTY_SHELL: &str = include_str!("../../tests/fixtures/jobbank/empty_shell.html");

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

    fn allow_robots(env: &TestEnv) {
        stub(env, &format!("{BASE}/robots.txt"), ok("User-agent: *\nCrawl-delay: 5\n"));
    }

    /// Built the same way the source itself builds a request url -- via `url::Url`'s own
    /// query encoder, not a hand-formatted string -- so a test asserting on an exact url
    /// can't drift from how the source actually encodes one.
    fn url_for_search(term: &str, page: u32) -> String {
        let mut url = url::Url::parse(&format!("{BASE}/jobsearch/jobsearch")).unwrap();
        url.query_pairs_mut()
            .append_pair("searchstring", term)
            .append_pair("locationstring", "Canada")
            .append_pair("page", &page.to_string());
        url.to_string()
    }

    fn url_for_page(page: u32) -> String {
        url_for_search(DEFAULT_SEARCH_TERM, page)
    }

    #[test]
    fn id_is_jobbank_canada_and_method_is_scrape() {
        assert_eq!(JobBankCanada.id(), "jobbank-canada");
        assert_eq!(JobBankCanada.method(), Method::Scrape);
    }

    #[tokio::test]
    async fn the_first_page_parses_listings_and_strips_the_session_id_from_the_url() {
        let env = TestEnv::new("fetchers");
        allow_robots(&env);
        stub(&env, &url_for_page(1), ok(PAGE1));

        let page = JobBankCanada.fetch_page(&env.ctx, None).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].source_id, "50363596");
        assert_eq!(page.items[0].url, "https://www.jobbank.gc.ca/jobsearch/jobposting/50363596");
        assert_eq!(page.items[0].title, "ADECCO -- software engineer");
        assert_eq!(page.items[0].raw_data["location"], "Ottawa (ON)");
        assert_eq!(page.items[1].source_id, "50444471");
        assert_eq!(page.next_cursor, Some("2".to_string()));
    }

    #[tokio::test]
    async fn a_page_past_the_last_one_ends_pagination_without_failing() {
        let env = TestEnv::new("fetchers");
        allow_robots(&env);
        stub(&env, &url_for_page(1), ok(EMPTY_PAGE));

        let page = JobBankCanada.fetch_page(&env.ctx, None).await.unwrap();
        assert_eq!(page.items.len(), 0);
        assert!(page.next_cursor.is_none(), "an empty page is the end, not a reason to keep paging");
    }

    #[tokio::test]
    async fn the_cursor_is_the_next_page_number() {
        let env = TestEnv::new("fetchers");
        allow_robots(&env);
        stub(&env, &url_for_page(3), ok(EMPTY_PAGE));

        let page = JobBankCanada.fetch_page(&env.ctx, Some("3")).await.unwrap();
        assert_eq!(page.items.len(), 0);
    }

    #[tokio::test]
    async fn a_configured_search_term_replaces_the_default_and_is_url_encoded() {
        let mut env = TestEnv::new("fetchers");
        env.ctx.config = ModuleConfig::new(json!({
            "sources": {"jobbank-canada": {"enabled": false, "interval": "daily", "search_term": "junior software developer"}}
        }));
        allow_robots(&env);
        stub(&env, &url_for_search("junior software developer", 1), ok(EMPTY_PAGE));

        // No stub at the default-term url: if the configured term were ignored, this would
        // fail with "no stub for...", not the empty-page success this asserts.
        let page = JobBankCanada.fetch_page(&env.ctx, None).await.unwrap();
        assert_eq!(page.items.len(), 0);
    }

    #[tokio::test]
    async fn a_blank_configured_search_term_falls_back_to_the_default() {
        let mut env = TestEnv::new("fetchers");
        env.ctx.config = ModuleConfig::new(json!({
            "sources": {"jobbank-canada": {"enabled": false, "interval": "daily", "search_term": "   "}}
        }));
        allow_robots(&env);
        stub(&env, &url_for_page(1), ok(PAGE1));

        let page = JobBankCanada.fetch_page(&env.ctx, None).await.unwrap();
        assert_eq!(page.items.len(), 2, "a blank search_term must behave exactly like none at all");
    }

    #[tokio::test]
    async fn a_robots_disallow_is_a_hard_refusal_not_a_fetch_attempt() {
        let env = TestEnv::new("fetchers");
        stub(&env, &format!("{BASE}/robots.txt"), ok("User-agent: *\nDisallow: /jobsearch/\n"));
        // Deliberately no stub for the search page itself -- if the refusal were downgraded
        // to a warning and fetched anyway, this would fail with "no stub for...".
        let err = JobBankCanada.fetch_page(&env.ctx, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
        assert!(err.message.contains("robots.txt"), "{}", err.message);
    }

    #[tokio::test]
    async fn a_page_whose_structure_no_longer_matches_is_a_reported_failure() {
        let env = TestEnv::new("fetchers");
        allow_robots(&env);
        stub(&env, &url_for_page(1), ok(EMPTY_SHELL));

        let err = JobBankCanada.fetch_page(&env.ctx, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
        assert!(err.message.contains("job search results"), "{}", err.message);
    }

    #[test]
    fn strip_label_removes_only_the_literal_prefix() {
        assert_eq!(strip_label("LocationOttawa (ON)", "Location"), "Ottawa (ON)");
        assert_eq!(strip_label("Ottawa (ON)", "Location"), "Ottawa (ON)");
    }

    #[test]
    fn collapse_squashes_internal_whitespace_to_single_spaces() {
        assert_eq!(collapse("\n\t\tOttawa (ON)\n\t\t\t"), "Ottawa (ON)");
    }
}
