//! A shared HTML-parsing helper for `Method::Scrape` sources (ADR 0028 §7), built on
//! `scraper` (pure Rust, ISC, no system dependency -- verified to build identically on both
//! CI legs, `.github/workflows/ci.yml:10`).

use scraper::{ElementRef, Html, Selector};
use shimmer_core::{Error, Result};

pub fn parse(body: &[u8]) -> Html {
    Html::parse_document(&String::from_utf8_lossy(body))
}

/// Every element matching `selector`, or a distinct, reported failure if none match -- a 200
/// page that doesn't contain what a source expects is a structure change (or a bug), not a
/// quietly empty result (ADR 0028 §7). `what` names what was being looked for, for the error
/// message; the caller (the orchestration that drives a `Source`, landing in a later change)
/// is responsible for turning this into `fetchers.fetch.failed`'s `reason: "parse_empty"`.
pub fn select_or_fail<'a>(doc: &'a Html, selector: &str, what: &str) -> Result<Vec<ElementRef<'a>>> {
    let parsed =
        Selector::parse(selector).map_err(|e| Error::internal(format!("invalid selector '{selector}': {e:?}")))?;
    let found: Vec<ElementRef<'a>> = doc.select(&parsed).collect();
    if found.is_empty() {
        return Err(Error::module_error(format!(
            "no elements matched '{selector}' looking for {what}; the page's structure may have changed"
        )));
    }
    Ok(found)
}

/// The first element matching `selector` within `scope`, or `None` if nothing matched --
/// for a field that's optional on one listing without failing the whole page, unlike
/// [`select_or_fail`], which is for the listing boundary itself.
pub fn select_one<'a>(scope: ElementRef<'a>, selector: &str) -> Result<Option<ElementRef<'a>>> {
    let parsed =
        Selector::parse(selector).map_err(|e| Error::internal(format!("invalid selector '{selector}': {e:?}")))?;
    Ok(scope.select(&parsed).next())
}

/// [`select_one`]'s matched element's text, trimmed; `None` if nothing matched or its text
/// was empty (never a distinction a caller needs to make between the two).
pub fn text_of(scope: ElementRef<'_>, selector: &str) -> Result<Option<String>> {
    let Some(el) = select_one(scope, selector)? else { return Ok(None) };
    let text = el.text().collect::<String>();
    let trimmed = text.trim();
    Ok((!trimmed.is_empty()).then(|| trimmed.to_owned()))
}

#[cfg(test)]
mod tests {
    use shimmer_core::ErrorCode;

    use super::*;

    const PAGE: &str = r#"<html><body>
        <div class="job"><a class="title" href="/jobs/1">Senior Rustacean</a></div>
        <div class="job"><a class="title" href="/jobs/2">Junior Rustacean</a></div>
    </body></html>"#;

    #[test]
    fn finds_every_matching_element() {
        let doc = parse(PAGE.as_bytes());
        let titles = select_or_fail(&doc, "a.title", "job titles").unwrap();
        assert_eq!(titles.len(), 2);
        assert_eq!(titles[0].text().collect::<String>(), "Senior Rustacean");
        assert_eq!(titles[1].value().attr("href"), Some("/jobs/2"));
    }

    #[test]
    fn select_one_and_text_of_find_a_scoped_optional_field() {
        let doc = parse(PAGE.as_bytes());
        let job = select_or_fail(&doc, "div.job", "jobs").unwrap()[0];
        assert_eq!(text_of(job, "a.title").unwrap(), Some("Senior Rustacean".to_string()));
        assert_eq!(select_one(job, "a.title").unwrap().unwrap().value().attr("href"), Some("/jobs/1"));
    }

    #[test]
    fn text_of_is_none_rather_than_erroring_when_an_optional_field_is_missing() {
        let doc = parse(PAGE.as_bytes());
        let job = select_or_fail(&doc, "div.job", "jobs").unwrap()[0];
        assert_eq!(text_of(job, "span.salary-that-does-not-exist").unwrap(), None);
    }

    #[test]
    fn no_matches_on_an_otherwise_fine_page_is_a_reported_failure_not_an_empty_success() {
        let doc = parse(PAGE.as_bytes());
        let err = select_or_fail(&doc, "div.listing-that-no-longer-exists", "job listings").unwrap_err();
        assert_eq!(err.code, ErrorCode::ModuleError);
        assert!(err.message.contains("job listings"), "{}", err.message);
    }

    #[test]
    fn an_invalid_selector_is_its_own_distinct_error() {
        let doc = parse(PAGE.as_bytes());
        let err = select_or_fail(&doc, ":::not-css", "anything").unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal, "a bad selector is a bug in the source, not the page");
    }

    #[test]
    fn non_utf8_bytes_are_handled_rather_than_panicking() {
        let doc = parse(&[0xff, 0xfe, b'<', b'p', b'>', b'h', b'i', b'<', b'/', b'p', b'>']);
        let found = select_or_fail(&doc, "p", "paragraphs").unwrap();
        assert_eq!(found.len(), 1);
    }
}
