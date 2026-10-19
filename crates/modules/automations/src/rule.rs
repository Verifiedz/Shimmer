//! One rule (ADR 0035 §1): its file, checked; whether an event matches it; its actions' params
//! filled from that event. Pure: no `Ctx`, no I/O (§12 rule 10).

use chrono::{Duration, NaiveDate};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{Map, Value};
use shimmer_core::filter::Filter;
use shimmer_core::ids::is_valid_id;
use shimmer_core::text::{lookup, Template};
use shimmer_core::topic::{is_valid_pattern, topic_matches};
use shimmer_core::Catalog;

/// At most this many actions in one rule: enough for any real rule, a cap on a pasted mistake.
pub const MAX_ACTIONS: usize = 10;
/// The longest wait an action may have (§4).
pub(crate) const MAX_AFTER_DAYS: i64 = 366;
/// Your rules can't react to these, nor run these ops (§3, §6).
const OWN: &str = "automations.";

/// A checked rule.
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    pub on: String,
    when: Option<Filter>,
    pub actions: Vec<Action>,
    /// One of Shimmer's own (§10): may react to `automations.*`, can't be removed.
    pub builtin: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Action {
    pub op: String,
    /// As written: `{field}` parts are filled when the rule fires.
    pub params: Value,
    /// `after = "3d"` (§4): becomes a one-time schedule instead of a task now.
    pub after: Option<Duration>,
}

/// The file as written. Unknown keys are mistakes, refused.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    rule: Head,
    #[serde(rename = "do", default)]
    actions: Vec<ActionFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Head {
    #[serde(default)]
    label: String,
    enabled: Option<bool>,
    on: String,
    when: Option<toml::Table>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionFile {
    op: String,
    #[serde(default)]
    params: toml::Table,
    after: Option<String>,
}

impl Rule {
    /// The rule `id`'s file text, checked against what the daemon has (`catalog`). Every problem
    /// is a sentence naming what's wrong.
    pub fn parse(id: &str, text: &str, catalog: &Catalog, builtin: bool) -> Result<Self, String> {
        if !is_valid_id(id) {
            return Err(format!("'{id}' isn't a rule id: lowercase letters, digits, '-' and '_'"));
        }
        let file: File = toml::from_str(text).map_err(|e| format!("not a rule file: {}", e.message()))?;
        let on = file.rule.on.trim().to_owned();
        if !is_valid_pattern(&on) {
            return Err(format!("on: '{on}' isn't a topic or pattern (records.item.created, records.item.*, *)"));
        }
        if !builtin && on.starts_with(OWN) {
            return Err(format!("on: rules can't react to automations' own events ('{on}')"));
        }
        if !on.contains('*') && !catalog.has_topic(&on) {
            return Err(format!("on: no event '{on}' (shimmer automations events lists them)"));
        }
        let when = match &file.rule.when {
            None => None,
            Some(table) => {
                let Value::Object(map) = json(table) else { unreachable!("a table is an object") };
                Some(Filter::parse(&map).map_err(|m| format!("when: {m}"))?)
            }
        };
        if file.actions.is_empty() {
            return Err("a rule needs at least one [[do]] action".into());
        }
        if file.actions.len() > MAX_ACTIONS {
            return Err(format!("a rule has at most {MAX_ACTIONS} actions"));
        }
        let actions = file
            .actions
            .into_iter()
            .enumerate()
            .map(|(i, a)| action(a, catalog).map_err(|m| format!("action {}: {m}", i + 1)))
            .collect::<Result<_, _>>()?;
        let label = match file.rule.label.trim() {
            "" => id.to_owned(),
            label => label.to_owned(),
        };
        Ok(Self { id: id.to_owned(), label, enabled: file.rule.enabled.unwrap_or(true), on, when, actions, builtin })
    }

    /// Does `topic` with `payload` set this rule off? `today` and `tz` resolve `today+N`.
    pub fn matches(&self, topic: &str, payload: &Value, today: NaiveDate, tz: Tz) -> bool {
        self.enabled
            && (self.builtin || !topic.starts_with(OWN))
            && topic_matches(&self.on, topic)
            && self.when.as_ref().is_none_or(|w| w.matches(payload, today, tz))
    }
}

fn action(a: ActionFile, catalog: &Catalog) -> Result<Action, String> {
    let op = a.op.trim().to_owned();
    if op.starts_with(OWN) {
        return Err(format!("a rule can't run automations' own ops ('{op}')"));
    }
    if catalog.op(&op).is_none() {
        return Err(format!("no op '{op}' (use the canonical name, e.g. notify.send)"));
    }
    let params = json(&a.params);
    check_templates(&params)?;
    let after = a.after.as_deref().map(wait).transpose()?;
    Ok(Action { op, params, after })
}

/// TOML as JSON: what ops take.
fn json(table: &toml::Table) -> Value {
    serde_json::to_value(table).unwrap_or_default()
}

/// Every text in the params is a valid template, so a typo is refused now, not when it fires.
fn check_templates(v: &Value) -> Result<(), String> {
    match v {
        Value::String(s) => Template::parse(s).map(drop).map_err(|m| format!("params: {m}")),
        Value::Array(items) => items.iter().try_for_each(check_templates),
        Value::Object(m) => m.values().try_for_each(check_templates),
        _ => Ok(()),
    }
}

/// `30m`, `2h`, `3d`, `1w`: a whole number and a unit.
pub fn wait(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let bad = || format!("after: '{s}' isn't a wait: write a number and m, h, d or w (30m, 2h, 3d, 1w)");
    let unit = s.chars().last().ok_or_else(bad)?;
    let n: i64 = s[..s.len() - unit.len_utf8()].parse().map_err(|_| bad())?;
    // `try_*`: a huge number is refused like any too-long wait, never a panic (#252's review).
    let d = match unit {
        'm' => Duration::try_minutes(n),
        'h' => Duration::try_hours(n),
        'd' => Duration::try_days(n),
        'w' => Duration::try_weeks(n),
        _ => return Err(bad()),
    };
    match d {
        Some(d) if n >= 1 && d <= Duration::days(MAX_AFTER_DAYS) => Ok(d),
        _ => Err(format!("after: '{s}' must be between 1m and {MAX_AFTER_DAYS}d")),
    }
}

/// `params` with `{field}`s filled from the event's `payload` (§1): a text that is exactly one
/// `{field}` takes that value with its type (a number stays a number, a list a list; missing is
/// `null`); any other text is filled in as text.
pub fn fill(params: &Value, payload: &Value) -> Value {
    match params {
        Value::String(s) => match whole_field(s) {
            Some(path) => lookup(payload, path).cloned().unwrap_or(Value::Null),
            None => Template::parse(s).map_or_else(|_| params.clone(), |t| Value::String(t.fill(payload))),
        },
        Value::Array(items) => Value::Array(items.iter().map(|i| fill(i, payload)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), fill(v, payload))).collect::<Map<_, _>>()),
        other => other.clone(),
    }
}

/// `item.url` when `s` is exactly `"{item.url}"`.
fn whole_field(s: &str) -> Option<&str> {
    let inner = s.strip_prefix('{')?.strip_suffix('}')?;
    let t = Template::parse(s).ok()?;
    (t.fields() == [inner]).then_some(inner)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use shimmer_core::{CommandSpec, Execution};

    use super::*;

    pub(crate) fn catalog() -> Catalog {
        let op = |op: &str| CommandSpec {
            op: op.into(),
            summary: op.into(),
            params_schema: json!({}),
            execution: Execution::Inline,
        };
        let topics = ["records.item.updated", "records.item.completed", "automations.chain.stopped"];
        Catalog::new(
            ["notify.send", "workspaces.activate", "automations.disable"].map(op),
            topics.map(|t| (t.to_owned(), None)),
        )
    }

    const INTERVIEWING: &str = r#"
[rule]
label = "Interviewing: open prep and tell me"
on = "records.item.updated"
when = { collection = "jobs", "item.stage" = "interviewing" }

[[do]]
op = "workspaces.activate"
params = { id = "interview-prep" }

[[do]]
op = "notify.send"
params = { title = "Interview stage: {title}[ at {item.company}]", priority = "high", tags = "{item.tags}" }
after = "3d"
"#;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 16).unwrap()
    }

    #[test]
    fn a_rule_reads_matches_and_fills() {
        let r = Rule::parse("interview", INTERVIEWING, &catalog(), false).unwrap();
        assert_eq!((r.label.as_str(), r.enabled, r.actions.len()), ("Interviewing: open prep and tell me", true, 2));
        assert_eq!(r.actions[1].after, Some(Duration::days(3)));

        let ev = json!({"collection": "jobs", "title": "Acme", "item": {"stage": "interviewing", "tags": ["remote"]}});
        assert!(r.matches("records.item.updated", &ev, today(), Tz::UTC));
        assert!(!r.matches("records.item.completed", &ev, today(), Tz::UTC), "another topic");
        assert!(!r.matches("records.item.updated", &json!({"collection": "jobs"}), today(), Tz::UTC), "when");

        let params = fill(&r.actions[1].params, &ev);
        assert_eq!(params["title"], "Interview stage: Acme", "an optional part with nothing set is left out");
        assert_eq!(params["tags"], json!(["remote"]), "a whole {{field}} keeps its type");
        assert_eq!(params["priority"], "high");
    }

    #[test]
    fn disabled_rules_and_own_events_never_match() {
        let off = INTERVIEWING.replace("[rule]", "[rule]\nenabled = false");
        let r = Rule::parse("x", &off, &catalog(), false).unwrap();
        assert!(!r.matches(
            "records.item.updated",
            &json!({"collection": "jobs", "item": {"stage": "interviewing"}}),
            today(),
            Tz::UTC
        ));

        let any = "[rule]\non = \"*\"\n[[do]]\nop = \"notify.send\"\n";
        let r = Rule::parse("any", any, &catalog(), false).unwrap();
        assert!(r.matches("records.item.updated", &json!({}), today(), Tz::UTC));
        assert!(!r.matches("automations.rule.fired", &json!({}), today(), Tz::UTC), "§3: never its own events");
        let builtin = Rule::parse("loop", &any.replace('*', "automations.chain.stopped"), &catalog(), true).unwrap();
        assert!(builtin.matches("automations.chain.stopped", &json!({}), today(), Tz::UTC));
    }

    #[test]
    fn mistakes_are_refused_with_what_is_wrong() {
        let rule = |on: &str, rest: &str| format!("[rule]\non = \"{on}\"\n{rest}");
        let act = "[[do]]\nop = \"notify.send\"\n";
        for (id, text, want) in [
            ("Bad Id", rule("records.item.updated", act), "isn't a rule id"),
            ("a", "nonsense".to_owned(), "not a rule file"),
            ("a", rule("records.item.updated", act) + "colour = 1\n", "not a rule file"),
            ("a", rule("records..x", act), "isn't a topic or pattern"),
            ("a", rule("records.item.nope", act), "no event 'records.item.nope'"),
            ("a", rule("automations.rule.fired", act), "can't react to automations"),
            ("a", rule("records.item.updated", ""), "at least one [[do]]"),
            ("a", rule("records.item.updated", "[[do]]\nop = \"notify.nope\"\n"), "action 1: no op 'notify.nope'"),
            ("a", rule("records.item.updated", "[[do]]\nop = \"automations.disable\"\n"), "own ops"),
            ("a", rule("records.item.updated", &format!("{act}params = {{ t = \"{{a\" }}\n")), "params: unmatched"),
            ("a", rule("records.item.updated", &format!("{act}after = \"soon\"\n")), "isn't a wait"),
            ("a", rule("records.item.updated", &format!("{act}after = \"400d\"\n")), "between 1m and 366d"),
            ("a", format!("[rule]\non = \"*\"\nwhen = {{ x = {{ big = 1 }} }}\n{act}"), "when: filter on 'x'"),
            ("a", rule("*", &act.repeat(MAX_ACTIONS + 1)), "at most 10 actions"),
        ] {
            let err = Rule::parse(id, &text, &catalog(), false).unwrap_err();
            assert!(err.contains(want), "{text}\n→ {err}");
        }
    }

    #[test]
    fn waits() {
        assert_eq!(wait("30m"), Ok(Duration::minutes(30)));
        assert_eq!(wait(" 2h "), Ok(Duration::hours(2)));
        assert_eq!(wait("1w"), Ok(Duration::weeks(1)));
        for bad in ["", "d", "3", "0d", "-1d", "3 days", "3D", "367d", "99999999999999w", "9999999999999999m"] {
            assert!(wait(bad).is_err(), "{bad}");
        }
        assert!(wait("99999999999999w").unwrap_err().contains("between 1m and"), "refused, not a panic");
    }

    #[test]
    fn whole_fields_keep_their_type_and_missing_ones_are_null() {
        let ev = json!({"n": 3, "item": {"ok": true}});
        let p = json!({"a": "{n}", "b": "{item.ok}", "c": "{gone}", "d": "n={n}", "e": ["{n}"], "f": 7});
        assert_eq!(fill(&p, &ev), json!({"a": 3, "b": true, "c": null, "d": "n=3", "e": [3], "f": 7}));
    }
}
