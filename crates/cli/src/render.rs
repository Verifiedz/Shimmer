//! Turning response data into text. Pure functions over JSON: nothing here talks to the
//! daemon, and nothing here computes anything the daemon did not send (§2).

use std::fmt::Write as _;

use serde_json::Value;
use shimmer_core::{Error, Execution};
use shimmer_proto::{ManifestData, PingData};

use crate::style::{paint, Role};

pub fn ping(data: &Value) -> String {
    match serde_json::from_value::<PingData>(data.clone()) {
        Ok(p) => format!("{} (daemon up {})", paint(Role::Success, "pong"), uptime(p.uptime_s)),
        Err(_) => json(data),
    }
}

pub fn manifest(data: &Value) -> String {
    let Ok(m) = serde_json::from_value::<ManifestData>(data.clone()) else { return json(data) };
    let mut out = format!("protocol v{}\n\n{}\n", m.protocol, paint(Role::Heading, "lanes"));
    let width = m.lanes.iter().map(|l| l.id.as_str().len()).max().unwrap_or(0);
    for l in &m.lanes {
        let id = l.id.as_str();
        let pad = " ".repeat(width - id.len());
        let _ = writeln!(out, "  {}{pad}  max {}", paint(Role::Name, id), l.max_concurrent);
    }
    let _ = writeln!(out, "\n{}", paint(Role::Heading, "modules"));
    if m.modules.is_empty() {
        out.push_str("  (none registered)\n");
    }
    for module in &m.modules {
        let _ = writeln!(out, "  {} {}", paint(Role::Heading, &module.id.to_string()), module.version);
        let rows: Vec<(String, &str)> =
            module.commands.iter().map(|c| (execution(&c.execution), c.summary.as_str())).collect();
        let op_w = module.commands.iter().map(|c| c.op.len()).max().unwrap_or(0);
        let ex_w = rows.iter().map(|(e, _)| e.len()).max().unwrap_or(0);
        for (c, (ex, summary)) in module.commands.iter().zip(&rows) {
            let pad = " ".repeat(op_w - c.op.len());
            let _ = writeln!(out, "    {}{pad}  {ex:ex_w$}  {summary}", paint(Role::Name, &c.op));
        }
    }
    out.truncate(out.trim_end().len());
    out
}

pub fn shutdown() -> String {
    "daemon stopped".into()
}

/// Anything without a dedicated renderer, and everything under `--json`.
pub fn json(data: &Value) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string())
}

/// `code: message`, plus the structured detail when there is one. Clients branch on the
/// code; the message is for people.
pub fn error(e: &Error) -> String {
    let mut out = format!("{}: {}", e.code, e.message);
    if let Some(detail) = e.detail.as_ref().filter(|d| !d.is_null()) {
        let _ = write!(out, "\n{}", json(detail));
    }
    out
}

fn execution(e: &Execution) -> String {
    match e {
        Execution::Inline => "inline".into(),
        Execution::Queued { lane } => format!("queued:{lane}"),
    }
}

pub(crate) fn uptime(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m {s}s"),
        _ => format!("{h}h {m}m {s}s"),
    }
}

// ---------------------------------------------------------------- tables, shared by records and workspaces

pub(crate) const MAX_CELL: usize = 40;

/// One value as table text: strings bare, `null` as `-`, long values cut with `…`.
pub(crate) fn cell(v: &Value) -> String {
    let s = match v {
        Value::Null => "-".to_owned(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if s.chars().count() > MAX_CELL {
        s.chars().take(MAX_CELL - 1).chain(['…']).collect()
    } else {
        s
    }
}

pub(crate) fn table(header: &[String], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (w, c) in widths.iter_mut().zip(row) {
            *w = (*w).max(c.chars().count());
        }
    }
    // Headers in the heading colour, the first named column (names and ids) in the name colour,
    // and a STATUS or STATE column by what each state means (#189). Widths are measured on the
    // plain text; colour goes around it.
    let name_column = header.iter().position(|h| !h.is_empty()).unwrap_or(0);
    let line = |cells: &[String], is_header: bool| {
        let mut s = String::new();
        for (i, (c, w)) in cells.iter().zip(&widths).enumerate() {
            let shown = match (is_header, i) {
                (true, _) => paint(Role::Heading, c),
                (false, i) if i == name_column => paint(Role::Name, c),
                (false, i) if matches!(header.get(i).map(String::as_str), Some("STATUS" | "STATE")) => {
                    crate::style::state(c)
                }
                _ => c.clone(),
            };
            if i + 1 == cells.len() {
                s.push_str(&shown);
            } else {
                let pad = w - c.chars().count();
                let _ = write!(s, "{shown}{}  ", " ".repeat(pad));
            }
        }
        s
    };
    let mut out = line(header, true);
    for row in rows {
        out.push('\n');
        out.push_str(&line(row, false));
    }
    out
}

/// The longest description shown in the list; `peek` shows it whole.
const LISTED_DESCRIPTION: usize = 72;

/// A description cut at [`LISTED_DESCRIPTION`] characters, at a word, for a list of templates.
fn shorten(text: &str) -> String {
    if text.chars().count() <= LISTED_DESCRIPTION {
        return text.to_owned();
    }
    let cut: String = text.chars().take(LISTED_DESCRIPTION).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{}…", cut.trim_end_matches([',', ':', ';', ' ']))
}

/// Words wrapped to `width`, each line after the first indented by `indent`.
pub fn wrap(text: &str, width: usize, indent: &str) -> String {
    let mut lines: Vec<String> = vec![String::new()];
    for word in text.split_whitespace() {
        let line = lines.last_mut().expect("never empty");
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(word.to_owned());
        } else {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
    }
    lines.join(&format!("\n{indent}"))
}

/// Ids as a list shows them (ADR 0031 §4): a long id (a ULID, or `usr-` plus one) as its last six
/// characters, or more where that's needed to tell it from another id in the same list, which is
/// what the daemon accepts back. Short ids (`digest`, a test's `usr-1`) are shown whole.
pub fn short_ids(ids: &[String]) -> Vec<String> {
    let lower: Vec<String> = ids.iter().map(|id| id.to_lowercase()).collect();
    ids.iter()
        .zip(&lower)
        .map(|(id, me)| {
            // Only a random id shortens well: a module's own trigger id (`fetchers.poll-hackernews`)
            // is shown whole, since its end means nothing on its own.
            if !is_random_id(id) {
                return id.clone();
            }
            let mut n = 6;
            while n < me.len() && lower.iter().any(|other| other != me && other.ends_with(&me[me.len() - n..])) {
                n += 1;
            }
            id[id.len() - n..].to_owned()
        })
        .collect()
}

/// A ULID, or `usr-` plus one: the ids whose end is random, so a short ending tells them apart.
fn is_random_id(id: &str) -> bool {
    let ulid = id.strip_prefix("usr-").unwrap_or(id);
    ulid.len() == 26 && ulid.chars().all(|c| c.is_ascii_alphanumeric())
}

/// The ending of one id that lists show (see [`short_ids`]), for saying which one a command hit.
pub fn short_id(id: &str) -> String {
    short_ids(&[id.to_owned()]).remove(0)
}

/// The part of a `*.templates` reply that `templates [CATEGORY | TEMPLATE]` shows, for `--json`:
/// the same shape, `{"templates": [...], "categories": [...]}`, with only that category's
/// templates, or only that template (#179). `which` must already be known to be one of them
/// (`templates_by_category` checks it); anything else gives everything.
pub fn templates_selected(data: &Value, which: Option<&str>) -> Value {
    let Some(which) = which else { return data.clone() };
    let all = data["templates"].as_array().map(Vec::as_slice).unwrap_or_default();
    let chosen: Vec<&Value> = match all.iter().find(|t| t["id"] == which) {
        Some(t) => vec![t],
        None => all.iter().filter(|t| t["category"] == which).collect(),
    };
    let mut out = data.clone();
    out["templates"] = serde_json::json!(chosen);
    out
}

/// The templates a `*.templates` op sent (`records`, `workspaces`), grouped under their
/// categories' headings in the daemon's order. With `which`: only that category's, or that one
/// template as `in_full` shows it. `kind` names the commands in the footer and in mistakes;
/// `make` is the footer's line for making one, e.g. ("make a workspace from it:", "shimmer
/// workspaces new NAME --from TEMPLATE").
pub fn templates_by_category(
    data: &Value,
    which: Option<&str>,
    kind: &str,
    make: (&str, &str),
    in_full: fn(&Value, &[Value]) -> String,
) -> Result<String, Error> {
    let all = data["templates"].as_array().map(Vec::as_slice).unwrap_or_default();
    let categories = data["categories"].as_array().map(Vec::as_slice).unwrap_or_default();
    if let Some(which) = which {
        if all.iter().any(|t| t["id"] == which) {
            return template_named(data, which, kind).map(|t| in_full(t, categories));
        }
        if !categories.iter().any(|c| c["id"] == which) {
            let names: Vec<String> = categories.iter().map(|c| cell(&c["id"])).collect();
            return Err(Error::not_found(format!(
                "no template or category '{which}' (categories: {}; templates: shimmer {kind} templates)",
                names.join(", ")
            )));
        }
    }
    if all.is_empty() {
        return Ok("no templates".into());
    }
    let id_width = all.iter().map(|t| cell(&t["id"]).chars().count()).max().unwrap_or(0);
    let mut groups: Vec<(String, Vec<&Value>)> = Vec::new();
    for t in all.iter().filter(|t| which.is_none_or(|w| t["category"] == w)) {
        // "Job hunt (job-hunt)": the short name is what `templates CATEGORY` takes.
        let heading = categories
            .iter()
            .find(|c| c["id"] == t["category"])
            .map_or_else(|| "Other".to_owned(), |c| format!("{} ({})", cell(&c["label"]), cell(&c["id"])));
        match groups.iter_mut().find(|(h, _)| *h == heading) {
            Some((_, ts)) => ts.push(t),
            None => groups.push((heading, vec![t])),
        }
    }
    let mut out = String::new();
    for (heading, ts) in &groups {
        let _ = writeln!(out, "{}", paint(Role::Heading, heading));
        for t in ts {
            let id = t["id"].as_str().unwrap_or_default();
            let pad = " ".repeat(id_width.saturating_sub(id.chars().count()));
            let name = paint(Role::Name, id);
            let _ = writeln!(out, "  {name}{pad}  {}", shorten(t["description"].as_str().unwrap_or_default()));
        }
        out.push('\n');
    }
    let peek = ("everything about one:", format!("shimmer {kind} peek TEMPLATE"));
    let width = peek.0.len().max(make.0.len());
    let _ = write!(out, "{:<width$} {}\n{:<width$} {}", peek.0, peek.1, make.0, make.1);
    Ok(out)
}

/// Template `id` from a `*.templates` reply. A category's name gets a pointer to
/// `templates CATEGORY`; anything else is `not_found` naming the templates there are.
pub fn template_named<'a>(data: &'a Value, id: &str, kind: &str) -> Result<&'a Value, Error> {
    let all = data["templates"].as_array().map(Vec::as_slice).unwrap_or_default();
    let categories = data["categories"].as_array().map(Vec::as_slice).unwrap_or_default();
    match all.iter().find(|t| t["id"] == id) {
        Some(t) => Ok(t),
        None if categories.iter().any(|c| c["id"] == id) => Err(Error::not_found(format!(
            "'{id}' is a category, not a template: see its templates with shimmer {kind} templates {id}"
        ))),
        None => {
            let names: Vec<String> = all.iter().map(|t| cell(&t["id"])).collect();
            Err(Error::not_found(format!("no template '{id}' (there are: {})", names.join(", "))))
        }
    }
}

/// The categories' headings a `*.templates` reply carries.
pub fn categories(data: &Value) -> &[Value] {
    data["categories"].as_array().map(Vec::as_slice).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn long_ids_show_their_unique_ending() {
        let ids: Vec<String> =
            ["01M4PPHE00ABCDEF0123XK7P2X", "01M4PPK8K0QRSTUV4567HK7P2X", "usr-01m4ppm000nntaem1rv43fq9tm", "digest"]
                .map(String::from)
                .to_vec();
        // "k7p2x" ends both of the first two, so they need seven characters; the others don't.
        assert_eq!(short_ids(&ids), ["XK7P2X", "HK7P2X", "3fq9tm", "digest"]);
        // A module's own trigger id isn't random: shown whole, however long (#183's review).
        let named = ["fetchers.poll-hackernews".to_owned()];
        assert_eq!(short_ids(&named), ["fetchers.poll-hackernews"]);
        assert_eq!(short_id("usr-01m4ppm000nntaem1rv43fq9tm"), "3fq9tm");
        let close: Vec<String> =
            ["01M4PPHE00ABCDEF0123AK7P2X", "01M4PPK8K0QRSTUV4567BK7P2X"].map(String::from).to_vec();
        assert_eq!(short_ids(&close), ["AK7P2X", "BK7P2X"]);
        let same6: Vec<String> =
            ["01M4PPHE00ABCDEF012AXK7P2X", "01M4PPK8K0QRSTUV456BXK7P2X"].map(String::from).to_vec();
        assert_eq!(short_ids(&same6), ["AXK7P2X", "BXK7P2X"], "lengthened until unique");
    }

    #[test]
    fn json_for_templates_shows_what_the_text_shows() {
        // #179: `templates CATEGORY --json` returned every template.
        let data = serde_json::json!({
            "templates": [{"id": "leetcode", "category": "practice"}, {"id": "jobs", "category": "job-hunt"},
                          {"id": "offers", "category": "job-hunt"}],
            "categories": [{"id": "job-hunt"}, {"id": "practice"}]
        });
        let ids = |v: Value| v["templates"].as_array().unwrap().iter().map(|t| cell(&t["id"])).collect::<Vec<_>>();
        assert_eq!(ids(templates_selected(&data, Some("job-hunt"))), ["jobs", "offers"]);
        assert_eq!(ids(templates_selected(&data, Some("leetcode"))), ["leetcode"]);
        assert_eq!(templates_selected(&data, None), data);
        assert_eq!(templates_selected(&data, Some("practice"))["categories"], data["categories"], "headings kept");
    }

    #[test]
    fn ping_shows_uptime() {
        assert_eq!(ping(&json!({"pong": true, "uptime_s": 4})), "pong (daemon up 4s)");
        assert_eq!(ping(&json!({"pong": true, "uptime_s": 3725})), "pong (daemon up 1h 2m 5s)");
        // An unexpected shape is shown, not hidden.
        assert_eq!(ping(&json!({"surprise": 1})), "{\n  \"surprise\": 1\n}");
    }

    #[test]
    fn manifest_lists_lanes_modules_and_ops() {
        let data = json!({
            "protocol": 1,
            "lanes": [{"id": "default", "max_concurrent": 4}, {"id": "index", "max_concurrent": 1}],
            "modules": [{
                "id": "records", "version": "0.1.0", "namespace": "records", "topics": [],
                "commands": [
                    {"op": "records.list", "summary": "List records", "params_schema": {}, "execution": "inline"},
                    {"op": "records.reindex", "summary": "Rebuild the index", "params_schema": {},
                     "execution": {"queued": {"lane": "index"}}}
                ]
            }]
        });
        let want = "\
protocol v1

lanes
  default  max 4
  index    max 1

modules
  records 0.1.0
    records.list     inline        List records
    records.reindex  queued:index  Rebuild the index";
        assert_eq!(manifest(&data), want);
    }

    #[test]
    fn empty_manifest_says_so() {
        let data = json!({"protocol": 1, "lanes": [{"id": "default", "max_concurrent": 4}], "modules": []});
        assert!(manifest(&data).ends_with("modules\n  (none registered)"));
    }

    #[test]
    fn errors_show_code_message_and_detail() {
        assert_eq!(error(&Error::not_found("no collection 'x'")), "not_found: no collection 'x'");
        let e = Error::conflict("stale").with_detail(json!({"queue_version": 41}));
        assert_eq!(error(&e), "conflict: stale\n{\n  \"queue_version\": 41\n}");
    }

    #[test]
    fn long_descriptions_are_cut_at_a_word() {
        assert_eq!(shorten("short"), "short");
        assert_eq!(
            shorten("Free space safely: build folders of projects you haven't touched in a while, and tool caches"),
            "Free space safely: build folders of projects you haven't touched in a…"
        );
    }
}
