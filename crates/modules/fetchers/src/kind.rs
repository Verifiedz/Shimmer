//! `Kind`: one source type, owning its own config parsing and validation (ADR 0028 §3).
//! Replaces the old flat `SourceConfig` match blocks duplicated in `schedule.rs` (validation)
//! and `source.rs` (building) -- adding a kind is adding one file implementing [`Kind`] and
//! [`KindConfig`], plus one line in [`kinds`], never an edit to another kind's file or to
//! either of those shared dispatchers.
//!
//! `hn-whoishiring` and `wwr` are `Kind`s too, as of ADR 0028 §10 -- there is no fixed,
//! compiled-in registry any more. `HnKind` has no fields of its own; `WwrKind` has one
//! (`category`), since "which category" used to be a hardcoded list this source cycled
//! through rather than a per-source setting.

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::source::{Method, Source};

/// One source type: parses and validates its own params, builds a [`Source`]. `method` is
/// static per kind (not per parsed instance) -- `schedule.rs`'s per-`Method` interval floor
/// needs it before a config entry is known to parse at all.
pub trait Kind: Send + Sync {
    /// The config-file word, e.g. `"rss"`, `"json-api"`.
    fn name(&self) -> &'static str;
    fn method(&self) -> Method;
    /// `params` is everything in a source's config entry that isn't `enabled`/`interval`/
    /// `kind`/`filter` (`SourceConfig::kind_params`) -- this kind's own fields, and nothing
    /// else. The message on `Err` is kind-local, no `sources.<id>:` prefix -- the caller
    /// (`schedule.rs`/`source.rs`) adds that context, so a kind's own file never needs to
    /// know which source id it's validating.
    fn parse(&self, params: &Map<String, Value>) -> std::result::Result<Box<dyn KindConfig>, String>;
}

/// One kind's own, already-validated settings, opaque to everything outside that kind's file.
pub trait KindConfig: Send + Sync + std::fmt::Debug {
    /// Builds the `Source` this config describes, for one source `id`.
    fn build(&self, id: String) -> Arc<dyn Source>;
}

/// Every kind a source's config may name. Adding one is adding a line here.
pub fn kinds() -> Vec<Box<dyn Kind>> {
    vec![
        Box::new(crate::sources::rss::RssKind),
        Box::new(crate::sources::json_api::JsonApiKind),
        Box::new(crate::sources::hn::HnKind),
        Box::new(crate::sources::wwr::WwrKind),
    ]
}

pub fn kind(name: &str) -> Option<Box<dyn Kind>> {
    kinds().into_iter().find(|k| k.name() == name)
}

/// Every known kind's name, for an "unknown kind" error to name what *is* known, generated
/// from `kinds()` rather than hand-maintained so it can never drift from the real list.
pub fn known_names() -> String {
    kinds().iter().map(|k| k.name()).collect::<Vec<_>>().join(", ")
}

/// `params` must have no keys beyond `known` -- a kind with no fields of its own (none exist
/// yet, but nothing stops one) calls this with an empty `known` to reject any stray key at
/// all, the same "catch a typo" guarantee `SourceConfig`'s old `deny_unknown_fields` gave
/// before `#[serde(flatten)]` made that no longer possible to check at the struct level
/// (serde's flatten and `deny_unknown_fields` don't compose).
pub(crate) fn reject_unknown(params: &Map<String, Value>, known: &[&str]) -> std::result::Result<(), String> {
    match params.keys().find(|k| !known.contains(&k.as_str())) {
        Some(bad) => Err(format!("unknown field '{bad}'")),
        None => Ok(()),
    }
}

/// A required field's value as non-empty text.
pub(crate) fn required_str(params: &Map<String, Value>, field: &str) -> std::result::Result<String, String> {
    match params.get(field).and_then(Value::as_str).map(str::trim) {
        Some(s) if !s.is_empty() => Ok(s.to_owned()),
        _ => Err(format!("needs a non-empty {field}")),
    }
}

/// An optional field's value as non-empty text when present; `None` when absent. A present
/// but blank value is still an error -- the same "say what you meant, or leave it out
/// entirely" rule every other optional field here already has.
pub(crate) fn optional_str(params: &Map<String, Value>, field: &str) -> std::result::Result<Option<String>, String> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_str().map(str::trim) {
            Some(s) if !s.is_empty() => Ok(Some(s.to_owned())),
            _ => Err(format!("{field} must be non-empty when given")),
        },
    }
}

/// A field's value as a non-empty, well-formed `http://`/`https://` url. `url` is deliberately
/// not free text: a configured kind's url feeds straight into `http::get`, so a malformed value
/// would otherwise surface as a cryptic connection error deep in the first fetch rather than a
/// clear one at load time.
pub(crate) fn required_url(params: &Map<String, Value>, field: &str) -> std::result::Result<String, String> {
    let url = required_str(params, field)?;
    if matches!(url::Url::parse(&url), Ok(u) if u.scheme() == "http" || u.scheme() == "https") {
        Ok(url)
    } else {
        Err(format!("{field} must be a well-formed http:// or https:// url, got '{url}'"))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn kinds_has_every_generic_kind_and_only_those() {
        let names: Vec<_> = kinds().iter().map(|k| k.name()).collect();
        assert_eq!(names, vec!["rss", "json-api", "hn-whoishiring", "wwr"]);
        assert!(kind("rss").is_some());
        assert!(kind("json-api").is_some());
        assert!(kind("hn-whoishiring").is_some());
        assert!(kind("wwr").is_some());
        assert!(kind("carrier-pigeon").is_none());
    }

    #[test]
    fn known_names_lists_every_kind() {
        assert_eq!(known_names(), "rss, json-api, hn-whoishiring, wwr");
    }

    #[test]
    fn reject_unknown_catches_a_stray_field_and_an_empty_known_list_rejects_anything() {
        let params = json!({"url": "https://x.test"}).as_object().unwrap().clone();
        assert!(reject_unknown(&params, &["url"]).is_ok());
        assert!(reject_unknown(&params, &[]).unwrap_err().contains("url"));

        let typo = json!({"urll": "https://x.test"}).as_object().unwrap().clone();
        assert!(reject_unknown(&typo, &["url"]).unwrap_err().contains("urll"));
    }

    #[test]
    fn required_str_rejects_missing_blank_and_non_string() {
        let present = json!({"id_field": "id"}).as_object().unwrap().clone();
        assert_eq!(required_str(&present, "id_field").unwrap(), "id");
        assert!(required_str(&json!({}).as_object().unwrap().clone(), "id_field").is_err());
        assert!(required_str(&json!({"id_field": "  "}).as_object().unwrap().clone(), "id_field").is_err());
        assert!(required_str(&json!({"id_field": 5}).as_object().unwrap().clone(), "id_field").is_err());
    }

    #[test]
    fn optional_str_is_none_when_absent_but_an_error_when_present_and_blank() {
        assert_eq!(optional_str(&json!({}).as_object().unwrap().clone(), "credential").unwrap(), None);
        assert_eq!(
            optional_str(&json!({"credential": null}).as_object().unwrap().clone(), "credential").unwrap(),
            None
        );
        assert_eq!(
            optional_str(&json!({"credential": "my-key"}).as_object().unwrap().clone(), "credential").unwrap(),
            Some("my-key".to_owned())
        );
        assert!(optional_str(&json!({"credential": "  "}).as_object().unwrap().clone(), "credential").is_err());
    }

    #[test]
    fn required_url_accepts_http_and_https_and_rejects_everything_else() {
        let ok = |u: &str| json!({"url": u}).as_object().unwrap().clone();
        assert!(required_url(&ok("https://x.test/feed.xml"), "url").is_ok());
        assert!(required_url(&ok("http://x.test/feed.xml"), "url").is_ok());
        assert!(required_url(&ok("ftp://x.test/feed.xml"), "url").unwrap_err().contains("url"));
        assert!(required_url(&ok("not a url"), "url").is_err());
        assert!(required_url(&json!({}).as_object().unwrap().clone(), "url").is_err());
    }
}
