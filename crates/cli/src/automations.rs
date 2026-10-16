//! `shimmer automations …` (ADR 0035 §8): rules that react to events by running commands. The
//! daemon checks and runs them; this only sends requests and shows the answers.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use serde_json::{json, Value};
use shimmer_core::{Error, ErrorCode, Result};

use crate::client::Client;
use crate::editor;
use crate::flags::Flags;
use crate::render::{self, table};
use crate::style::{paint, Role};
use crate::when;
use crate::workspaces::Prompt;

pub const USAGE: &str = "usage: shimmer automations <command>

commands:
  list                       your rules: on or off, what they react to, what they do
  show RULE                  one rule and its file
  new RULE [--file FILE]     a starter rule (switched off) opened in your editor, or one from FILE
  edit RULE [--file FILE]    open its file in your editor, or replace it with FILE; a broken
                             rule is refused and the old one kept
  remove RULE                remove a rule (restore brings it back)
  restore REMOVED            bring a removed rule back
  enable RULE                switch a rule on
  disable RULE               switch a rule off
  test RULE --event FILE     what it would do for a sample event, without doing it; FILE is
                             JSON: {\"topic\": \"records.item.completed\", \"payload\": {…}}
  events                     everything a rule can react to
  history [RULE] [--since DATE] [--limit N]
                             what your rules did, newest first
  status                     rules, the depth limit, stopped loops, broken files
  depth [N]                  show or set how many rules may follow each other (1-10, default 3)
  reload                     re-read the rule files after editing them by hand

A rule is a file in your data folder, data/automations/rules/RULE.toml:

  [rule]
  on = \"records.item.completed\"            # an event, or a pattern: records.item.*
  when = { collection = \"leetcode\" }       # conditions, as in records' filters
  [[do]]
  op = \"notify.send\"                       # any command's op; 'shimmer manifest' lists them
  params = { title = \"Solved: {title}\" }   # {field} is filled from the event
  after = \"60d\"                            # optional: wait first (30m, 2h, 3d, 1w)

Rules only run while the daemon runs. Add --json to any command for raw output.";

#[derive(Clone, Debug, PartialEq)]
pub enum AutomationsCmd {
    Help,
    List,
    Show { id: String },
    New { id: String, file: Option<PathBuf> },
    Edit { id: String, file: Option<PathBuf> },
    Remove { id: String },
    Restore { id: String },
    Enable { id: String },
    Disable { id: String },
    Test { id: String, event: PathBuf },
    Events,
    History { id: Option<String>, since: Option<DateTime<Utc>>, limit: Option<u64> },
    Status,
    Depth { depth: Option<u64> },
    Reload,
}

/// `words` are what follows `shimmer automations`.
pub fn parse(words: Vec<String>) -> std::result::Result<AutomationsCmd, String> {
    let mut words = words.into_iter();
    let Some(sub) = words.next() else { return Ok(AutomationsCmd::List) };
    let options: &[&str] = match sub.as_str() {
        "new" | "edit" => &["--file"],
        "test" => &["--event"],
        "history" => &["--since", "--limit"],
        _ => &[],
    };
    let flags = Flags::split(words.collect(), options, &[]).map_err(|e| format!("automations {sub}: {e}"))?;
    let usage = |what: &str| format!("usage: shimmer automations {sub} {what}");
    let id = |what: &str| match flags.positional.as_slice() {
        [id] => Ok(id.clone()),
        _ => Err(usage(what)),
    };
    let none = || match flags.positional.is_empty() {
        true => Ok(()),
        false => Err(format!("'automations {sub}' takes no arguments")),
    };
    let file = flags.value("--file").map(PathBuf::from);
    Ok(match sub.as_str() {
        "help" => AutomationsCmd::Help,
        "list" => none().map(|()| AutomationsCmd::List)?,
        "show" => AutomationsCmd::Show { id: id("RULE")? },
        "new" => AutomationsCmd::New { id: id("RULE [--file FILE]")?, file },
        "edit" => AutomationsCmd::Edit { id: id("RULE [--file FILE]")?, file },
        "remove" => AutomationsCmd::Remove { id: id("RULE")? },
        "restore" => AutomationsCmd::Restore { id: id("REMOVED")? },
        "enable" => AutomationsCmd::Enable { id: id("RULE")? },
        "disable" => AutomationsCmd::Disable { id: id("RULE")? },
        "test" => {
            let event = flags.value("--event").ok_or_else(|| usage("RULE --event FILE"))?;
            AutomationsCmd::Test { id: id("RULE --event FILE")?, event: PathBuf::from(event) }
        }
        "events" => none().map(|()| AutomationsCmd::Events)?,
        "history" => {
            let id = match flags.positional.as_slice() {
                [] => None,
                [id] => Some(id.clone()),
                _ => return Err(usage("[RULE] [--since DATE] [--limit N]")),
            };
            let since = flags.value("--since").map(since).transpose()?;
            let limit = flags.value("--limit").map(|n| count("--limit", n)).transpose()?;
            AutomationsCmd::History { id, since, limit }
        }
        "status" => none().map(|()| AutomationsCmd::Status)?,
        "depth" => match flags.positional.as_slice() {
            [] => AutomationsCmd::Depth { depth: None },
            [n] => AutomationsCmd::Depth { depth: Some(count("depth", n)?) },
            _ => return Err(usage("[N]")),
        },
        "reload" => none().map(|()| AutomationsCmd::Reload)?,
        other => return Err(format!("unknown automations command '{other}'; see 'shimmer automations --help'")),
    })
}

/// `--since`: a day (from its start, in your local time) or a time as `--once` takes it.
fn since(text: &str) -> std::result::Result<DateTime<Utc>, String> {
    if let Ok(day) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        let start = day.and_hms_opt(0, 0, 0).unwrap_or_default();
        if let Some(t) = Local.from_local_datetime(&start).earliest() {
            return Ok(t.with_timezone(&Utc));
        }
    }
    when::parse_once(text).map_err(|_| format!("--since '{text}' isn't a date: use 2026-10-10 or 2026-10-10 09:00"))
}

fn count(what: &str, n: &str) -> std::result::Result<u64, String> {
    n.parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(|| format!("{what} takes a whole number, e.g. 3"))
}

pub async fn run(client: &mut Client, cmd: &AutomationsCmd, json: bool, prompt: &mut impl Prompt) -> Result<String> {
    let one = |id: &str| json!({ "id": id });
    let raw = |data: Value| Ok(render::json(&data));
    match cmd {
        AutomationsCmd::Help => Ok(USAGE.into()),
        AutomationsCmd::List => {
            let data = client.call("automations.list", json!({})).await?;
            if json {
                return raw(data);
            }
            Ok(list(&data, Utc::now()))
        }
        AutomationsCmd::Show { id } => {
            let data = client.call("automations.get", one(id)).await?;
            if json {
                return raw(data);
            }
            Ok(show(&data, Utc::now()))
        }
        AutomationsCmd::New { id, file: Some(file) } => {
            let data = client.call("automations.create", json!({"id": id, "text": read(file)?})).await?;
            if json {
                return raw(data);
            }
            Ok(format!("✓ made rule {}", summary(&data)))
        }
        AutomationsCmd::New { id, file: None } => {
            let data = client.call("automations.create", one(id)).await?;
            if json || !prompt.interactive() {
                return if json {
                    raw(data)
                } else {
                    Ok(format!(
                        "✓ made a starter rule {id}, switched off\n  write it with: shimmer automations edit {id}"
                    ))
                };
            }
            let text = data_text(client, id).await?;
            edit_loop(client, id, text, prompt).await.map(|saved| match saved {
                true => format!("✓ made rule {id}; it's switched off until: shimmer automations enable {id}"),
                false => format!("✓ made a starter rule {id}, switched off and unchanged"),
            })
        }
        AutomationsCmd::Edit { id, file: Some(file) } => {
            let data = client.call("automations.save", json!({"id": id, "text": read(file)?})).await?;
            if json {
                return raw(data);
            }
            Ok(format!("✓ saved rule {}", summary(&data)))
        }
        AutomationsCmd::Edit { id, file: None } => {
            let text = data_text(client, id).await?;
            if !prompt.interactive() {
                return Err(Error::invalid_params(format!(
                    "automations edit needs a terminal; replace the rule from a file instead: \
                     shimmer automations edit {id} --file FILE"
                )));
            }
            edit_loop(client, id, text, prompt).await.map(|saved| match saved {
                true => format!("✓ saved rule {id}"),
                false => format!("no changes to {id}"),
            })
        }
        AutomationsCmd::Remove { id } => {
            let data = client.call("automations.remove", one(id)).await?;
            if json {
                return raw(data);
            }
            Ok(format!("✓ removed rule {id}\n  bring it back with: shimmer automations restore {id}"))
        }
        AutomationsCmd::Restore { id } => {
            let data = client.call("automations.restore", one(id)).await?;
            if json {
                return raw(data);
            }
            Ok(match data["problem"].as_str() {
                None => format!("✓ restored rule {}", summary(&data["rule"])),
                Some(problem) => format!(
                    "✓ restored {id}, but it can't run: {problem}\n  fix it with: shimmer automations edit {id}"
                ),
            })
        }
        AutomationsCmd::Enable { id } | AutomationsCmd::Disable { id } => {
            let on = matches!(cmd, AutomationsCmd::Enable { .. });
            let op = if on { "automations.enable" } else { "automations.disable" };
            let data = client.call(op, one(id)).await?;
            if json {
                return raw(data);
            }
            Ok(if on { format!("✓ {id} is on") } else { format!("✓ {id} is off: it won't react until enabled") })
        }
        AutomationsCmd::Test { id, event } => {
            let sample: Value = serde_json::from_str(&read(event)?)
                .map_err(|e| Error::invalid_params(format!("{} isn't JSON: {e}", event.display())))?;
            let data = client.call("automations.test", json!({"id": id, "event": sample})).await?;
            if json {
                return raw(data);
            }
            Ok(tested(&data))
        }
        AutomationsCmd::Events => {
            let data = client.call("automations.events", json!({})).await?;
            if json {
                return raw(data);
            }
            Ok(events(&data))
        }
        AutomationsCmd::History { id, since, limit } => {
            let mut params = json!({});
            if let Some(id) = id {
                params["id"] = json!(id);
            }
            if let Some(since) = since {
                params["since"] = json!(since);
            }
            if let Some(limit) = limit {
                params["limit"] = json!(limit);
            }
            let data = client.call("automations.history", params).await?;
            if json {
                return raw(data);
            }
            Ok(history(&data, Utc::now()))
        }
        AutomationsCmd::Status => {
            let data = client.call("automations.status", json!({})).await?;
            if json {
                return raw(data);
            }
            Ok(status(&data, Utc::now()))
        }
        AutomationsCmd::Depth { depth } => {
            let params = depth.map_or(json!({}), |d| json!({ "depth": d }));
            let data = client.call("automations.depth", params).await?;
            if json {
                return raw(data);
            }
            let n = render::cell(&data["depth"]);
            Ok(match depth {
                Some(_) => format!("✓ depth is {n}: up to {n} rules may follow each other"),
                None => {
                    format!("depth {n}: up to {n} rules may follow each other (change it: shimmer automations depth N)")
                }
            })
        }
        AutomationsCmd::Reload => {
            let data = client.call("automations.reload", json!({})).await?;
            if json {
                return raw(data);
            }
            let mut out = format!("✓ read {} rules", render::cell(&data["rules"]));
            out.push_str(&problems(&data["problems"]));
            Ok(out)
        }
    }
}

fn read(file: &Path) -> Result<String> {
    std::fs::read_to_string(file).map_err(|e| Error::invalid_params(format!("can't read {}: {e}", file.display())))
}

async fn data_text(client: &mut Client, id: &str) -> Result<String> {
    let data = client.call("automations.get", json!({ "id": id })).await?;
    Ok(data["text"].as_str().unwrap_or_default().to_owned())
}

/// Edit a copy of the rule in the editor and send it with `automations.save`; a refused rule can
/// be edited again with your text kept (ADR 0035 §8). True when something was saved.
async fn edit_loop(client: &mut Client, id: &str, text: String, prompt: &mut impl Prompt) -> Result<bool> {
    let path = std::env::temp_dir().join(format!("shimmer-rule-{id}-{}.toml", std::process::id()));
    std::fs::write(&path, &text).map_err(|e| Error::internal(format!("can't write {}: {e}", path.display())))?;
    loop {
        editor::open(&path)?;
        let edited = read(&path)?;
        if edited == text {
            let _ = std::fs::remove_file(&path);
            return Ok(false);
        }
        match client.call("automations.save", json!({"id": id, "text": edited})).await {
            Ok(_) => {
                let _ = std::fs::remove_file(&path);
                return Ok(true);
            }
            Err(e) if e.code == ErrorCode::InvalidParams => {
                eprintln!("{} {}", paint(Role::Error, "✗ not saved:"), e.message);
                if !prompt.confirm("Edit it again?") {
                    return Err(Error::invalid_params(format!(
                        "{id} not saved (the old rule is unchanged); your text is in {}",
                        path.display()
                    )));
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// `id` (label): what the rule reacts to.
fn summary(rule: &Value) -> String {
    format!("{} ({}): on {}", render::cell(&rule["id"]), render::cell(&rule["label"]), render::cell(&rule["on"]))
}

/// The ops a rule runs: `notify.send, workspaces.activate (after 3d)`.
fn does(rule: &Value) -> String {
    rule["actions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| match a["after_s"].as_u64() {
            Some(s) => format!("{} (after {})", render::cell(&a["op"]), when::format_duration(s)),
            None => render::cell(&a["op"]),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn ago(v: &Value, now: DateTime<Utc>, none: &str) -> String {
    when::timestamp(v).map_or(none.to_owned(), |at| when::relative(at, now))
}

fn list(data: &Value, now: DateTime<Utc>) -> String {
    let rules = data["rules"].as_array().cloned().unwrap_or_default();
    let mut out = if rules.is_empty() {
        "no rules yet; make one with: shimmer automations new RULE".to_owned()
    } else {
        let rows: Vec<Vec<String>> = rules
            .iter()
            .map(|r| {
                let mut note = Vec::new();
                if r["builtin"] == true {
                    note.push("built in".to_owned());
                }
                if r["loop_stopped_at"].is_string() {
                    note.push(format!("⚠ stopped a loop {}", ago(&r["loop_stopped_at"], now, "")));
                }
                vec![
                    render::cell(&r["id"]),
                    if r["enabled"] == true { "on".into() } else { "off".into() },
                    render::cell(&r["on"]),
                    does(r),
                    ago(&r["last_fired"], now, "never"),
                    note.join(", "),
                ]
            })
            .collect();
        let header = ["RULE", "ON", "REACTS TO", "DOES", "LAST FIRED", ""].map(String::from);
        table(&header, &rows)
    };
    out.push_str(&problems(&data["problems"]));
    let removed: Vec<String> = data["removed"].as_array().into_iter().flatten().map(render::cell).collect();
    if !removed.is_empty() {
        out.push_str(&format!(
            "\n\nremoved: {} (bring one back: shimmer automations restore RULE)",
            removed.join(", ")
        ));
    }
    out
}

/// Rule files that can't be used, under a list.
fn problems(v: &Value) -> String {
    let lines: Vec<String> = v
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| format!("  ✗ {}: {}", render::cell(&p["file"]), render::cell(&p["error"])))
        .collect();
    match lines.is_empty() {
        true => String::new(),
        false => format!(
            "\n\n{}\n{}\n  fix with 'shimmer automations edit RULE', or the file and 'shimmer automations reload'",
            paint(Role::Warning, "not running (can't be used):"),
            lines.join("\n")
        ),
    }
}

fn show(r: &Value, now: DateTime<Utc>) -> String {
    let mut out = format!(
        "{}  ({})\n",
        paint(Role::Name, &render::cell(&r["id"])),
        if r["enabled"] == true { "on" } else { "off" }
    );
    let mut line = |label: &str, value: String| out.push_str(&format!("  {}{value}\n", crate::style::label(label, 12)));
    line("label", render::cell(&r["label"]));
    line("reacts to", render::cell(&r["on"]));
    line("does", does(r));
    line("last fired", ago(&r["last_fired"], now, "never"));
    if r["loop_stopped_at"].is_string() {
        line(
            "⚠ stopped",
            format!(
                "a loop {} (see: shimmer automations history {})",
                ago(&r["loop_stopped_at"], now, ""),
                render::cell(&r["id"])
            ),
        );
    }
    format!("{out}\n{}", r["text"].as_str().unwrap_or_default().trim_end())
}

fn tested(data: &Value) -> String {
    let id = render::cell(&data["rule"]);
    if data["matches"] != true {
        return format!("{id} doesn't react to that event (check its 'on' and 'when')");
    }
    let mut out = format!("{id} reacts, and would:");
    for a in data["actions"].as_array().into_iter().flatten() {
        let params = serde_json::to_string(&a["params"]).unwrap_or_default();
        out.push_str(&match a["after_s"].as_u64() {
            Some(s) => format!("\n  schedule {} {params} in {}", render::cell(&a["op"]), when::format_duration(s)),
            None => format!("\n  queue {} {params}", render::cell(&a["op"])),
        });
    }
    if data["enabled"] != true {
        out.push_str(&format!("\n  (it's switched off now: shimmer automations enable {id})"));
    }
    out
}

fn events(data: &Value) -> String {
    let rows: Vec<Vec<String>> = data["events"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let d = render::cell(&e["description"]);
            vec![render::cell(&e["topic"]), if d.is_empty() { "–".into() } else { d }]
        })
        .collect();
    let header = ["EVENT", "WHAT HAPPENED"].map(String::from);
    format!("{}\n\nUse one as a rule's 'on', or a pattern: records.item.*", table(&header, &rows))
}

fn history(data: &Value, now: DateTime<Utc>) -> String {
    let entries = data["history"].as_array().cloned().unwrap_or_default();
    if entries.is_empty() {
        return "nothing yet: your rules haven't reacted to anything".into();
    }
    entries.iter().map(|e| entry(e, now)).collect::<Vec<_>>().join("\n")
}

fn entry(e: &Value, now: DateTime<Utc>) -> String {
    let when = ago(&e["at"], now, "?");
    let rule = render::cell(&e["rule"]);
    let topic = render::cell(&e["event"]["topic"]);
    if e["kind"] == "stopped" {
        let why =
            if e["reason"] == "cycle" { "it was already in the chain (a loop)" } else { "the chain was too deep" };
        return format!("{when}  ⚠ {rule} stopped on {topic}: {why}");
    }
    let mut did = Vec::new();
    for q in e["queued"].as_array().into_iter().flatten() {
        did.push(format!("queued {}", render::cell(&q["op"])));
    }
    for s in e["scheduled"].as_array().into_iter().flatten() {
        let at = when::timestamp(&s["at"]).map_or("later".into(), |at| when::relative(at, now));
        did.push(format!("scheduled {} {at}", render::cell(&s["op"])));
    }
    for f in e["failed"].as_array().into_iter().flatten() {
        did.push(format!("✗ {} failed: {}", render::cell(&f["op"]), render::cell(&f["error"])));
    }
    format!("{when}  {rule} on {topic}: {}", did.join(", "))
}

fn status(data: &Value, now: DateTime<Utc>) -> String {
    let mut out = format!(
        "rules: {} ({} on)\ndepth: {} (shimmer automations depth N)",
        render::cell(&data["rules"]),
        render::cell(&data["enabled"]),
        render::cell(&data["depth"])
    );
    let stopped: Vec<String> =
        data["stopped"].as_array().into_iter().flatten().map(|e| format!("  {}", entry(e, now))).collect();
    if !stopped.is_empty() {
        out.push_str(&format!("\n\n{}\n{}", paint(Role::Warning, "stopped loops:"), stopped.join("\n")));
    }
    out.push_str(&problems(&data["problems"]));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(words: &str) -> std::result::Result<AutomationsCmd, String> {
        parse(words.split_whitespace().map(String::from).collect())
    }

    #[test]
    fn commands_parse() {
        assert_eq!(p(""), Ok(AutomationsCmd::List));
        assert_eq!(p("show solved"), Ok(AutomationsCmd::Show { id: "solved".into() }));
        assert_eq!(p("new x --file r.toml"), Ok(AutomationsCmd::New { id: "x".into(), file: Some("r.toml".into()) }));
        assert_eq!(p("edit x"), Ok(AutomationsCmd::Edit { id: "x".into(), file: None }));
        assert_eq!(p("test x --event e.json"), Ok(AutomationsCmd::Test { id: "x".into(), event: "e.json".into() }));
        assert_eq!(p("depth"), Ok(AutomationsCmd::Depth { depth: None }));
        assert_eq!(p("depth 5"), Ok(AutomationsCmd::Depth { depth: Some(5) }));
        assert_eq!(
            p("history solved --limit 3"),
            Ok(AutomationsCmd::History { id: Some("solved".into()), since: None, limit: Some(3) })
        );
        assert!(matches!(p("history --since 2026-10-01"), Ok(AutomationsCmd::History { since: Some(_), .. })));
        for (bad, want) in [
            ("show", "usage: shimmer automations show RULE"),
            ("test x", "--event FILE"),
            ("depth zero", "whole number"),
            ("history --since soon", "isn't a date"),
            ("list x", "takes no arguments"),
            ("frob", "unknown automations command"),
        ] {
            assert!(p(bad).unwrap_err().contains(want), "{bad}: {:?}", p(bad));
        }
    }

    #[test]
    fn list_shows_rules_problems_and_removed() {
        let now = Utc::now();
        let data = json!({
            "rules": [{"id": "solved", "label": "Solved", "enabled": true, "builtin": false, "on": "records.item.completed",
                       "actions": [{"op": "notify.send", "after_s": null}, {"op": "notify.send", "after_s": 5_184_000}],
                       "last_fired": null, "loop_stopped_at": (now - chrono::Duration::minutes(5)).to_rfc3339()}],
            "problems": [{"file": "rules/bad.toml", "error": "on: no event 'x'"}],
            "removed": ["old"]});
        let out = list(&data, now);
        let row = out.lines().nth(1).unwrap();
        for want in [
            "solved",
            "on",
            "records.item.completed",
            "notify.send, notify.send (after 8w 4d)",
            "never",
            "⚠ stopped a loop",
        ] {
            assert!(row.contains(want), "{want} in {row}");
        }
        assert!(out.contains("✗ rules/bad.toml: on: no event 'x'"), "{out}");
        assert!(out.contains("removed: old"), "{out}");
        assert!(list(&json!({"rules": []}), now).starts_with("no rules yet"));
    }

    #[test]
    fn test_and_history_read_naturally() {
        let t = json!({"rule": "solved", "enabled": false, "matches": true,
                       "actions": [{"op": "notify.send", "params": {"title": "Solved: Two Sum"}, "after_s": null},
                                   {"op": "notify.send", "params": {}, "after_s": 86_400}]});
        let out = tested(&t);
        assert!(out.contains("queue notify.send {\"title\":\"Solved: Two Sum\"}"), "{out}");
        assert!(out.contains("schedule notify.send {} in 1d") && out.contains("switched off now"), "{out}");
        assert!(tested(&json!({"rule": "x", "matches": false})).contains("doesn't react"));

        let now = Utc::now();
        let fired = json!({"kind": "fired", "rule": "solved", "at": now.to_rfc3339(), "event": {"topic": "records.item.completed"},
                           "queued": [{"op": "notify.send"}], "scheduled": [], "failed": [{"op": "x.y", "error": "down"}]});
        let line = entry(&fired, now);
        assert!(line.contains("solved on records.item.completed: queued notify.send, ✗ x.y failed: down"), "{line}");
        let stopped =
            json!({"kind": "stopped", "rule": "again", "reason": "cycle", "event": {"topic": "records.item.updated"}});
        assert!(entry(&stopped, now).contains("⚠ again stopped on records.item.updated: it was already in the chain"));
    }
}
