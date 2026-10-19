//! The manifest lists every topic the module emits, and only those.

use shimmer_core::Module;

fn module() -> shimmer_workspaces::Workspaces {
    shimmer_workspaces::Workspaces::default()
}
const PREFIX: &str = "workspaces.";

/// Every topic `{prefix}…` the crate's code emits, as a quoted `"<module>.<entity>.<verb>"`
/// outside its `#[cfg(test)]` parts.
fn emitted_topics(prefix: &str) -> std::collections::BTreeSet<String> {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(src).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        out.extend(topics_in(text.split("#[cfg(test)]").next().unwrap(), prefix));
    }
    out
}

/// The topics quoted in `code`: each `"` followed straight away by `<prefix><entity>.<verb>"`.
/// Nothing counts quotes, so a stray `"` (escaped, `'"'`, in a comment or a raw string) can't
/// make the rest of a file be skipped (#200's and #208's reviews, #218).
fn topics_in(code: &str, prefix: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let mut rest = code;
    while let Some(at) = rest.find('"') {
        rest = &rest[at + 1..];
        let name: String = rest.chars().take_while(|c| c.is_ascii_lowercase() || *c == '.' || *c == '_').collect();
        let closed = rest[name.len()..].starts_with('"');
        if closed && name.starts_with(prefix) && name.matches('.').count() == 2 {
            out.insert(name);
        }
    }
    out
}

#[test]
fn the_scan_finds_topics_past_stray_quotes() {
    // Each sample has an odd number of stray quotes, so counting quotes would lose step after it:
    // the line #200's review found, a quote character, a quote in a comment, a raw string.
    for stray in [r#"let open = ["\"\"\"", "'''"];"#, r#"let q = '"';"#, "// a 5\" screen", r##"r#"say "hi"#"##] {
        let code = format!("{stray}\nemit(\"{PREFIX}thing.happened\", x);");
        assert!(topics_in(&code, PREFIX).contains(&format!("{PREFIX}thing.happened")), "lost after {stray}");
    }
    // Not topics: the wrong number of parts, or no closing quote.
    let not = topics_in(&format!(r#""{PREFIX}a" "{PREFIX}a.b.c" "{PREFIX}a.b x"#), PREFIX);
    assert!(not.is_empty(), "{not:?}");
}

#[test]
fn a_topic_at_the_end_of_any_source_file_is_found() {
    // Every file, so one the scan misreads fails here with its name, instead of silently hiding
    // whatever comes after the line that fools it.
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in std::fs::read_dir(src).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        let code = text.split("#[cfg(test)]").next().unwrap();
        let added = format!("{code}\nfn x() {{ emit(\"{PREFIX}late.added\") }}");
        assert!(
            topics_in(&added, PREFIX).contains(&format!("{PREFIX}late.added")),
            "the scan stops early in {}",
            path.display()
        );
    }
}

/// A client learns what to subscribe to from `core.manifest` (CLAUDE.md §9), so a topic the
/// module emits but doesn't declare is one nobody knows to listen for.
#[test]
fn every_topic_it_emits_is_in_its_manifest() {
    let declared = module().manifest().topics;
    let emitted = emitted_topics(PREFIX);
    assert!(!emitted.is_empty(), "found no topics in the source: the scan is broken");
    for topic in &emitted {
        assert!(declared.contains(topic), "{topic} is emitted but not in the manifest's topics");
    }
    for topic in &declared {
        assert!(emitted.contains(topic), "{topic} is in the manifest but never emitted");
    }
}

/// `shimmer automations events` shows each topic with a line on it (ADR 0035 §5.5).
#[test]
fn every_topic_has_a_description() {
    let m = module().manifest();
    for topic in &m.topics {
        let line = m.topic_descriptions.get(topic).map(String::as_str).unwrap_or("");
        assert!(!line.is_empty(), "{topic} has no description");
    }
}
