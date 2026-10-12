//! The manifest lists every topic the module emits, and only those.

use shimmer_core::Module;

fn module() -> shimmer_records::Records {
    shimmer_records::Records::default()
}
const PREFIX: &str = "records.";

/// Every topic `{prefix}…` the crate's code emits, as a quoted `"<module>.<entity>.<verb>"`
/// outside its `#[cfg(test)]` parts.
fn emitted_topics(prefix: &str) -> std::collections::BTreeSet<String> {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(src).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        let code = text.split("#[cfg(test)]").next().unwrap();
        for piece in code.split('"').skip(1).step_by(2) {
            let is_topic = piece.starts_with(prefix)
                && piece.matches('.').count() == 2
                && piece.chars().all(|c| c.is_ascii_lowercase() || c == '.' || c == '_');
            if is_topic {
                out.insert(piece.to_owned());
            }
        }
    }
    out
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
