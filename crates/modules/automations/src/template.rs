//! Rule templates (ADR 0035 §9), in the workspaces template format (ADR 0025 §3): a header,
//! `[[question]]`s, and one or more `[[rule]]`s whose text takes the answers as `${NAME}` (a
//! rule's own `{field}` is filled from the event later, so the two never mix). A line ending in
//! `# if NAME` is left out when NAME's answer is empty, and one ending in `# unless NAME` when it
//! isn't. `${RULE}` is the id of the rule being made. Pure: no `Ctx` (§12 rule 10).

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{json, Map, Value};

/// `(id, heading)`, in the order `automations.templates` lists them.
pub const CATEGORIES: &[(&str, &str)] =
    &[("messages", "Tell me"), ("reminders", "Remind me later"), ("workspaces", "Open and make workspaces")];

/// Every built-in template, by id.
const BUILTIN: &[(&str, &str)] = &[
    ("notify-on-add", include_str!("../templates/notify-on-add.toml")),
    ("notify-on-complete", include_str!("../templates/notify-on-complete.toml")),
    ("notify-on-failure", include_str!("../templates/notify-on-failure.toml")),
    ("stage-message", include_str!("../templates/stage-message.toml")),
    ("field-threshold", include_str!("../templates/field-threshold.toml")),
    ("follow-up-reminder", include_str!("../templates/follow-up-reminder.toml")),
    ("spaced-review", include_str!("../templates/spaced-review.toml")),
    ("stage-opens-workspace", include_str!("../templates/stage-opens-workspace.toml")),
    ("stage-creates-workspace", include_str!("../templates/stage-creates-workspace.toml")),
];

/// What an answer may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// One line, no `{ } [ ]` (they'd read as a rule's own fields).
    Text,
    /// A collection, field or workspace id: `[a-z0-9][a-z0-9_-]*`.
    Name,
    /// A whole number, 1 to 100000.
    Number,
    Choice,
    /// Texts separated by commas: `offer, rejected`.
    List,
    /// `QUESTION=field` pairs separated by commas: a workspace template's answers from the record.
    Pairs,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Name => "name",
            Kind::Number => "number",
            Kind::Choice => "choice",
            Kind::List => "list",
            Kind::Pairs => "pairs",
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
    #[serde(default)]
    pub choices: Vec<String>,
    pub help: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleText {
    /// The rule's id is the one asked for, or `<id>-<suffix>` when a template makes several.
    #[serde(default)]
    pub suffix: String,
    pub text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    template: Header,
    #[serde(default)]
    question: Vec<Question>,
    rule: Vec<RuleText>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    label: String,
    description: String,
    category: String,
    /// What someone must know first: what it does, what it can't.
    good_to_know: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    pub id: String,
    pub label: String,
    pub description: String,
    pub category: String,
    pub good_to_know: Vec<String>,
    pub questions: Vec<Question>,
    pub rules: Vec<RuleText>,
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
            if (q.kind == Kind::Choice) != !q.choices.is_empty() {
                return Err(bad(format!("question '{}': choices go with kind = \"choice\", and only it", q.name)));
            }
            if let Some(d) = &q.default {
                q.check(d).map_err(|m| bad(format!("question '{}': default: {m}", q.name)))?;
            }
        }
        for r in &f.rule {
            let mut rest = r.text.as_str();
            while let Some(at) = rest.find("${") {
                let end = rest[at..].find('}').ok_or_else(|| bad("an unclosed ${".into()))?;
                let name = &rest[at + 2..at + end];
                if name != "RULE" && !f.question.iter().any(|q| q.name == name) {
                    return Err(bad(format!("${{{name}}} isn't one of its questions")));
                }
                rest = &rest[at + end..];
            }
        }
        Ok(Self {
            id: id.to_owned(),
            label: f.template.label,
            description: f.template.description,
            category: f.template.category,
            good_to_know: f.template.good_to_know,
            questions: f.question,
            rules: f.rule,
        })
    }

    /// The ops its rules run, for "this template needs …".
    pub fn ops(&self) -> Vec<String> {
        let mut ops: Vec<String> = self
            .rules
            .iter()
            .flat_map(|r| r.text.lines())
            .filter_map(|l| l.trim().strip_prefix("op = \"")?.split('"').next().map(str::to_owned))
            .collect();
        ops.sort();
        ops.dedup();
        ops
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
                if !q.choices.is_empty() {
                    out["choices"] = json!(q.choices);
                }
                if let Some(h) = &q.help {
                    out["help"] = json!(h);
                }
                out
            })
            .collect();
        let rules: Vec<Value> = self.rules.iter().map(|r| json!({"suffix": r.suffix, "text": r.text})).collect();
        json!({"id": self.id, "label": self.label, "description": self.description, "category": self.category,
               "good_to_know": self.good_to_know, "questions": questions, "rules": rules, "ops": self.ops()})
    }

    /// Each answer as it goes into the rule text, checked: `values` maps question names to
    /// text; a missing one takes its default, or is empty; a required one can't be empty.
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

    /// The rules it makes for `id`, `(rule id, file text)`, from checked `answers`.
    pub fn fill(&self, id: &str, answers: &BTreeMap<String, String>) -> Vec<(String, String)> {
        self.rules
            .iter()
            .map(|r| {
                let rule_id = if r.suffix.is_empty() { id.to_owned() } else { format!("{id}-{}", r.suffix) };
                let mut text = String::new();
                for line in r.text.lines() {
                    let given = |name: &str| answers.get(name.trim()).is_some_and(|a| !a.is_empty());
                    let (line, keep) = match (line.rsplit_once("# if "), line.rsplit_once("# unless ")) {
                        (Some((before, name)), _) => (before.trim_end(), given(name)),
                        (_, Some((before, name))) => (before.trim_end(), !given(name)),
                        _ => (line, true),
                    };
                    if keep {
                        text.push_str(&substitute(line, answers).replace("${RULE}", &rule_id));
                        text.push('\n');
                    }
                }
                (rule_id, format!("# Made from the {} template. Yours to edit.\n{}", self.id, text.trim_start()))
            })
            .collect()
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
    /// `answer`, checked for its kind, as it goes into the rule text.
    pub fn check(&self, answer: &str) -> Result<String, String> {
        let a = answer.trim();
        let name_ok = |s: &str| {
            s.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        };
        match self.kind {
            Kind::Text => {
                if a.contains(['{', '}', '[', ']', '\n']) {
                    return Err("can't contain { } [ ] or a new line".into());
                }
                Ok(escape(a))
            }
            Kind::Name if name_ok(a) => Ok(a.to_owned()),
            Kind::Name => Err(format!("'{a}' isn't an id: lowercase letters, digits, '-' and '_'")),
            Kind::Number => match a.parse::<u32>() {
                Ok(n) if (1..=100_000).contains(&n) => Ok(n.to_string()),
                _ => Err(format!("'{a}' isn't a whole number from 1 to 100000")),
            },
            Kind::Choice if self.choices.iter().any(|c| c == a) => Ok(a.to_owned()),
            Kind::Choice => Err(format!("'{a}' isn't one of {}", self.choices.join(", "))),
            Kind::List => {
                let items: Vec<&str> = a.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
                if items.is_empty() || items.iter().any(|i| i.contains(['{', '}', '[', ']'])) {
                    return Err("write values separated by commas, e.g. offer, rejected".into());
                }
                Ok(items.iter().map(|i| format!("\"{}\"", escape(i))).collect::<Vec<_>>().join(", "))
            }
            Kind::Pairs => {
                let mut pairs = Vec::new();
                for pair in a.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    let (q, field) = pair
                        .split_once('=')
                        .map(|(q, f)| (q.trim(), f.trim()))
                        .ok_or_else(|| format!("'{pair}': write QUESTION=field, e.g. COMPANY=title"))?;
                    let upper =
                        !q.is_empty() && q.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
                    if !upper
                        || !field
                            .split('.')
                            .all(|s| s.chars().next().is_some_and(|c| c.is_ascii_lowercase()) && name_ok(s))
                    {
                        return Err(format!("'{pair}': write QUESTION=field, e.g. COMPANY=title or JOB_URL=item.url"));
                    }
                    pairs.push(format!("{q} = \"{{{field}}}\""));
                }
                Ok(pairs.join(", "))
            }
        }
    }
}

/// Text inside a TOML basic string.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(kind: Kind) -> Question {
        Question {
            name: "X".into(),
            prompt: "x".into(),
            kind,
            required: false,
            default: None,
            choices: vec![],
            help: None,
        }
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
        assert_eq!(q(Kind::Text).check(r#"say "hi" \o/"#), Ok(r#"say \"hi\" \\o/"#.into()));
        assert!(q(Kind::Text).check("a {field}").is_err());
        assert_eq!(q(Kind::Name).check(" jobs "), Ok("jobs".into()));
        assert!(q(Kind::Name).check("Jobs").is_err());
        assert_eq!(q(Kind::Number).check("3"), Ok("3".into()));
        for bad in ["0", "-1", "x", "100001"] {
            assert!(q(Kind::Number).check(bad).is_err(), "{bad}");
        }
        assert_eq!(q(Kind::List).check("offer, rejected,"), Ok(r#""offer", "rejected""#.into()));
        assert!(q(Kind::List).check(" , ").is_err());
        assert_eq!(
            q(Kind::Pairs).check("COMPANY=title, JOB_URL=item.url"),
            Ok(r#"COMPANY = "{title}", JOB_URL = "{item.url}""#.into())
        );
        assert!(q(Kind::Pairs).check("company=title").is_err());
        assert!(q(Kind::Pairs).check("COMPANY").is_err());
        let choice = Question { choices: vec!["low".into(), "high".into()], ..q(Kind::Choice) };
        assert!(choice.check("high").is_ok() && choice.check("loud").is_err());
    }

    #[test]
    fn filling_substitutes_answers_and_drops_lines_for_empty_ones() {
        let t = Template::parse(
            "t",
            r#"
[template]
label = "T"
description = "d"
category = "messages"
good_to_know = []
[[question]]
name = "C"
prompt = "c"
kind = "name"
required = true
[[question]]
name = "LANE"
prompt = "lane"
kind = "name"
[[rule]]
text = '''
on = "${C}"
lane = "${LANE}" # if LANE
lane = "any" # unless LANE
'''
[[rule]]
suffix = "two"
text = "x"
"#,
        )
        .unwrap();
        let mut values = Map::new();
        assert!(t.answers(&values).unwrap_err().contains("C (c) is required"));
        values.insert("C".into(), json!("jobs"));
        let filled = t.fill("mine", &t.answers(&values).unwrap());
        assert_eq!(filled[0].0, "mine");
        assert!(filled[0].1.contains("on = \"jobs\"\nlane = \"any\"\n"), "{}", filled[0].1);
        assert_eq!(filled[1].0, "mine-two");
        values.insert("LANE".into(), json!("notify"));
        let filled = t.fill("mine", &t.answers(&values).unwrap());
        assert!(filled[0].1.contains("lane = \"notify\"\n") && !filled[0].1.contains("any"), "{}", filled[0].1);
        values.insert("NOPE".into(), json!("x"));
        assert!(t.answers(&values).unwrap_err().contains("no question 'NOPE'"));
        assert!(Template::parse("b", "[template]\nlabel=\"\"\ndescription=\"\"\ncategory=\"messages\"\ngood_to_know=[]\n[[rule]]\ntext=\"${Q}\"\n")
            .unwrap_err()
            .contains("${Q} isn't one of its questions"));
    }
}
