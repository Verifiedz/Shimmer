//! One record: its file on disk and its shape on the wire. Pure functions (§12 rule 10).
//!
//! On disk, `items/<collection>/<id>.toml` is flat and hand-editable: `status` plus the field
//! values, with unset fields absent. On the wire an item is flat too: `id`, `status` and every
//! schema field, unset ones as `null`. That is the shape of the `records.list` mock fixture.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use shimmer_core::ids::is_valid_id;
use shimmer_core::{Error, Result};

use crate::schema::Collection;

const MAX_ID_LEN: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Todo,
    Done,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::Done => "done",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: String,
    pub status: Status,
    /// Set fields only. May include keys a hand-edit added that the schema does not know.
    pub fields: BTreeMap<String, Value>,
}

/// A record id is its file name: `[a-z0-9][a-z0-9_-]*` (`shimmer_core::ids::is_valid_id`,
/// ADR 0008), so `1-two-sum` works too.
pub fn check_id(id: &str) -> Result<()> {
    if id.len() <= MAX_ID_LEN && is_valid_id(id) {
        Ok(())
    } else {
        Err(Error::invalid_params(format!(
            "record id '{id}' must be lowercase letters, digits, '-' or '_' (at most {MAX_ID_LEN})"
        )))
    }
}

impl Item {
    pub fn new(id: &str, fields: Map<String, Value>) -> Self {
        Self { id: id.to_owned(), status: Status::Todo, fields: fields.into_iter().collect() }
    }

    /// Read a record file. Lenient on purpose: a hand-edited file with an unknown key or a native
    /// TOML date still reads, and is only validated again when it is next written.
    pub fn from_toml(id: &str, text: &str) -> Result<Self> {
        let bad = |msg: String| Error::module_error(format!("record file '{id}.toml': {msg}"));
        let mut table: toml::Table = toml::from_str(text).map_err(|e| bad(e.to_string()))?;
        let status = match table.remove("status") {
            None => Status::Todo,
            Some(toml::Value::String(s)) if s == "todo" => Status::Todo,
            Some(toml::Value::String(s)) if s == "done" => Status::Done,
            Some(other) => return Err(bad(format!("status must be \"todo\" or \"done\", got {other}"))),
        };
        table.remove("id");
        let fields = table.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect();
        Ok(Self { id: id.to_owned(), status, fields })
    }

    pub fn to_toml(&self) -> String {
        let mut table = toml::Table::new();
        table.insert("status".into(), toml::Value::String(self.status.as_str().into()));
        for (k, v) in &self.fields {
            if let Some(v) = json_to_toml(v) {
                table.insert(k.clone(), v);
            }
        }
        toml::to_string(&table).unwrap_or_default()
    }

    /// The wire shape: `id`, `status`, every schema field (`null` when unset), then anything
    /// else the file holds.
    pub fn to_wire(&self, c: &Collection) -> Value {
        let mut out = Map::new();
        out.insert("id".into(), Value::String(self.id.clone()));
        out.insert("status".into(), Value::String(self.status.as_str().into()));
        for f in &c.fields {
            out.insert(f.name.clone(), self.fields.get(&f.name).cloned().unwrap_or(Value::Null));
        }
        for (k, v) in &self.fields {
            out.entry(k.clone()).or_insert_with(|| v.clone());
        }
        Value::Object(out)
    }

    /// Set each value, or unset it where the value is `null`.
    pub fn apply(&mut self, changes: Map<String, Value>) {
        for (k, v) in changes {
            if v.is_null() {
                self.fields.remove(&k);
            } else {
                self.fields.insert(k, v);
            }
        }
    }

    pub fn fields_map(&self) -> Map<String, Value> {
        self.fields.clone().into_iter().collect()
    }
}

/// Exact equality on every key of `filter`, against the wire shape; `null` matches unset.
pub fn matches(wire: &Value, filter: &Map<String, Value>) -> bool {
    filter.iter().all(|(k, want)| wire.get(k).unwrap_or(&Value::Null) == want)
}

/// TOML to JSON. Dates become `"YYYY-MM-DD"`-style strings, the form the schema checks.
pub fn toml_to_json(v: toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => Value::from(i),
        toml::Value::Float(f) => Value::from(f),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(a) => Value::Array(a.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => Value::Object(t.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect()),
    }
}

/// JSON to TOML. `null` has no TOML form and yields `None`: an unset field is an absent key.
fn json_to_toml(v: &Value) -> Option<toml::Value> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => toml::Value::Integer(i),
            None => toml::Value::Float(n.as_f64()?),
        },
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Array(a) => toml::Value::Array(a.iter().filter_map(json_to_toml).collect()),
        Value::Object(o) => {
            toml::Value::Table(o.iter().filter_map(|(k, v)| Some((k.clone(), json_to_toml(v)?))).collect())
        }
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn leetcode() -> Collection {
        Collection::parse("leetcode", include_str!("../collections/leetcode.toml")).unwrap()
    }

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn ids() {
        for ok in ["two-sum", "1-two-sum", "lru_cache", "a"] {
            assert!(check_id(ok).is_ok(), "{ok}");
        }
        for bad in ["", "Two-Sum", "-x", "a/b", "../x", "a b", &"x".repeat(129)] {
            assert!(check_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn file_round_trip_drops_nulls() {
        let mut item = Item::new("two-sum", obj(json!({"title": "Two Sum", "difficulty": "easy"})));
        item.status = Status::Done;
        let text = item.to_toml();
        assert!(text.contains("status = \"done\""), "{text}");
        assert_eq!(Item::from_toml("two-sum", &text).unwrap(), item);

        item.apply(obj(json!({"difficulty": null, "url": "https://leetcode.com/problems/two-sum"})));
        assert!(!item.fields.contains_key("difficulty"));
        assert!(!item.to_toml().contains("difficulty"));
    }

    #[test]
    fn hand_edited_files_read_leniently() {
        let item = Item::from_toml("x", "title = \"X\"\nlast_solved = 2026-09-18\nextra = 1\n").unwrap();
        assert_eq!(item.status, Status::Todo);
        assert_eq!(item.fields["last_solved"], json!("2026-09-18"));
        assert_eq!(item.fields["extra"], json!(1));

        assert!(Item::from_toml("x", "status = \"maybe\"").unwrap_err().message.contains("'x.toml'"));
        assert!(Item::from_toml("x", "not toml [").is_err());
    }

    #[test]
    fn wire_shape_matches_the_mock_fixture() {
        let item = Item::new("lru-cache", obj(json!({"title": "LRU Cache", "difficulty": "medium"})));
        assert_eq!(
            item.to_wire(&leetcode()),
            json!({"id": "lru-cache", "status": "todo", "title": "LRU Cache", "difficulty": "medium",
                   "url": null, "last_solved": null})
        );
    }

    #[test]
    fn filters_are_exact_and_null_means_unset() {
        let wire = Item::new("a", obj(json!({"title": "A", "difficulty": "easy"}))).to_wire(&leetcode());
        assert!(matches(&wire, &obj(json!({"status": "todo", "difficulty": "easy"}))));
        assert!(matches(&wire, &obj(json!({"last_solved": null}))));
        assert!(!matches(&wire, &obj(json!({"difficulty": "hard"}))));
        assert!(matches(&wire, &Map::new()));
    }
}
