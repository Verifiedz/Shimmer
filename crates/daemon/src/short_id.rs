//! Short ids (ADR 0031 §4): a client may write a unique ending of a task or trigger id instead
//! of the whole thing. Ids are ULIDs (or `usr-` plus one), whose first ten characters are the
//! creation time and whose rest is random, so the end tells them apart and the start doesn't.

use shimmer_core::{Error, Result};

/// The one id in `ids` that `written` is the ending of, ignoring case. `kind` names them in
/// errors ("task", "trigger").
pub fn resolve(written: &str, ids: &[String], kind: &str) -> Result<String> {
    let want = written.to_lowercase();
    if want.is_empty() {
        return Err(Error::invalid_params(format!("a {kind} id can't be empty")));
    }
    let matches: Vec<&String> = ids.iter().filter(|id| id.to_lowercase().ends_with(&want)).collect();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        // Worded as each op said it before short ids, so clients matching on it keep working.
        [] if kind == "trigger" => Err(Error::not_found(format!("no trigger '{written}'"))),
        [] => Err(Error::not_found(format!("no {kind} {written}"))),
        many => {
            let shown: Vec<&str> = many.iter().take(5).map(|s| s.as_str()).collect();
            let more = if many.len() > 5 { format!(" and {} more", many.len() - 5) } else { String::new() };
            Err(Error::conflict(format!(
                "'{written}' ends {} {kind} ids ({}{more}): give more of it",
                many.len(),
                shown.join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shimmer_core::ErrorCode;

    fn ids() -> Vec<String> {
        // Made a minute apart: the same start; the random ends differ but share "k7p2x".
        vec!["01M4PPHE00ABCDEF0123XK7P2X".into(), "01M4PPK8K0QRSTUV4567HK7P2X".into(), "usr-01m4ppm0003fq9tm".into()]
    }

    #[test]
    fn a_unique_ending_finds_the_id_in_any_case() {
        assert_eq!(resolve("xk7p2x", &ids(), "task").unwrap(), "01M4PPHE00ABCDEF0123XK7P2X");
        assert_eq!(resolve("HK7P2X", &ids(), "task").unwrap(), "01M4PPK8K0QRSTUV4567HK7P2X");
        assert_eq!(resolve("3fq9tm", &ids(), "trigger").unwrap(), "usr-01m4ppm0003fq9tm");
    }

    #[test]
    fn an_ending_shared_by_several_asks_for_more_and_none_is_not_found() {
        let e = resolve("k7p2x", &ids(), "task").unwrap_err();
        assert_eq!(e.code, ErrorCode::Conflict);
        assert!(e.message.contains("ends 2 task ids") && e.message.contains("give more of it"), "{}", e.message);
        assert_eq!(resolve("zzz", &ids(), "task").unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(resolve("", &ids(), "task").unwrap_err().code, ErrorCode::InvalidParams);
    }
}
