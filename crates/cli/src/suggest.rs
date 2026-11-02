//! "Did you mean…?" for a mistyped command, subcommand or option. The candidates are read from
//! the same help pages Tab completion reads (`complete::command_words`), so a new command is
//! suggested with no list to keep up to date.

/// The candidate closest to `word`, when it's close enough to be what was meant: at most one
/// edit for a short word, two for a longer one (an edit is a letter added, removed, changed or
/// two swapped), or `word` being the start of exactly one candidate. `None` when nothing is that
/// close, or two are equally close.
pub fn closest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let word = word.to_lowercase();
    let candidates: Vec<&str> = candidates.into_iter().filter(|c| !c.is_empty()).collect();
    if candidates.contains(&word.as_str()) {
        return None;
    }
    let starts: Vec<&str> = candidates.iter().copied().filter(|c| word.len() >= 3 && c.starts_with(&word)).collect();
    if let [only] = starts.as_slice() {
        return Some(only);
    }
    let allowed = if word.chars().count() <= 4 { 1 } else { 2 };
    let mut best: Option<(&str, usize)> = None;
    let mut tied = false;
    for c in candidates {
        let d = distance(&word, c);
        if d == 0 || d > allowed {
            continue;
        }
        match best {
            Some((_, b)) if d > b => {}
            Some((_, b)) if d == b => tied = true,
            _ => {
                best = Some((c, d));
                tied = false;
            }
        }
    }
    best.filter(|_| !tied).map(|(c, _)| c)
}

/// `unknown records command 'lsit'; did you mean 'list'? See 'shimmer records --help'`, the one
/// wording every feature uses. `feature` is `None` for a top-level command.
pub fn unknown_command(feature: Option<&str>, word: &str, usage: &str) -> String {
    let words = crate::complete::command_words(usage);
    let guess = closest(word, words.iter().map(String::as_str));
    let help = match feature {
        Some(f) => format!("shimmer {f} --help"),
        None => "shimmer --help".to_owned(),
    };
    let what = match feature {
        Some(f) => format!("unknown {f} command '{word}'"),
        None => format!("unknown command '{word}'"),
    };
    match guess {
        Some(g) => format!("{what}; did you mean '{g}'? See '{help}'"),
        // A short list is quicker read here than on the help page.
        None if (2..=4).contains(&words.len()) => {
            let (last, rest) = words.split_last().expect("at least two");
            format!("{what}: it's {} or {last}", rest.join(", "))
        }
        None => format!("{what}; see '{help}'"),
    }
}

/// `unknown option '--titel'`, with the closest of `known` when there is one.
pub fn unknown_option(name: &str, known: &[&str]) -> String {
    match closest(name, known.iter().copied()) {
        Some(g) => format!("unknown option '{name}'; did you mean '{g}'?"),
        None => format!("unknown option '{name}'"),
    }
}

/// Edits between `a` and `b` (optimal string alignment: insert, delete, change, swap two).
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_near_miss_is_suggested_and_a_far_one_isnt() {
        let top = ["records", "workspaces", "queue", "scheduler", "notify", "ping"];
        assert_eq!(closest("recrods", top), Some("records"), "two letters swapped");
        assert_eq!(closest("workspace", top), Some("workspaces"), "one letter missing");
        assert_eq!(closest("schedular", top), Some("scheduler"), "one letter changed");
        assert_eq!(closest("sched", top), Some("scheduler"), "the start of exactly one");
        assert_eq!(closest("banana", top), None, "nothing close");
        assert_eq!(closest("records", top), None, "an exact word isn't a typo");
        assert_eq!(closest("pong", ["ping", "pang"]), None, "two equally close: no guess");
        assert_eq!(closest("lsit", ["list", "show", "add"]), Some("list"));
    }

    #[test]
    fn the_message_names_the_guess_and_the_help_page() {
        let usage = "usage: shimmer records <command>\n\ncommands:\n  list COLLECTION   list them\n  show ID   one\n";
        assert_eq!(
            unknown_command(Some("records"), "lsit", usage),
            "unknown records command 'lsit'; did you mean 'list'? See 'shimmer records --help'"
        );
        assert_eq!(
            unknown_command(Some("records"), "zzz", usage),
            "unknown records command 'zzz': it's list or show",
            "a short list is named"
        );
        let long = "commands:\n  a  x\n  bb  x\n  cc  x\n  dd  x\n  ee  x\n";
        assert_eq!(unknown_command(Some("x"), "zzz", long), "unknown x command 'zzz'; see 'shimmer x --help'");
        assert_eq!(
            unknown_option("--titel", &["--title", "--limit"]),
            "unknown option '--titel'; did you mean '--title'?"
        );
        assert_eq!(unknown_option("--zzz", &["--title"]), "unknown option '--zzz'");
    }
}
