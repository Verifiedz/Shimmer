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

/// The topics quoted in `code`. Escapes and the quote character aren't string boundaries, so they
/// go first: otherwise a line with an odd number of `"` (an escaped one, or `'"'`) turns the rest
/// of the file inside out, and nothing after it is read as a string (#200's review).
fn topics_in(code: &str, prefix: &str) -> std::collections::BTreeSet<String> {
    let code = code.replace("\\\\", "").replace("\\\"", "").replace("'\"'", "");
    code.split('"')
        .skip(1)
        .step_by(2)
        .filter(|piece| {
            piece.starts_with(prefix)
                && piece.matches('.').count() == 2
                && piece.chars().all(|c| c.is_ascii_lowercase() || c == '.' || c == '_')
        })
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_scan_reads_past_escaped_quotes() {
    // The kind of line #200's review found, then a topic after it; also a quote character and an
    // escaped backslash.
    let code = r#"let open = ["\"\"\"", "'''"]; let q = '"'; let b = "\\"; emit("PREFIXthing.happened", x);"#;
    let found = topics_in(&code.replace("PREFIX", PREFIX), PREFIX);
    assert!(found.contains(&format!("{PREFIX}thing.happened")), "{found:?}");
    // In this crate's real source: a topic added at the very end of template.rs's code is found.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/template.rs");
    let real = std::fs::read_to_string(path).unwrap();
    let code = real.split("#[cfg(test)]").next().unwrap();
    let added = format!("{code}\nfn x() {{ emit(\"{PREFIX}late.added\") }}");
    assert!(topics_in(&added, PREFIX).contains(&format!("{PREFIX}late.added")), "the scan stops early in template.rs");
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
