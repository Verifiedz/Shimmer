//! We Work Remotely, scraped (ADR 0028 §10) -- static, server-rendered category pages, no
//! JS needed. Verified live on 2026-10-07: `robots.txt` is `Allow: /`, disallowing only
//! `/admin/`, `/account/`, `/job-seekers/.../profile`, `/manage-company/` -- none of which
//! this touches; no login wall on listings; selectors below captured against the real
//! `/categories/remote-full-stack-programming-jobs` page that day (see
//! `tests/fixtures/wwr/full_stack.html` for a trimmed snapshot).
//!
//! One source, one category (the `wwr` kind's own `category` field) -- there is no `?page=`
//! on a category page (a category lists everything in one page), so tracking several
//! categories means making several sources, each its own id, rather than one source cycling
//! through a fixed, compiled-in list the way this did before ADR 0028 §10.

use async_trait::async_trait;
use serde_json::json;
use shimmer_core::{Ctx, Error, Result};

use crate::kind::{Kind, KindConfig};
use crate::source::{Method, Page, RawItem, Source};
use crate::{html, http, robots};

const BASE: &str = "https://weworkremotely.com";

pub struct WeWorkRemotely {
    id: String,
    /// The category's own url slug, e.g. `remote-full-stack-programming-jobs` -- one source
    /// per category (ADR 0028 §10): tracking several means making several sources, each with
    /// its own id and its own dedup/schedule state, rather than one source silently cycling
    /// through a fixed, compiled-in list of them.
    category: String,
}

impl WeWorkRemotely {
    pub fn new(id: String, category: String) -> Self {
        Self { id, category }
    }
}

/// The `wwr` kind (ADR 0028 §10): one required field, `category` (the url slug after
/// `/categories/`, e.g. `remote-full-stack-programming-jobs`).
pub struct WwrKind;

const WWR_FIELDS: &[&str] = &["category"];

impl Kind for WwrKind {
    fn name(&self) -> &'static str {
        "wwr"
    }

    fn method(&self) -> Method {
        Method::Scrape
    }

    fn parse(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> std::result::Result<Box<dyn KindConfig>, String> {
        crate::kind::reject_unknown(params, WWR_FIELDS)?;
        let category = crate::kind::required_str(params, "category")?;
        Ok(Box::new(WwrConfig { category }))
    }
}

#[derive(Debug)]
struct WwrConfig {
    category: String,
}

impl KindConfig for WwrConfig {
    fn build(&self, id: String) -> std::sync::Arc<dyn Source> {
        std::sync::Arc::new(WeWorkRemotely::new(id, self.category.clone()))
    }
}

#[async_trait]
impl Source for WeWorkRemotely {
    fn id(&self) -> &str {
        &self.id
    }

    fn method(&self) -> Method {
        Method::Scrape
    }

    /// One category is one page -- WWR's own category listing has no further pagination
    /// (confirmed live, ADR 0028 §10), so this never returns a `next_cursor`.
    async fn fetch_page(&self, ctx: &Ctx, _cursor: Option<&str>) -> Result<Page> {
        let url = format!("{BASE}/categories/{}", self.category);

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
                raw_data: json!({"category": self.category, "company": company, "title": title}),
            });
        }

        Ok(Page { items, ..Page::default() })
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

    fn wwr(category: &str) -> WeWorkRemotely {
        WeWorkRemotely::new("my-wwr".into(), category.into())
    }

    #[test]
    fn id_is_whatever_it_was_constructed_with_and_method_is_scrape() {
        let w = wwr("remote-full-stack-programming-jobs");
        assert_eq!(w.id(), "my-wwr");
        assert_eq!(w.method(), Method::Scrape);
    }

    #[test]
    fn wwr_kind_requires_a_category_and_rejects_anything_else() {
        let params = json!({"category": "remote-devops-sysadmin-jobs"}).as_object().unwrap().clone();
        assert!(WwrKind.parse(&params).is_ok());
        assert!(WwrKind.parse(&serde_json::Map::new()).is_err());
        let extra = json!({"category": "x", "nope": true}).as_object().unwrap().clone();
        assert!(WwrKind.parse(&extra).is_err());
    }

    #[tokio::test]
    async fn the_first_page_parses_both_listings_from_the_real_page_shape() {
        let env = TestEnv::new("fetchers");
        allow_robots(&env);
        stub(&env, &format!("{BASE}/categories/remote-full-stack-programming-jobs"), ok(FULL_STACK_PAGE));

        let page = wwr("remote-full-stack-programming-jobs").fetch_page(&env.ctx, None).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].source_id, "/remote-jobs/lemon-io-senior-angular-full-stack-developer");
        assert_eq!(
            page.items[0].url,
            "https://weworkremotely.com/remote-jobs/lemon-io-senior-angular-full-stack-developer"
        );
        assert_eq!(page.items[0].title, "Lemon.io -- Senior Angular Full-stack Developer");
        assert_eq!(page.items[1].title, "Toggl -- Senior Full Stack");
        assert!(page.next_cursor.is_none(), "a category page has no further pagination");
    }

    #[tokio::test]
    async fn a_robots_disallow_is_a_hard_refusal_not_a_fetch_attempt() {
        let env = TestEnv::new("fetchers");
        stub(&env, &format!("{BASE}/robots.txt"), ok("User-agent: *\nDisallow: /categories/\n"));
        // Deliberately no stub for the category page itself -- if the refusal were
        // downgraded to a warning and fetched anyway, this would fail with "no stub for...".
        let err = wwr("remote-full-stack-programming-jobs").fetch_page(&env.ctx, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
        assert!(err.message.contains("robots.txt"), "{}", err.message);
    }

    #[tokio::test]
    async fn a_page_whose_structure_no_longer_matches_is_a_reported_failure() {
        let env = TestEnv::new("fetchers");
        allow_robots(&env);
        stub(&env, &format!("{BASE}/categories/remote-full-stack-programming-jobs"), ok(EMPTY_SHELL_PAGE));

        let err = wwr("remote-full-stack-programming-jobs").fetch_page(&env.ctx, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
        assert!(err.message.contains("job listings"), "{}", err.message);
    }
}
