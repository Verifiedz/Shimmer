//! Source templates (ADR 0028 §10): a header, `[[question]]`s, and a single `[source]` block
//! whose text takes the answers as `${NAME}`. Mirrors `automations::template`'s format
//! (header/questions/`${NAME}`/`# if NAME`/`# unless NAME`), but modules may not depend on each
//! other (CLAUDE.md §3), so this is its own, smaller copy: exactly one source per template, never
//! several, so there is no `${RULE}`-style generated id and no `suffix`; and only the three
//! question kinds a starter template actually needs, not automations' six.
//!
//! `builtin()` is the five starters (§10's table): each one's `[kind]` table names only fields
//! `crate::kind` already validates, so a broken template is a bug this crate's own tests catch,
//! the same posture `automations::template::builtin` already takes.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{json, Map, Value};

/// `(id, heading)`, in the order `fetchers.templates` lists them.
pub const CATEGORIES: &[(&str, &str)] = &[("job-boards", "Job boards"), ("feeds", "Feeds")];

/// Every built-in template, by id.
const BUILTIN: &[(&str, &str)] = &[
    ("hn-whoishiring", include_str!("../templates/hn-whoishiring.toml")),
    ("wwr", include_str!("../templates/wwr.toml")),
    ("greenhouse", include_str!("../templates/greenhouse.toml")),
    ("lever", include_str!("../templates/lever.toml")),
    ("rss", include_str!("../templates/rss.toml")),
];

/// What an answer may be -- only what the five starters need, not automations' full set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A url-slug-shaped id (a board token, a company id, a WWR category): `[a-z0-9][a-z0-9_-]*`.
    Name,
    /// A well-formed `http://`/`https://` url.
    Url,
    /// Titles to keep: a comma list of words, OR'd (`core::filter::Filter`, ADR 0028 §8) --
    /// becomes a whole filter *value*, not plain text, so the template's own line is
    /// `filter = ${NAME}`, never `filter = "${NAME}"`.
    Filter,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Name => "name",
            Kind::Url => "url",
            Kind::Filter => "filter",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub name: String,
    pub prompt: String,
    pub kind: Kind,
    #[serde(default)]
    pub required: bool,
    pub default: Option<String>,
    pub help: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    template: Header,
    #[serde(default)]
    question: Vec<Question>,
    source: SourceText,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    label: String,
    description: String,
    category: String,
    /// The `kind` this template's `[source]` text writes -- shown in `fetchers.templates`/
    /// `.peek` so someone can tell what a template makes before reading its text.
    kind: String,
    /// What someone must know first: what it does, what it can't.
    good_to_know: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceText {
    text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    pub id: String,
    pub label: String,
    pub description: String,
    pub category: String,
    pub kind: String,
    pub good_to_know: Vec<String>,
    pub questions: Vec<Question>,
    source_text: String,
}

/// Every built-in template. One that doesn't parse is a bug a test catches, so it's left out
/// rather than failing the daemon.
pub fn builtin() -> Vec<Template> {
    BUILTIN.iter().filter_map(|(id, text)| Template::parse(id, text).ok()).collect()
}

impl Template {
    pub fn parse(id: &str, text: &str) -> Result<Self, String> {
        let bad = |m: String| format!("template '{id}': {m}");
        let f: File = toml::from_str(text).map_err(|e| bad(e.message().to_owned()))?;
        if !CATEGORIES.iter().any(|(c, _)| *c == f.template.category) {
            return Err(bad(format!("no category '{}'", f.template.category)));
        }
        for q in &f.question {
            let upper = q.name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            if q.name.is_empty() || !upper {
                return Err(bad(format!("question '{}': names are UPPER_CASE", q.name)));
            }
            if let Some(d) = &q.default {
                q.check(d).map_err(|m| bad(format!("question '{}': default: {m}", q.name)))?;
            }
        }
        // Its `[source]` text must not name a question this template never asks.
        let mut rest = f.source.text.as_str();
        while let Some(at) = rest.find("${") {
            let end = rest[at..].find('}').ok_or_else(|| bad("an unclosed ${".into()))?;
            let name = &rest[at + 2..at + end];
            if !f.question.iter().any(|q| q.name == name) {
                return Err(bad(format!("${{{name}}} isn't one of its questions")));
            }
            rest = &rest[at + end..];
        }
        if !f.source.text.contains(&format!("kind = \"{}\"", f.template.kind)) {
            return Err(bad(format!("its [template].kind ('{}') isn't what [source].text writes", f.template.kind)));
        }
        Ok(Self {
            id: id.to_owned(),
            label: f.template.label,
            description: f.template.description,
            category: f.template.category,
            kind: f.template.kind,
            good_to_know: f.template.good_to_know,
            questions: f.question,
            source_text: f.source.text,
        })
    }

    pub fn to_wire(&self) -> Value {
        let questions: Vec<Value> = self
            .questions
            .iter()
            .map(|q| {
                let mut out =
                    json!({"name": q.name, "prompt": q.prompt, "kind": q.kind.as_str(), "required": q.required});
                if let Some(d) = &q.default {
                    out["default"] = json!(d);
                }
                if let Some(h) = &q.help {
                    out["help"] = json!(h);
                }
                out
            })
            .collect();
        json!({"id": self.id, "label": self.label, "description": self.description, "category": self.category,
               "kind": self.kind, "good_to_know": self.good_to_know, "questions": questions,
               "text": self.source_text})
    }

    /// Each answer as it goes into the source file's text, checked: `values` maps question
    /// names to text; a missing one takes its default, or is empty; a required one can't be.
    pub fn answers(&self, values: &Map<String, Value>) -> Result<BTreeMap<String, String>, String> {
        if let Some(name) = values.keys().find(|k| !self.questions.iter().any(|q| &q.name == *k)) {
            let names: Vec<&str> = self.questions.iter().map(|q| q.name.as_str()).collect();
            return Err(format!("template '{}' has no question '{name}' (it asks: {})", self.id, names.join(", ")));
        }
        let mut out = BTreeMap::new();
        for q in &self.questions {
            let given = match values.get(&q.name) {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
                Some(other) => return Err(format!("{}: answers are text, got {other}", q.name)),
            };
            let answer = match given.or_else(|| q.default.clone()) {
                Some(a) => q.check(&a).map_err(|m| format!("{} ({}): {m}", q.name, q.prompt))?,
                None if q.required => return Err(format!("{} ({}) is required", q.name, q.prompt)),
                None => String::new(),
            };
            out.insert(q.name.clone(), answer);
        }
        Ok(out)
    }

    /// The source file text `id` would get, from checked `answers` -- the mechanical inverse
    /// of `schedule::SourceConfig::parse`'s own on-disk shape, but produced from a template
    /// rather than `schedule::render`'s structured params.
    pub fn fill(&self, answers: &BTreeMap<String, String>) -> String {
        let mut text = String::new();
        for line in self.source_text.lines() {
            let given = |name: &str| answers.get(name.trim()).is_some_and(|a| !a.is_empty());
            let (line, keep) = match (line.rsplit_once("# if "), line.rsplit_once("# unless ")) {
                (Some((before, name)), _) => (before.trim_end(), given(name)),
                (_, Some((before, name))) => (before.trim_end(), !given(name)),
                _ => (line, true),
            };
            if keep {
                text.push_str(&substitute(line, answers));
                text.push('\n');
            }
        }
        format!("# Made from the {} template. Yours to edit.\n{}", self.id, text.trim_start())
    }
}

fn substitute(line: &str, answers: &BTreeMap<String, String>) -> String {
    let mut out = line.to_owned();
    for (name, answer) in answers {
        out = out.replace(&format!("${{{name}}}"), answer);
    }
    out
}

impl Question {
    /// `answer`, checked for its kind, as it goes into the source file's text.
    pub fn check(&self, answer: &str) -> Result<String, String> {
        let a = answer.trim();
        match self.kind {
            Kind::Name => {
                let ok = a.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                    && a.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
                if ok {
                    Ok(a.to_owned())
                } else {
                    Err(format!("'{a}' isn't an id: lowercase letters, digits, '-' and '_'"))
                }
            }
            Kind::Url => match url::Url::parse(a) {
                Ok(u) if u.scheme() == "http" || u.scheme() == "https" => Ok(a.to_owned()),
                _ => Err(format!("'{a}' isn't a well-formed http:// or https:// url")),
            },
            Kind::Filter => {
                let words: Vec<&str> = a.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
                if words.is_empty() {
                    return Err("write one or more words separated by commas, e.g. senior, staff".into());
                }
                // A quote, backslash, bracket or control character would break the filter
                // table it's about to become part of, or (a new line) the file itself.
                if words.iter().any(|w| w.chars().any(|c| c.is_control() || "\"\\{}[]".contains(c))) {
                    return Err("can't contain a quote, backslash, bracket or control character".into());
                }
                let cond = |w: &str| format!("{{ title = {{ contains = \"{w}\" }} }}");
                Ok(if words.len() == 1 {
                    cond(words[0])
                } else {
                    format!("{{ or = [{}] }}", words.iter().map(|w| cond(w)).collect::<Vec<_>>().join(", "))
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(kind: Kind) -> Question {
        Question { name: "X".into(), prompt: "x".into(), kind, required: false, default: None, help: None }
    }

    #[test]
    fn every_built_in_template_parses() {
        for (id, text) in BUILTIN {
            Template::parse(id, text).unwrap();
        }
        assert_eq!(builtin().len(), BUILTIN.len());
    }

    #[test]
    fn answers_are_checked_by_kind_and_made_safe_for_the_file() {
        assert_eq!(q(Kind::Name).check(" figma "), Ok("figma".into()));
        assert!(q(Kind::Name).check("Figma").is_err());
        assert!(q(Kind::Name).check("").is_err());
        assert_eq!(q(Kind::Url).check("https://example.test/feed.xml"), Ok("https://example.test/feed.xml".into()));
        assert!(q(Kind::Url).check("ftp://example.test/feed.xml").is_err());
        assert!(q(Kind::Url).check("not a url").is_err());
        assert_eq!(q(Kind::Filter).check("senior"), Ok(r#"{ title = { contains = "senior" } }"#.into()));
        assert_eq!(
            q(Kind::Filter).check("senior, staff"),
            Ok(r#"{ or = [{ title = { contains = "senior" } }, { title = { contains = "staff" } }] }"#.into())
        );
        assert!(q(Kind::Filter).check(" , ").is_err());
        for bad in ["se\"nior", "se\\nior", "se{nior", "se}nior", "se[nior", "se]nior", "se\nior"] {
            assert!(q(Kind::Filter).check(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn filling_substitutes_answers_and_drops_lines_for_empty_ones() {
        let t = Template::parse(
            "t",
            r#"
[template]
label = "T"
description = "d"
category = "feeds"
kind = "rss"
good_to_know = []
[[question]]
name = "URL"
prompt = "u"
kind = "url"
required = true
[[question]]
name = "TITLE_FILTER"
prompt = "f"
kind = "filter"
[source]
text = '''
[source]
enabled = false
interval = "hourly"
kind = "rss"
filter = ${TITLE_FILTER} # if TITLE_FILTER

[kind]
url = "${URL}"
'''
"#,
        )
        .unwrap();
        let mut values = Map::new();
        assert!(t.answers(&values).unwrap_err().contains("URL (u) is required"));
        values.insert("URL".into(), json!("https://example.test/feed.xml"));
        let filled = t.fill(&t.answers(&values).unwrap());
        assert!(!filled.contains("filter ="), "{filled}");
        assert!(filled.contains("url = \"https://example.test/feed.xml\""), "{filled}");
        values.insert("TITLE_FILTER".into(), json!("senior"));
        let filled = t.fill(&t.answers(&values).unwrap());
        assert!(filled.contains(r#"filter = { title = { contains = "senior" } }"#), "{filled}");
        values.insert("NOPE".into(), json!("x"));
        assert!(t.answers(&values).unwrap_err().contains("no question 'NOPE'"));
    }

    #[test]
    fn a_template_whose_source_text_disagrees_with_its_own_declared_kind_fails_to_parse() {
        let text = r#"
[template]
label = "T"
description = "d"
category = "feeds"
kind = "rss"
good_to_know = []
[source]
text = '''
[source]
enabled = false
interval = "hourly"
kind = "json-api"
'''
"#;
        assert!(Template::parse("t", text).unwrap_err().contains("isn't what [source].text writes"));
    }
}
