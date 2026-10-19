//! Text templates with `{field}` and `[optional part]`: a record's title (ADR 0017 §2, ADR 0024 §5)
//! and an automation's params (ADR 0035 §1). In `core` so both read the same way (ADR 0035 §5.4).
//!
//! `"Solved: {title}[ ({difficulty})]"`: text, a field, and a part left out unless every field
//! inside it is set. A bracket or brace that isn't part of that syntax is an error: there is no
//! escaping. A field name is a name (`title`) or a dotted path of names (`item.url`).

use serde_json::Value;

use crate::ids::is_valid_name;

/// A piece of a template.
#[derive(Clone, Debug, PartialEq)]
enum Part {
    Text(String),
    Field(String),
    /// `[ by {author}]`: left out unless every field inside is set.
    Optional(Vec<Part>),
}

/// A parsed template, to fill as many times as needed.
#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    parts: Vec<Part>,
}

impl Template {
    /// Every problem is named with the template it's in.
    pub fn parse(template: &str) -> Result<Self, String> {
        let mut parts = Vec::new();
        let mut rest = template;
        while let Some(open) = rest.find(['[', ']']) {
            if rest[open..].starts_with(']') {
                return Err(format!("unmatched ']' in \"{template}\""));
            }
            parts.extend(plain_parts(&rest[..open], template)?);
            let after = &rest[open + 1..];
            let close = after.find(']').ok_or_else(|| format!("unmatched '[' in \"{template}\""))?;
            let inner = &after[..close];
            if inner.contains('[') {
                return Err(format!("optional parts can't be nested in \"{template}\""));
            }
            let inner = plain_parts(inner, template)?;
            if !inner.iter().any(|p| matches!(p, Part::Field(_))) {
                return Err(format!("an optional part needs a {{field}} inside in \"{template}\""));
            }
            parts.push(Part::Optional(inner));
            rest = &after[close + 1..];
        }
        parts.extend(plain_parts(rest, template)?);
        Ok(Self { parts })
    }

    /// Every field the template names, in order, optional ones included.
    pub fn fields(&self) -> Vec<&str> {
        let mut out = Vec::new();
        for part in &self.parts {
            match part {
                Part::Field(name) => out.push(name.as_str()),
                Part::Optional(inner) => {
                    out.extend(
                        inner.iter().filter_map(|p| if let Part::Field(n) = p { Some(n.as_str()) } else { None }),
                    )
                }
                Part::Text(_) => {}
            }
        }
        out
    }

    /// The filled text, trimmed, or `None` when no field is set or it comes out empty: a record's
    /// title falls back to its id then (ADR 0017 §2). `value` gives a field's text, `None` if unset.
    pub fn render_with(&self, value: impl Fn(&str) -> Option<String>) -> Option<String> {
        let (out, any_set) = self.fill_with(&value);
        let out = out.trim();
        (any_set && !out.is_empty()).then(|| out.to_owned())
    }

    /// The filled text, always: a field that isn't set is left out (an automation's message, ADR
    /// 0035 §1, says what it says even when an event lacks a field).
    pub fn fill(&self, data: &Value) -> String {
        self.fill_with(&|name: &str| lookup(data, name).and_then(text_of)).0
    }

    fn fill_with(&self, value: &dyn Fn(&str) -> Option<String>) -> (String, bool) {
        let mut any_set = false;
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Text(t) => out.push_str(t),
                Part::Field(name) => {
                    if let Some(v) = value(name) {
                        any_set = true;
                        out.push_str(&v);
                    }
                }
                Part::Optional(inner) => {
                    let all_set = inner.iter().all(|p| !matches!(p, Part::Field(name) if value(name).is_none()));
                    if all_set {
                        for p in inner {
                            match p {
                                Part::Field(name) => {
                                    any_set = true;
                                    out.push_str(&value(name).unwrap_or_default());
                                }
                                Part::Text(t) => out.push_str(t),
                                Part::Optional(_) => {}
                            }
                        }
                    }
                }
            }
        }
        (out, any_set)
    }
}

/// A value as template text: `None` when unset (null, an empty text or an empty list); a list as
/// its items joined with ", " (ADR 0024 §2); text as itself; anything else as JSON.
pub fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(s.clone()),
        Value::Array(items) if items.is_empty() => None,
        Value::Array(items) => Some(
            items
                .iter()
                .map(|i| i.as_str().map_or_else(|| i.to_string(), str::to_owned))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        other => Some(other.to_string()),
    }
}

/// `data`'s value at a dotted path (`item.url`), if there is one.
pub fn lookup<'a>(data: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(data, |v, key| v.get(key))
}

/// Text and `{field}` parts of a piece of a template with no brackets.
fn plain_parts(piece: &str, template: &str) -> Result<Vec<Part>, String> {
    let mut parts = Vec::new();
    let mut rest = piece;
    while let Some(open) = rest.find(['{', '}']) {
        if rest[open..].starts_with('}') {
            return Err(format!("unmatched '}}' in \"{template}\""));
        }
        if open > 0 {
            parts.push(Part::Text(rest[..open].to_owned()));
        }
        let after = &rest[open + 1..];
        let close = after.find('}').ok_or_else(|| format!("unmatched '{{' in \"{template}\""))?;
        let name = &after[..close];
        if name.contains('{') || !name.split('.').all(is_valid_name) {
            return Err(format!("'{{{name}}}' is not a field name"));
        }
        parts.push(Part::Field(name.to_owned()));
        rest = &after[close + 1..];
    }
    if !rest.is_empty() {
        parts.push(Part::Text(rest.to_owned()));
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn fields_and_optional_parts() {
        let t = Template::parse("Solved: {title}[ ({difficulty})]").unwrap();
        assert_eq!(t.fields(), ["title", "difficulty"]);
        assert_eq!(t.fill(&json!({"title": "Two Sum", "difficulty": "easy"})), "Solved: Two Sum (easy)");
        assert_eq!(t.fill(&json!({"title": "Two Sum"})), "Solved: Two Sum", "the optional part needs its field");
        assert_eq!(t.fill(&json!({})), "Solved: ", "fill always gives text");
    }

    #[test]
    fn render_is_none_when_nothing_is_set() {
        let t = Template::parse("{a} - {b}").unwrap();
        let v = |pairs: &[(&str, &str)]| {
            let m: std::collections::BTreeMap<String, String> =
                pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            t.render_with(move |n| m.get(n).cloned())
        };
        assert_eq!(v(&[]), None);
        assert_eq!(v(&[("a", "x")]), Some("x -".into()), "trimmed");
        assert_eq!(v(&[("a", "x"), ("b", "y")]), Some("x - y".into()));
    }

    #[test]
    fn dotted_paths_reach_into_values() {
        let event = json!({"title": "Acme", "item": {"url": "https://x.y", "tags": ["a", "b"], "n": 3}});
        let t = Template::parse("{title}: {item.url} [{item.tags}] {item.n}").unwrap();
        assert_eq!(t.fill(&event), "Acme: https://x.y a, b 3");
        assert_eq!(lookup(&event, "item.n"), Some(&json!(3)));
        assert_eq!(lookup(&event, "item.nope"), None);
        assert_eq!(text_of(&json!("")), None);
        assert_eq!(text_of(&json!([])), None);
    }

    #[test]
    fn mistakes_are_named() {
        for (bad, want) in [
            ("a]", "unmatched ']'"),
            ("[a", "unmatched '['"),
            ("[[{a}]]", "can't be nested"),
            ("[text]", "needs a {field}"),
            ("a}", "unmatched '}'"),
            ("{a", "unmatched '{'"),
            ("{A B}", "is not a field name"),
            ("{a..b}", "is not a field name"),
        ] {
            assert!(Template::parse(bad).unwrap_err().contains(want), "{bad}");
        }
    }
}
