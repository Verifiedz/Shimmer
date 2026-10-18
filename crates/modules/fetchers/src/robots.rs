//! robots.txt fetching and enforcement, for `Method::Scrape` sources only (ADR 0028 §7).
//! `Method::Api` sources never go through this: they call a known, source-maintained
//! endpoint, not a crawled page, so there is nothing to ask permission for.
//!
//! Built on `texting_robots` (pure Rust, zero dependencies, dual MIT/Apache-2.0 -- verified
//! live against crates.io, more recently touched than the alternative `robotstxt` crate).

use shimmer_core::{Ctx, Error, Result};
use texting_robots::Robot;

/// Identifies this daemon to sites it scrapes. Never spoofs a browser (ADR 0028 §7).
/// Matches the token `RealHttpBackend` actually sends on every request (PR #126 review:
/// the daemon-wide UA is `"Shimmer/<version>"`, not fetchers-specific -- see that fix's own
/// comment in `crates/daemon/src/http_backend.rs`) so a site's `Disallow` rule written for
/// either name is checked against the identity we actually present, not a different one.
pub const USER_AGENT: &str = concat!("Shimmer/", env!("CARGO_PKG_VERSION"));

/// Fetches and parses `url`'s host's robots.txt -- through `ctx.http`, so it gets the same
/// cache/rate-limit treatment as any other request, not a bespoke path -- and reports
/// whether `url` may be fetched as [`USER_AGENT`].
///
/// A `Disallow` match is a hard refusal (ADR 0028 §7): the caller must treat `Ok(false)` as
/// "do not fetch this," never downgrade it to a warning and proceed anyway. A missing or
/// unreadable robots.txt (any in-band status `>= 400`) permits everything -- the standard
/// crawling convention, and the absence of a policy is not itself a policy.
pub async fn allowed(ctx: &Ctx, url: &str) -> Result<bool> {
    let robots_url = texting_robots::get_robots_url(url)
        .map_err(|e| Error::invalid_params(format!("'{url}' is not a valid url: {e}")))?;
    let resp = crate::http::get(ctx, &robots_url).await?;
    if resp.status >= 400 {
        return Ok(true);
    }
    // A robots.txt that fails to parse is vanishingly rare -- the grammar is deliberately
    // forgiving -- but treated the same as "missing" rather than blocking every fetch on a
    // host whose policy file happens to be garbage.
    let Ok(robot) = Robot::new(USER_AGENT, &resp.body) else { return Ok(true) };
    Ok(robot.allowed(url))
}

#[cfg(test)]
mod tests {
    use shimmer_core::testing::TestEnv;
    use shimmer_core::HttpResponse;

    use super::*;

    fn ok_body(body: &str) -> HttpResponse {
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

    #[tokio::test]
    async fn an_allowed_path_is_allowed() {
        let env = TestEnv::new("fetchers");
        let target = "https://example.test/categories/remote-programming-jobs";
        stub(&env, &texting_robots::get_robots_url(target).unwrap(), ok_body("User-agent: *\nAllow: /\n"));
        assert!(allowed(&env.ctx, target).await.unwrap());
    }

    #[tokio::test]
    async fn a_disallowed_path_is_refused() {
        let env = TestEnv::new("fetchers");
        let target = "https://example.test/admin/settings";
        stub(&env, &texting_robots::get_robots_url(target).unwrap(), ok_body("User-agent: *\nDisallow: /admin/\n"));
        assert!(!allowed(&env.ctx, target).await.unwrap(), "a Disallow match must be a hard refusal");
    }

    #[tokio::test]
    async fn a_missing_robots_txt_permits_everything() {
        let env = TestEnv::new("fetchers");
        let target = "https://example.test/categories/remote-programming-jobs";
        stub(
            &env,
            &texting_robots::get_robots_url(target).unwrap(),
            HttpResponse { status: 404, body: Vec::new(), from_cache: false, stale: false, retry_after_secs: None },
        );
        assert!(allowed(&env.ctx, target).await.unwrap(), "absence of a policy is not itself a policy");
    }
}
