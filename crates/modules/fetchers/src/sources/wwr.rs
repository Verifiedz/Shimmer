//! We Work Remotely, scraped (ADR 0028 §10) -- static, server-rendered category pages, no
//! JS needed. Verified live on 2026-10-07: `robots.txt` is `Allow: /`, disallowing only
//! `/admin/`, `/account/`, `/job-seekers/.../profile`, `/manage-company/` -- none of which
//! this touches; no login wall on listings; selectors below captured against the real
//! `/categories/remote-full-stack-programming-jobs` page that day (see
//! `tests/fixtures/wwr/full_stack.html` for a trimmed snapshot).
//!
//! Pagination walks a fixed, software-engineering-focused rotation of the site's own
//! category slugs -- there is no `?page=` on a category page (a category lists everything
//! in one page), so a page number would be invented, not real. Re-check these slugs
//! occasionally: WWR has renamed or merged categories before.

use async_trait::async_trait;
use serde_json::json;
use shimmer_core::{Ctx, Error, Result};

use crate::source::{Method, Page, RawItem, Source};
use crate::{html, http, robots};

const BASE: &str = "https://weworkremotely.com";

const CATEGORIES: &[&str] = &[
    "remote-full-stack-programming-jobs",
    "remote-front-end-programming-jobs",
    "remote-back-end-programming-jobs",
    "remote-devops-sysadmin-jobs",
];

pub struct WeWorkRemotely;

#[async_trait]
impl Source for WeWorkRemotely {
    fn id(&self) -> &str {
        "weworkremotely"
    }

    fn method(&self) -> Method {
        Method::Scrape
    }

    async fn fetch_page(&self, ctx: &Ctx, cursor: Option<&str>) -> Result<Page> {
        let index = match cursor {
            Some(c) => c.parse::<usize>().map_err(|_| Error::invalid_params(format!("bad cursor '{c}'")))?,
            None => 0,
        };
        let Some(&category) = CATEGORIES.get(index) else {
            return Ok(Page::default());
        };
        let url = format!("{BASE}/categories/{category}");

        // Method::Scrape's own obligation (ADR 0028 §7): the host has no way to know this
        // URL ahead of time to check it centrally, so the source checks it itself.
        if !robots::allowed(ctx, &url).await? {
            return Err(Error::module_error(format!("robots.txt disallows fetching {url}")));
        }

        let resp = http::require_ok(http::get(ctx, &url).await?, &url)?;
        let doc = html::parse(&resp.body);
        let listings = html::select_or_fail(&doc, "li.new-listing-container", "job listings")?;

        let mut items = Vec::new();
        for listing in listings {
            let Some(link) = html::select_one(listing, "a.listing-link--unlocked")? else { continue };
            let Some(href) = link.value().attr("href") else { continue };
            let Some(title) = html::text_of(listing, ".new-listing__header__title__text")? else { continue };
            let company = html::text_of(listing, ".new-listing__company-name")?;
            let label = match &company {
                Some(c) => format!("{c} -- {title}"),
                None => title.clone(),
            };
            items.push(RawItem {
                source_id: href.to_owned(),
                title: label,
                url: format!("{BASE}{href}"),
                raw_data: json!({"category": category, "company": company, "title": title}),
            });
        }

        let next_cursor = (index + 1 < CATEGORIES.len()).then(|| (index + 1).to_string());
        Ok(Page { items, next_cursor, ..Page::default() })
    }
}

#[cfg(test)]
mod tests {
    use shimmer_core::testing::TestEnv;
    use shimmer_core::{ErrorCode, HttpResponse};

    use super::*;

    const FULL_STACK_PAGE: &str = include_str!("../../tests/fixtures/wwr/full_stack.html");
    const EMPTY_SHELL_PAGE: &str = include_str!("../../tests/fixtures/wwr/empty_shell.html");

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
        stub(env, &format!("{BASE}/robots.txt"), ok("User-agent: *\nAllow: /\n"));
    }

    #[test]
    fn id_is_weworkremotely_and_method_is_scrape() {
        assert_eq!(WeWorkRemotely.id(), "weworkremotely");
        assert_eq!(WeWorkRemotely.method(), Method::Scrape);
    }

    #[tokio::test]
    async fn the_first_page_parses_both_listings_from_the_real_page_shape() {
        let env = TestEnv::new("fetchers");
        allow_robots(&env);
        stub(&env, &format!("{BASE}/categories/remote-full-stack-programming-jobs"), ok(FULL_STACK_PAGE));

        let page = WeWorkRemotely.fetch_page(&env.ctx, None).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].source_id, "/remote-jobs/lemon-io-senior-angular-full-stack-developer");
        assert_eq!(
            page.items[0].url,
            "https://weworkremotely.com/remote-jobs/lemon-io-senior-angular-full-stack-developer"
        );
        assert_eq!(page.items[0].title, "Lemon.io -- Senior Angular Full-stack Developer");
        assert_eq!(page.items[1].title, "Toggl -- Senior Full Stack");
        assert_eq!(page.next_cursor, Some("1".to_string()));
    }

    #[tokio::test]
    async fn the_cursor_walks_every_category_then_stops() {
        let env = TestEnv::new("fetchers");
        allow_robots(&env);
        for category in CATEGORIES {
            stub(&env, &format!("{BASE}/categories/{category}"), ok(FULL_STACK_PAGE));
        }

        let mut cursor = None;
        let mut pages = 0;
        loop {
            let page = WeWorkRemotely.fetch_page(&env.ctx, cursor.as_deref()).await.unwrap();
            pages += 1;
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
            assert!(pages <= CATEGORIES.len(), "must not cycle past the last category");
        }
        assert_eq!(pages, CATEGORIES.len());
    }

    #[tokio::test]
    async fn a_robots_disallow_is_a_hard_refusal_not_a_fetch_attempt() {
        let env = TestEnv::new("fetchers");
        stub(&env, &format!("{BASE}/robots.txt"), ok("User-agent: *\nDisallow: /categories/\n"));
        // Deliberately no stub for the category page itself -- if the refusal were
        // downgraded to a warning and fetched anyway, this would fail with "no stub for...".
        let err = WeWorkRemotely.fetch_page(&env.ctx, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
        assert!(err.message.contains("robots.txt"), "{}", err.message);
    }

    #[tokio::test]
    async fn a_page_whose_structure_no_longer_matches_is_a_reported_failure() {
        let env = TestEnv::new("fetchers");
        allow_robots(&env);
        stub(&env, &format!("{BASE}/categories/remote-full-stack-programming-jobs"), ok(EMPTY_SHELL_PAGE));

        let err = WeWorkRemotely.fetch_page(&env.ctx, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
        assert!(err.message.contains("job listings"), "{}", err.message);
    }
}
