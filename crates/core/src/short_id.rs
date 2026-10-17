//! Short ids (ADR 0031 §4): a client may write a unique ending of a task or trigger id instead
//! of the whole thing. Ids are ULIDs (or `usr-` plus one), whose first ten characters are the
//! creation time and whose rest is random, so the end tells them apart and the start doesn't.
//! The daemon's task and trigger ids and notify's message ids follow it (ADR 0032).

use crate::{Error, Result};

/// The shortest ending accepted, as for git's abbreviated hashes: a stray keystroke or half a
/// paste must never remove or cancel something (#183's review). The lists show 6 or more.
pub const MIN_ENDING: usize = 4;

/// The one id in `ids` that `written` is, or is the ending of, ignoring case. A whole id
/// matches at any length; an ending needs [`MIN_ENDING`] characters. `kind` names them in
/// errors ("task", "trigger").
pub fn resolve(written: &str, ids: &[String], kind: &str) -> Result<String> {
    let want = written.to_lowercase();
    if want.is_empty() {
        return Err(Error::invalid_params(format!("a {kind} id can't be empty")));
    }
    if let Some(whole) = ids.iter().find(|id| id.to_lowercase() == want) {
        return Ok(whole.clone());
    }
    if want.chars().count() < MIN_ENDING {
        return Err(Error::invalid_params(format!(
            "'{written}' is too short to pick a {kind}: give at least {MIN_ENDING} characters of its end"
        )));
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

/// How many characters of its end a list shows for an id, at the least.
pub const SHOWN: usize = 6;

/// The last `n` bytes of `s`, if that's on a character boundary.
fn tail(s: &str, n: usize) -> Option<&str> {
    s.len().checked_sub(n).and_then(|start| s.get(start..))
}

/// The ending of `id` a list shows: its last [`SHOWN`] characters, or more where another id in
/// `ids` ends the same way (ignoring case), so [`resolve`] given it back finds `id`. `ids` should
/// be every id `resolve` will look through, not only the ones on screen.
pub fn ending(id: &str, ids: &[String]) -> String {
    // Compared in place, ignoring ASCII case, without a lowercased copy of every id at every
    // width: `notify.list` calls this for each message it shows, against the whole inbox
    // (#276's review). `get` keeps a slice that isn't on a character boundary from panicking.
    let clashes = |n: usize| {
        let mine = tail(id, n);
        ids.iter().any(|other| {
            !other.eq_ignore_ascii_case(id) && tail(other, n).zip(mine).is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
        })
    };
    let mut n = SHOWN.min(id.len());
    while n < id.len() && clashes(n) {
        n += 1;
    }
    id[id.len() - n..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;

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
        assert_eq!(resolve("zzzz", &ids(), "task").unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(resolve("", &ids(), "task").unwrap_err().code, ErrorCode::InvalidParams);
    }

    #[test]
    fn an_ending_needs_four_characters_but_a_whole_id_any() {
        // One character ends exactly one id here, and is still refused (#183's review).
        let e = resolve("m", &ids(), "trigger").unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidParams);
        assert!(e.message.contains("at least 4"), "{}", e.message);
        assert_eq!(resolve("q9tm", &ids(), "trigger").unwrap(), "usr-01m4ppm0003fq9tm");
        // A module's own trigger may have a short id: written whole, it's found.
        let short = vec!["ab".to_owned()];
        assert_eq!(resolve("AB", &short, "trigger").unwrap(), "ab");
    }

    #[test]
    fn the_ending_shown_grows_until_resolve_finds_only_that_id() {
        let ids = ids();
        // The two ULIDs share "k7p2x", so six characters tell them apart; the third shows six.
        assert_eq!(ending(&ids[0], &ids), "XK7P2X");
        assert_eq!(ending(&ids[2], &ids), "3fq9tm");
        let close = vec!["01M4PPHE00ABCDEF0123AK7P2X".to_owned(), "01M4PPK8K0QRSTUV4567AK7P2X".to_owned()];
        assert_eq!(ending(&close[0], &close), "3AK7P2X");
        for id in ids.iter().chain(&close) {
            let all: Vec<String> = ids.iter().chain(&close).cloned().collect();
            assert_eq!(&resolve(&ending(id, &all), &all, "message").unwrap(), id);
        }
        assert_eq!(ending("ab", &[]), "ab");
    }
}
