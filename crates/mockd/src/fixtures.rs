//! Fixtures: what the mock daemon knows how to say. Loading, validation and request matching,
//! with no sockets and no clock, so all of it is testable as plain data.
//!
//! A fixture file is a JSON object with two optional lists (format documented in
//! `crates/mockd/fixtures/README.md`):
//!
//! * `responses`: rules `{op, params?, queue?, data | error, delay_ms?, emit?}`. The first rule
//!   whose `op` matches and whose `params` and `queue` are subsets of the request's wins. Files
//!   are read in filename order, so precedence is deterministic.
//! * `timeline`: events replayed once per connection, starting at its first `subscribe`.
//!
//! Everything is checked at load time and reported with the file name: a typo in a fixture
//! should fail `shimmer mockd` at startup, not surface as a puzzling client bug.

use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;
use shimmer_core::Error;
use shimmer_proto::ManifestData;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    responses: Vec<Rule>,
    #[serde(default)]
    timeline: Vec<TimedEvent>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Canonical op name. Aliases never appear on the wire (protocol.md).
    pub op: String,
    /// Matches when every key given here is present, with a matching value, in the request's
    /// params (recursively). Absent = matches any params.
    #[serde(default)]
    pub params: Option<Value>,
    /// The same, against the request's `queue` block. A request with no `queue` block counts as
    /// the defaults: `{"priority":"normal","confirm":false}`.
    #[serde(default)]
    pub queue: Option<Value>,
    /// Response `data`. Exactly one of `data` and `error` is required.
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default)]
    pub error: Option<Error>,
    /// Hold the response back this long.
    #[serde(default)]
    pub delay_ms: u64,
    /// Events pushed after the response, `after_ms` counted from the moment it is sent.
    #[serde(default)]
    pub emit: Vec<TimedEvent>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimedEvent {
    #[serde(default)]
    pub after_ms: u64,
    pub topic: String,
    /// Defaults to the topic's first segment, which is how real events are attributed.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub payload: Value,
}

impl TimedEvent {
    pub fn source(&self) -> &str {
        self.source.as_deref().unwrap_or_else(|| self.topic.split('.').next().unwrap_or(""))
    }
}

pub enum Lookup<'a> {
    Hit(&'a Rule),
    /// The op has fixtures, but none for these params or queue control. Distinct from an
    /// unknown op so that a gap in a fixture is not mistaken for a missing op.
    NoMatch,
    UnknownOp,
}

#[derive(Debug, Default)]
pub struct Fixtures {
    rules: Vec<Rule>,
    timeline: Vec<TimedEvent>,
}

impl Fixtures {
    /// Every `*.json` directly inside `dir`, in filename order. Other files are ignored so a
    /// README can sit alongside.
    pub fn load(dir: &Path) -> Result<Self, Error> {
        let entries = fs::read_dir(dir)
            .map_err(|e| Error::invalid_params(format!("cannot read fixtures directory {}: {e}", dir.display())))?;
        let mut files = Vec::new();
        for entry in entries {
            let path = entry.map_err(Error::from)?.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "json") {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let text = fs::read_to_string(&path)
                    .map_err(|e| Error::invalid_params(format!("cannot read fixture {name}: {e}")))?;
                files.push((name, text));
            }
        }
        files.sort();
        if files.is_empty() {
            return Err(Error::invalid_params(format!("no *.json fixtures in {}", dir.display())));
        }
        Self::parse(files)
    }

    /// `(file name, contents)` pairs, in precedence order.
    pub fn parse(files: Vec<(String, String)>) -> Result<Self, Error> {
        let mut out = Self::default();
        for (name, text) in files {
            let file: File =
                serde_json::from_str(&text).map_err(|e| Error::invalid_params(format!("fixture {name}: {e}")))?;
            for (i, rule) in file.responses.iter().enumerate() {
                validate_rule(rule)
                    .map_err(|m| Error::invalid_params(format!("fixture {name}, responses[{i}]: {m}")))?;
            }
            for (i, ev) in file.timeline.iter().enumerate() {
                validate_event(ev).map_err(|m| Error::invalid_params(format!("fixture {name}, timeline[{i}]: {m}")))?;
            }
            out.rules.extend(file.responses);
            out.timeline.extend(file.timeline);
        }
        Ok(out)
    }

    pub fn lookup(&self, op: &str, params: &Value, queue: &Value) -> Lookup<'_> {
        let mut seen = false;
        // A request with no params decodes as null; treat it as `{}` so a rule can still match.
        let params = if params.is_null() { &Value::Object(Default::default()) } else { params };
        for rule in self.rules.iter().filter(|r| r.op == op) {
            seen = true;
            let ok = |pattern: &Option<Value>, actual: &Value| pattern.as_ref().is_none_or(|p| subset(p, actual));
            if ok(&rule.params, params) && ok(&rule.queue, queue) {
                return Lookup::Hit(rule);
            }
        }
        if seen {
            Lookup::NoMatch
        } else {
            Lookup::UnknownOp
        }
    }

    pub fn timeline(&self) -> &[TimedEvent] {
        &self.timeline
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.timeline.is_empty()
    }
}

/// Objects match by containment, recursively; everything else by equality.
fn subset(pattern: &Value, actual: &Value) -> bool {
    match (pattern, actual) {
        (Value::Object(p), Value::Object(a)) => p.iter().all(|(k, v)| a.get(k).is_some_and(|x| subset(v, x))),
        (p, a) => p == a,
    }
}

fn validate_rule(rule: &Rule) -> Result<(), String> {
    if !is_canonical_op(&rule.op) {
        return Err(format!("op '{}' is not a canonical '<module>.<verb>' name", rule.op));
    }
    match (&rule.data, &rule.error) {
        (Some(_), Some(_)) => return Err("has both `data` and `error`; a response is one or the other".into()),
        (None, None) => return Err("needs `data` or `error` (`data` cannot be null; use {} for empty)".into()),
        _ => {}
    }
    if rule.queue.as_ref().is_some_and(|q| !q.is_object()) {
        return Err("`queue` must be an object".into());
    }
    // Where the wire type is fixed by the protocol, hold the fixture to it.
    if rule.op == "core.manifest" {
        if let Some(data) = &rule.data {
            serde_json::from_value::<ManifestData>(data.clone())
                .map_err(|e| format!("`data` is not a valid core.manifest response: {e}"))?;
        }
    }
    rule.emit.iter().enumerate().try_for_each(|(i, e)| validate_event(e).map_err(|m| format!("emit[{i}]: {m}")))
}

fn validate_event(ev: &TimedEvent) -> Result<(), String> {
    let segments: Vec<&str> = ev.topic.split('.').collect();
    if segments.len() < 2 || segments.iter().any(|s| s.is_empty() || s.contains('*')) {
        return Err(format!("topic '{}' must be a concrete '<module>.<entity>.<verb>' name", ev.topic));
    }
    if ev.source.as_deref().is_some_and(str::is_empty) {
        return Err("`source` cannot be empty".into());
    }
    Ok(())
}

fn is_canonical_op(op: &str) -> bool {
    matches!(op.split_once('.'), Some((m, v)) if !m.is_empty() && !v.is_empty())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;
    use shimmer_core::ErrorCode;

    use super::*;

    fn one(text: &str) -> Result<Fixtures, Error> {
        Fixtures::parse(vec![("t.json".into(), text.into())])
    }

    fn msg(text: &str) -> String {
        one(text).unwrap_err().message
    }

    fn shipped() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
    }

    #[test]
    fn matching_is_by_containment_and_first_rule_wins() {
        let f = one(r#"{"responses":[
            {"op":"a.b","params":{"x":1,"deep":{"k":true}},"data":{"which":"specific"}},
            {"op":"a.b","data":{"which":"general"}}
        ]}"#)
        .unwrap();
        let which = |params: Value| match f.lookup("a.b", &params, &json!({})) {
            Lookup::Hit(r) => r.data.clone().unwrap()["which"].as_str().unwrap().to_owned(),
            _ => "miss".into(),
        };
        assert_eq!(which(json!({"x": 1, "deep": {"k": true, "extra": 2}, "more": 3})), "specific");
        assert_eq!(which(json!({"x": 2})), "general", "falls through to the next rule");
        assert_eq!(which(Value::Null), "general", "absent params match a rule with none");
    }

    #[test]
    fn a_gap_in_a_fixture_is_not_an_unknown_op() {
        let f = one(r#"{"responses":[{"op":"a.b","params":{"x":1},"data":{}}]}"#).unwrap();
        assert!(matches!(f.lookup("a.b", &json!({"x": 2}), &json!({})), Lookup::NoMatch));
        assert!(matches!(f.lookup("a.c", &json!({}), &json!({})), Lookup::UnknownOp));
        assert!(matches!(f.lookup("a.b", &Value::Null, &json!({})), Lookup::NoMatch), "null params are {{}}");
    }

    #[test]
    fn queue_control_is_matched_like_params() {
        let f = one(r#"{"responses":[
            {"op":"a.b","queue":{"priority":"override","confirm":true,"queue_version":41},"data":{"r":"granted"}},
            {"op":"a.b","queue":{"priority":"override"},"data":{"r":"asked"}}
        ]}"#)
        .unwrap();
        let r = |q: Value| match f.lookup("a.b", &json!({}), &q) {
            Lookup::Hit(r) => r.data.clone().unwrap()["r"].as_str().unwrap().to_owned(),
            _ => "miss".into(),
        };
        assert_eq!(r(json!({"priority": "override", "confirm": true, "queue_version": 41})), "granted");
        assert_eq!(r(json!({"priority": "override", "confirm": true, "queue_version": 40})), "asked");
        assert_eq!(r(json!({"priority": "normal", "confirm": false})), "miss");
    }

    #[test]
    fn bad_fixtures_are_rejected_with_the_file_and_the_reason() {
        let cases = [
            (r#"{"responses":[{"op":"bankai","data":{}}]}"#, "canonical"),
            (r#"{"responses":[{"op":"a.","data":{}}]}"#, "canonical"),
            (r#"{"responses":[{"op":"a.b"}]}"#, "needs `data` or `error`"),
            (r#"{"responses":[{"op":"a.b","data":null}]}"#, "needs `data` or `error`"),
            (
                r#"{"responses":[{"op":"a.b","data":{},"error":{"code":"internal","message":"x"}}]}"#,
                "both `data` and `error`",
            ),
            (r#"{"responses":[{"op":"a.b","error":{"code":"nope","message":"x"}}]}"#, "unknown variant"),
            (r#"{"responses":[{"op":"a.b","data":{},"delay":5}]}"#, "unknown field"),
            (r#"{"responses":[{"op":"a.b","queue":[],"data":{}}]}"#, "`queue` must be an object"),
            (r#"{"responses":[{"op":"core.manifest","data":{"protocol":1}}]}"#, "core.manifest"),
            (r#"{"responses":[{"op":"a.b","data":{},"emit":[{"topic":"queue.*"}]}]}"#, "concrete"),
            (r#"{"responses":[{"op":"a.b","data":{},"emit":[{"topic":"solo"}]}]}"#, "concrete"),
            (r#"{"timeline":[{"topic":"a..b"}]}"#, "concrete"),
            (r#"{"timeline":[{"after_ms":-5,"topic":"a.b.c"}]}"#, "invalid value"),
            (r#"{"respones":[]}"#, "unknown field"),
            ("not json", "expected"),
        ];
        for (text, needle) in cases {
            let m = msg(text);
            assert!(m.contains("t.json") && m.contains(needle), "{text}\n  -> {m}");
        }
        assert_eq!(one(r#"{"responses":[{"op":"a.b"}]}"#).unwrap_err().code, ErrorCode::InvalidParams);
    }

    #[test]
    fn events_default_their_source_to_the_topics_module() {
        let f =
            one(r#"{"timeline":[{"topic":"queue.task.started"},{"topic":"queue.task.failed","source":"x"}]}"#).unwrap();
        let sources: Vec<_> = f.timeline().iter().map(|e| e.source()).collect();
        assert_eq!(sources, ["queue", "x"]);
    }

    #[test]
    fn load_reads_json_files_in_name_order_and_ignores_the_rest() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join("b.json"), r#"{"responses":[{"op":"a.b","data":{"from":"b"}}]}"#).unwrap();
        fs::write(dir.path().join("a.json"), r#"{"responses":[{"op":"a.b","data":{"from":"a"}}]}"#).unwrap();
        fs::write(dir.path().join("README.md"), "not a fixture {{{").unwrap();
        let f = Fixtures::load(dir.path()).unwrap();
        let Lookup::Hit(r) = f.lookup("a.b", &json!({}), &json!({})) else { panic!() };
        assert_eq!(r.data.as_ref().unwrap()["from"], "a", "a.json is read before b.json");

        let empty = tempfile::TempDir::new().unwrap();
        assert!(Fixtures::load(empty.path()).unwrap_err().message.contains("no *.json"));
        assert!(Fixtures::load(&empty.path().join("missing")).unwrap_err().message.contains("cannot read"));
    }

    /// The fixtures Dev C builds against must themselves be valid, and must cover the four
    /// paths `protocol.md` says are hardest to reach on a real daemon.
    #[test]
    fn shipped_fixtures_load_and_cover_the_paths_protocol_md_requires() {
        let f = Fixtures::load(&shipped()).unwrap();
        let has = |op: &str, code: Option<ErrorCode>| {
            f.rules.iter().any(|r| r.op == op && r.error.as_ref().map(|e| e.code) == code)
        };
        assert!(has("core.manifest", None));
        assert!(has("fetchers.run", Some(ErrorCode::ConfirmationRequired)), "confirmation round trip");
        assert!(has("workspaces.activate", Some(ErrorCode::WorkspaceDirty)), "workspace_dirty");
        assert!(f.timeline().iter().any(|e| e.topic == "core.stream.lagged"), "stream lag");
        let progress = f.rules.iter().flat_map(|r| &r.emit).filter(|e| e.topic == "queue.task.progress").count();
        assert!(progress >= 3, "a long queued task that reports progress");
    }

    /// Task ids are ULIDs on the wire; a client that parses them must not choke on a fixture.
    #[test]
    fn shipped_fixtures_use_real_ulids_for_ids() {
        fn walk(v: &Value, bad: &mut Vec<String>) {
            match v {
                Value::Object(m) => {
                    for (k, v) in m {
                        if matches!(k.as_str(), "task_id" | "before") {
                            if let Some(s) = v.as_str() {
                                if ulid::Ulid::from_string(s).is_err() {
                                    bad.push(s.to_owned());
                                }
                            }
                        }
                        walk(v, bad);
                    }
                }
                Value::Array(a) => a.iter().for_each(|v| walk(v, bad)),
                _ => {}
            }
        }
        let mut bad = Vec::new();
        for entry in fs::read_dir(shipped()).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "json") {
                walk(&serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap(), &mut bad);
            }
        }
        assert!(bad.is_empty(), "not ULIDs: {bad:?}");
    }
}
