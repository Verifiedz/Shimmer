//! Record templates end to end (ADRs 0022, 0030): the real `shimmer` binary and daemon in a
//! throwaway `$SHIMMER_HOME`. Browsing and peeking change nothing; every template's own "How it
//! works" examples run as written.

mod common;

use common::{fails, ok, Home};
use serde_json::Value;

fn json(home: &Home, args: &[&str]) -> Value {
    let mut args = args.to_vec();
    args.push("--json");
    serde_json::from_str(&ok(home, &args)).unwrap()
}

/// Every built-in template's id, as the daemon lists them.
fn template_ids(home: &Home) -> Vec<String> {
    json(home, &["records", "templates"])["templates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Nothing written by a read: no collections, and no event but the daemon's own start-up.
fn nothing_written(home: &Home) {
    let collections = home.dir.path().join("home/data/records/collections");
    let made: Vec<_> = std::fs::read_dir(&collections).map(|d| d.flatten().collect()).unwrap_or_default();
    assert!(made.is_empty(), "browsing created {made:?}");
    let events = home.dir.path().join("home/events");
    for day in std::fs::read_dir(events).into_iter().flatten().flatten() {
        let text = std::fs::read_to_string(day.path()).unwrap();
        assert!(!text.contains("\"records."), "browsing emitted a records event: {text}");
    }
}

#[test]
fn templates_are_listed_by_category_and_each_can_be_peeked() {
    let home = Home::new();
    let listing = ok(&home, &["records", "templates"]);
    let headings: Vec<&str> = listing.lines().filter(|l| !l.starts_with(' ') && l.ends_with(')')).collect();
    assert_eq!(
        headings,
        ["Job hunt (job-hunt)", "Practice (practice)", "Career history (career)", "Life admin (life)"]
    );
    assert!(listing.contains("shimmer records peek TEMPLATE"), "{listing}");

    let ids = template_ids(&home);
    assert_eq!(ids.len(), 16, "{ids:?}");
    for id in &ids {
        assert!(listing.contains(&format!("\n  {id} ")), "{id} missing from:\n{listing}");
        let peek = ok(&home, &["records", "peek", id]);
        assert!(peek.starts_with(&format!("{id}: ")), "{peek}");
        for part in
            ["\nFields (", "\nWhen you complete a record\n", "\nHow it works (", "shimmer records new ID --from"]
        {
            assert!(peek.contains(part), "{id}: no '{part}' in:\n{peek}");
        }
        // `templates TEMPLATE` is the same as `peek TEMPLATE`.
        assert_eq!(ok(&home, &["records", "templates", id]), peek, "{id}");
    }

    // One category on its own.
    let practice = ok(&home, &["records", "templates", "practice"]);
    assert!(
        practice.starts_with("Practice (practice)\n  leetcode") && !practice.contains("job-applications"),
        "{practice}"
    );

    // What completing does, in words, from each kind of collection.
    assert!(ok(&home, &["records", "peek", "leetcode"]).contains("stamps last_solved with today's date"));
    assert!(ok(&home, &["records", "peek", "job-applications"]).contains("It happens once"));
    assert!(ok(&home, &["records", "peek", "addresses"]).contains("it's a reference list"));
    // Notes from the header, privacy first among them.
    assert!(ok(&home, &["records", "peek", "documents"]).contains("• Never store document numbers here"));

    // Mistakes say what there is, and fail.
    assert!(fails(&home, &["records", "peek", "chess"]).contains("no template 'chess' (there are: "));
    assert!(fails(&home, &["records", "peek", "practice"]).contains("'practice' is a category, not a template"));
    assert!(fails(&home, &["records", "templates", "games"]).contains("no template or category 'games'"));
    assert!(fails(&home, &["records", "peek"]).contains("usage: shimmer records peek TEMPLATE"));

    nothing_written(&home);
}

#[test]
fn peek_file_is_exactly_what_new_creates() {
    let home = Home::new();
    for id in template_ids(&home) {
        let file = ok(&home, &["records", "peek", &id, "--file"]);
        assert!(file.contains(&format!("id = \"{id}\"")), "{id}");
        let one = json(&home, &["records", "peek", &id, "--file"]);
        assert_eq!(one["file"].as_str().unwrap().trim_end(), file.trim_end(), "{id}: --json carries the same file");

        // `new` copies it with only the id changed.
        let mine = format!("my-{id}");
        ok(&home, &["records", "new", &mine, "--from", &id]);
        let written =
            std::fs::read_to_string(home.dir.path().join(format!("home/data/records/collections/{mine}.toml")))
                .unwrap();
        let expected = file.replacen(&format!("id = \"{id}\""), &format!("id = \"{mine}\""), 1);
        assert_eq!(written.trim_end(), expected.trim_end(), "{id}");
    }
    let summary = json(&home, &["records", "peek", "leetcode"]);
    assert_eq!((summary["id"].as_str(), summary["category"].as_str()), (Some("leetcode"), Some("practice")));
    assert!(summary.get("file").is_none(), "the file only with --file");
}

/// The shell words of one example command: quotes grouped, `\` continuations joined, and the
/// description after it (three or more spaces outside quotes) left off.
fn example_commands(examples: &[Value]) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut lines = examples.iter().filter_map(Value::as_str).peekable();
    while let Some(line) = lines.next() {
        if !line.starts_with("shimmer records ") {
            continue; // a description on its own line
        }
        let mut text = line.to_owned();
        while text.trim_end().ends_with('\\') {
            text = format!("{} {}", text.trim_end().trim_end_matches('\\'), lines.next().unwrap_or_default().trim());
        }
        let (mut words, mut word, mut quoted, mut spaces) = (Vec::new(), String::new(), false, 0);
        for c in text.chars() {
            match c {
                '"' => quoted = !quoted,
                ' ' if !quoted => {
                    spaces += 1;
                    if spaces == 3 {
                        break;
                    }
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                    continue;
                }
                _ => word.push(c),
            }
            spaces = 0;
        }
        if !word.is_empty() {
            words.push(word);
        }
        commands.push(words.into_iter().skip(1).collect());
    }
    commands
}

#[test]
fn every_templates_examples_work_as_written() {
    let home = Home::new();
    let all = json(&home, &["records", "templates"]);
    // Each under its own id: that's the name the templates' refs use for each other.
    for t in all["templates"].as_array().unwrap() {
        let id = t["id"].as_str().unwrap();
        ok(&home, &["records", "new", id, "--from", id]);
    }
    for t in all["templates"].as_array().unwrap() {
        let id = t["id"].as_str().unwrap();
        let commands = example_commands(t["examples"].as_array().unwrap());
        assert!(commands.iter().any(|c| c.first().is_some_and(|w| w == "records")), "{id}: {commands:?}");
        let mut added = String::new();
        for words in commands {
            let words: Vec<String> = words
                .iter()
                .map(|w| w.replace("<collection>/<id>", &format!("{id}/{added}")).replace("<collection>", id))
                .collect();
            let args: Vec<&str> = words.iter().map(String::as_str).collect();
            let out = ok(&home, &args);
            if let Some(made) = out.trim().strip_prefix(&format!("added {id}/")) {
                added = made.to_owned();
            }
        }
        let check = ok(&home, &["records", "check", id]);
        assert!(check.contains("all fit"), "{id}: {check}");
    }
}
