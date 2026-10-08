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
//! Pagination walks the site's own `page=` query parameter for the one fixed search term
//! below -- broad enough to surface "software developer" and "full-stack" postings too,
//! confirmed live. The results container (`#results-list-content`) is present even past the
//! last page (confirmed live against `page=50` on a 3-page result set), so a page with zero
//! listings ends pagination cleanly; only a missing container is treated as a structure
//! change (ADR 0028 §7).

use async_trait::async_trait;
use serde_json::json;
use shimmer_core::{Ctx, Error, Result};

use crate::source::{Method, Page, RawItem, Source};
use crate::{html, http, robots};

const BASE: &str = "https://www.jobbank.gc.ca";
const SEARCH_TERM: &str = "software+engineer";

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
        let url = format!("{BASE}/jobsearch/jobsearch?searchstring={SEARCH_TERM}&locationstring=Canada&page={page}");

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
    use shimmer_core::{ErrorCode, HttpResponse};

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

    fn url_for_page(page: u32) -> String {
        format!("{BASE}/jobsearch/jobsearch?searchstring={SEARCH_TERM}&locationstring=Canada&page={page}")
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
