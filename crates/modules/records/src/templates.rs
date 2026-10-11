//! The built-in collection templates (ADR 0022) and what `records.templates` says about each
//! (ADR 0030): its category, and the notes and examples from its header comment. Pure: the
//! texts are compiled in, and everything here reads them (§12 rule 10).

/// Built-in templates, by id, with their category. Each is an ordinary collection file whose
/// `[collection] id` is the template's id; creating one copies it with only the id (and label)
/// changed.
pub const TEMPLATES: &[(&str, &str, &str)] = &[
    ("addresses", "life", include_str!("../templates/addresses.toml")),
    ("certifications", "career", include_str!("../templates/certifications.toml")),
    ("charity", "life", include_str!("../templates/charity.toml")),
    ("documents", "life", include_str!("../templates/documents.toml")),
    ("education", "career", include_str!("../templates/education.toml")),
    ("employment", "career", include_str!("../templates/employment.toml")),
    ("interview-questions", "job-hunt", include_str!("../templates/interview-questions.toml")),
    ("interviews", "job-hunt", include_str!("../templates/interviews.toml")),
    ("job-applications", "job-hunt", include_str!("../templates/job-applications.toml")),
    ("leetcode", "practice", include_str!("../templates/leetcode.toml")),
    ("networking-events", "job-hunt", include_str!("../templates/networking-events.toml")),
    ("offers", "job-hunt", include_str!("../templates/offers.toml")),
    ("outreach", "job-hunt", include_str!("../templates/outreach.toml")),
    ("projects", "career", include_str!("../templates/projects.toml")),
    ("stories", "job-hunt", include_str!("../templates/stories.toml")),
    ("subscriptions", "life", include_str!("../templates/subscriptions.toml")),
];

/// The categories, in the order `records.templates` lists them, with their headings.
pub const CATEGORIES: &[(&str, &str)] =
    &[("job-hunt", "Job hunt"), ("practice", "Practice"), ("career", "Career history"), ("life", "Life admin")];

/// The template `id`'s text, or `None`.
pub fn find(id: &str) -> Option<&'static str> {
    TEMPLATES.iter().find(|(t, _, _)| *t == id).map(|(_, _, text)| *text)
}

/// Every template id, for `not_found` messages.
pub fn names() -> Vec<&'static str> {
    TEMPLATES.iter().map(|(id, _, _)| *id).collect()
}

/// A template's position in [`CATEGORIES`], for sorting.
pub fn rank(category: &str) -> usize {
    CATEGORIES.iter().position(|(c, _)| *c == category).unwrap_or(usize::MAX)
}

/// What a template's header comment says beyond its description: `notes`, each paragraph as one
/// line (privacy warnings, what it can't do, what goes with it), and `examples`, the lines under
/// "How it works:" as written. The first paragraph (the description and "Created from…") is left
/// out: the description is already in the collection, and "Created from" is about the copy.
pub fn header(text: &str) -> (Vec<String>, Vec<String>) {
    let lines: Vec<&str> = text
        .lines()
        .take_while(|l| l.starts_with('#'))
        .map(|l| l.strip_prefix("# ").or_else(|| l.strip_prefix('#')).unwrap_or(l))
        .collect();
    let mut paragraphs: Vec<Vec<&str>> = vec![Vec::new()];
    for line in lines {
        match line.trim().is_empty() {
            true if !paragraphs.last().is_some_and(Vec::is_empty) => paragraphs.push(Vec::new()),
            true => {}
            false => paragraphs.last_mut().expect("never empty").push(line),
        }
    }
    let (mut notes, mut examples) = (Vec::new(), Vec::new());
    for p in paragraphs.into_iter().skip(1).filter(|p| !p.is_empty()) {
        match p[0].trim() == "How it works:" {
            true => examples = p[1..].iter().map(|l| l.strip_prefix("  ").unwrap_or(l).trim_end().to_owned()).collect(),
            false => notes.push(p.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ")),
        }
    }
    (notes, examples)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_has_a_known_category_and_says_how_it_works() {
        for (id, category, text) in TEMPLATES {
            assert!(rank(category) < CATEGORIES.len(), "{id}: unknown category {category}");
            let (_, examples) = header(text);
            assert!(!examples.is_empty(), "{id}: no 'How it works' examples");
            assert!(examples[0].starts_with("shimmer records "), "{id}: {examples:?}");
        }
        for (c, _) in CATEGORIES {
            assert!(TEMPLATES.iter().any(|(_, tc, _)| tc == c), "category {c} has no templates");
        }
    }

    #[test]
    fn header_splits_notes_from_examples() {
        let text = "# Things: one per thing.\n# Created from Shimmer's leetcode template.\n#\n\
                    # Private: keep it\n# private.\n#\n# How it works:\n#   shimmer records add x\n\
                    #       --more\n#\n# After the examples.\n\n[collection]\n# not header\n";
        let (notes, examples) = header(text);
        assert_eq!(notes, ["Private: keep it private.", "After the examples."]);
        assert_eq!(examples, ["shimmer records add x", "    --more"]);
    }
}
