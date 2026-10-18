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
  list [--removed]           your rules: on or off, what they react to, what they do;
                             --removed: the ones you removed, to restore or purge
  show RULE                  one rule and its file
  new RULE [--file FILE]     a starter rule (switched off) opened in your editor, or one from FILE
  new RULE --from TEMPLATE [--set QUESTION=ANSWER]… [--yes]
                             rules from a template: asks its questions (or takes --set), shows
                             the rules, and makes them once you say yes (--yes: without asking)
  edit RULE [--file FILE]    open its file in your editor, or replace it with FILE; a broken
                             rule is refused and the old one kept
  remove RULE                remove a rule (restore brings it back)
  restore REMOVED            bring a removed rule back
  rename RULE NEW            give a rule a new id (not while a delayed action from it waits)
  copy RULE NEW [--label TEXT]
                             a new rule like RULE, switched off until you enable it
  purge [REMOVED] [--yes]    delete a removed rule for good, or every removed one (asks first)
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
  templates [CATEGORY]       ready-made rules, by category
  peek TEMPLATE              everything about one template: its questions and the rules it makes

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
    /// `removed`: the removed rules instead (ADR 0038 §6).
    List {
        removed: bool,
    },
    Rename {
        id: String,
        to: String,
    },
    /// A new rule from another, switched off; `label` its own.
    Copy {
        id: String,
        to: String,
        label: Option<String>,
    },
    Show {
        id: String,
    },
    New {
        id: String,
        file: Option<PathBuf>,
    },
    /// `new RULE --from TEMPLATE [--set Q=A]… [--yes]`.
    FromTemplate {
        id: String,
        template: String,
        set: Vec<(String, String)>,
        yes: bool,
    },
    Templates {
        which: Option<String>,
    },
    Peek {
        template: String,
    },
    Edit {
        id: String,
        file: Option<PathBuf>,
    },
    Remove {
        id: String,
    },
    Restore {
        id: String,
    },
    /// `id`: one removed rule; `None` is every one (ADR 0038).
    Purge {
        id: Option<String>,
        yes: bool,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Test {
        id: String,
        event: PathBuf,
    },
    Events,
    History {
        id: Option<String>,
        since: Option<DateTime<Utc>>,
        limit: Option<u64>,
    },
    Status,
    Depth {
        depth: Option<u64>,
    },
    Reload,
}

/// `words` are what follows `shimmer automations`.
pub fn parse(words: Vec<String>) -> std::result::Result<AutomationsCmd, String> {
    if words.first().is_some_and(|w| w == "new") && words.iter().any(|w| w == "--from" || w.starts_with("--from=")) {
        return from_template(words.into_iter().skip(1));
    }
    let mut words = words.into_iter();
    let Some(sub) = words.next() else { return Ok(AutomationsCmd::List { removed: false }) };
    let options: &[&str] = match sub.as_str() {
        "new" | "edit" => &["--file"],
        "copy" => &["--label"],
        "test" => &["--event"],
        "history" => &["--since", "--limit"],
        _ => &[],
    };
    let switches: &[&str] = match sub.as_str() {
        "purge" => &["--yes"],
        "list" => &["--removed"],
        _ => &[],
    };
    let flags = Flags::split(words.collect(), options, switches).map_err(|e| format!("automations {sub}: {e}"))?;
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
        "list" => none().map(|()| AutomationsCmd::List { removed: flags.has("--removed") })?,
        "rename" | "copy" => {
            let [id, to]: [String; 2] = flags.positional.clone().try_into().map_err(|_| match sub.as_str() {
                "rename" => usage("RULE NEW"),
                _ => usage("RULE NEW [--label TEXT]"),
            })?;
            match sub.as_str() {
                "rename" => AutomationsCmd::Rename { id, to },
                _ => AutomationsCmd::Copy { id, to, label: flags.value("--label").map(str::to_owned) },
            }
        }
        "show" => AutomationsCmd::Show { id: id("RULE")? },
        "new" => AutomationsCmd::New { id: id("RULE [--file FILE]")?, file },
        "edit" => AutomationsCmd::Edit { id: id("RULE [--file FILE]")?, file },
        "remove" => AutomationsCmd::Remove { id: id("RULE")? },
        "restore" => AutomationsCmd::Restore { id: id("REMOVED")? },
        "purge" => match flags.positional.as_slice() {
            [] => AutomationsCmd::Purge { id: None, yes: flags.has("--yes") },
            [id] => AutomationsCmd::Purge { id: Some(id.clone()), yes: flags.has("--yes") },
            _ => return Err(usage("[REMOVED] [--yes]")),
        },
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
        "templates" => match flags.positional.as_slice() {
            [] => AutomationsCmd::Templates { which: None },
            [which] => AutomationsCmd::Templates { which: Some(which.clone()) },
            _ => return Err(usage("[CATEGORY]")),
        },
        "peek" => AutomationsCmd::Peek { template: id("TEMPLATE")? },
        other => return Err(format!("unknown automations command '{other}'; see 'shimmer automations --help'")),
    })
}

/// `new RULE --from TEMPLATE [--set QUESTION=ANSWER]… [--yes]`.
fn from_template(words: impl Iterator<Item = String>) -> std::result::Result<AutomationsCmd, String> {
    let usage = "usage: shimmer automations new RULE --from TEMPLATE [--set QUESTION=ANSWER]… [--yes]";
    let (mut id, mut template, mut set, mut yes) = (None, None, Vec::new(), false);
    let mut words = words.into_iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--from" => template = Some(words.next().ok_or("--from needs a TEMPLATE")?),
            "--set" => set.push(crate::workspaces::set_answer(&words.next().ok_or("--set needs QUESTION=ANSWER")?)?),
            "--yes" | "-y" => yes = true,
            w if w.starts_with("--from=") => template = w.strip_prefix("--from=").map(String::from),
            w if w.starts_with("--set=") => set.push(crate::workspaces::set_answer(&w["--set=".len()..])?),
            w if w.starts_with('-') => return Err(format!("automations new: unknown option '{w}'; {usage}")),
            _ if id.is_none() => id = Some(word),
            _ => return Err(usage.into()),
        }
    }
    match (id, template) {
        (Some(id), Some(template)) => Ok(AutomationsCmd::FromTemplate { id, template, set, yes }),
        _ => Err(usage.into()),
    }
}

/// `--since`: a day (from its start, in your local time) or a time as `--once` takes it.
fn since(text: &str) -> std::result::Result<DateTime<Utc>, String> {
    if let Ok(day) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        let start = day.and_hms_opt(0, 0, 0).unwrap_or_default();
        // A midnight the clocks repeat counts from the first one: "since" that day includes all
        // of it. One they skip falls through to the full form below.
        if let Some(t) = Local.from_local_datetime(&start).earliest() {
            return Ok(t.with_timezone(&Utc));
        }
    }
    when::parse_once(text).map_err(|_| format!("--since '{text}' isn't a date: use 2026-10-10 or 2026-10-10 09:00"))
}

fn count(what: &str, n: &str) -> std::result::Result<u64, String> {
    n.parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(|| format!("{what} takes a number from 1 up, e.g. 3"))
}

pub async fn run(client: &mut Client, cmd: &AutomationsCmd, json: bool, prompt: &mut impl Prompt) -> Result<String> {
    let one = |id: &str| json!({ "id": id });
    let raw = |data: Value| Ok(render::json(&data));
    match cmd {
        AutomationsCmd::Help => Ok(USAGE.into()),
        AutomationsCmd::List { removed: false } => {
            let data = client.call("automations.list", json!({})).await?;
            if json {
                return raw(data);
            }
            Ok(list(&data, Utc::now()))
        }
        AutomationsCmd::List { removed: true } => {
            let data = client.call("automations.list", json!({"removed": true})).await?;
            if json {
                return raw(data);
            }
            Ok(removed_list(&data))
        }
        AutomationsCmd::Rename { id, to } => {
            let data = client.call("automations.rename", json!({"id": id, "to": to})).await?;
            if json {
                return raw(data);
            }
            Ok(format!("✓ {id} is now {to}"))
        }
        AutomationsCmd::Copy { id, to, label } => {
            let mut params = json!({"id": id, "to": to});
            if let Some(label) = label {
                params["label"] = json!(label);
            }
            let data = client.call("automations.copy", params).await?;
            if json {
                return raw(data);
            }
            Ok(format!("✓ made {to} from {id}, switched off so the same event doesn't fire both\n  turn it on when it's ready: shimmer automations enable {to}"))
        }
        AutomationsCmd::Show { id } => {
            let data = client.call("automations.show", one(id)).await?;
            if json {
                return raw(data);
            }
            Ok(show(&data, Utc::now()))
        }
        AutomationsCmd::New { id, file: Some(file) } => {
            let data = client.call("automations.new", json!({"id": id, "text": read(file)?})).await?;
            if json {
                return raw(data);
            }
            Ok(format!("✓ made rule {}", summary(&data)))
        }
        AutomationsCmd::New { id, file: None } => {
            let data = client.call("automations.new", one(id)).await?;
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
            let data = client.call("automations.edit", json!({"id": id, "text": read(file)?})).await?;
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
        AutomationsCmd::Purge { id, yes } => {
            // ADR 0038: the daemon says how many go; ask, then send that count back.
            let mut params = json!({});
            if let Some(id) = id {
                params["id"] = json!(id);
            }
            let data = match client.call("automations.purge", params.clone()).await {
                Err(e) if e.code == shimmer_core::ErrorCode::ConfirmationRequired => {
                    let what = match id {
                        Some(id) => format!("the removed rule '{id}'"),
                        None => "every removed rule".to_owned(),
                    };
                    if !crate::records::confirm_purge(prompt, *yes, &what, &e.message)? {
                        return Ok("nothing purged".into());
                    }
                    params["confirm"] = e.detail.unwrap_or_default();
                    client.call("automations.purge", params).await?
                }
                other => other?,
            };
            if json {
                return raw(data);
            }
            Ok(match (data["purged"].as_u64().unwrap_or(0), id) {
                (0, _) => "no removed rules to purge".to_owned(),
                (_, Some(id)) => format!("✓ {id} is deleted for good"),
                (n, None) => format!("✓ {n} removed rule(s) deleted for good"),
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
            let n = full(&data["depth"]);
            Ok(match depth {
                Some(_) => format!("✓ depth is {n}: up to {n} rules may follow each other"),
                None => {
                    format!("depth {n}: up to {n} rules may follow each other (change it: shimmer automations depth N)")
                }
            })
        }
        AutomationsCmd::Templates { which } => {
            let data = client.call("automations.templates", json!({})).await?;
            let text = render::templates_by_category(
                &data,
                which.as_deref(),
                "automations",
                ("make rules from it:", "shimmer automations new RULE --from TEMPLATE"),
                in_full,
            )?;
            Ok(if json { render::json(&render::templates_selected(&data, which.as_deref())) } else { text })
        }
        AutomationsCmd::Peek { template } => {
            let data = client.call("automations.templates", json!({})).await?;
            let t = render::template_named(&data, template, "automations")?;
            Ok(if json { render::json(t) } else { in_full(t, render::categories(&data)) })
        }
        AutomationsCmd::FromTemplate { id, template, set, yes } => {
            from_template_run(client, id, template, set, *yes, json, prompt).await
        }
        AutomationsCmd::Reload => {
            let data = client.call("automations.reload", json!({})).await?;
            if json {
                return raw(data);
            }
            let mut out = format!("✓ read {} rules", full(&data["rules"]));
            out.push_str(&problems(&data["problems"]));
            Ok(out)
        }
    }
}

/// Ask the template's questions, show the rules they make, and make them once agreed (ADR 0035
/// §9, ADR 0025 §6a): nothing is made before the person has seen it, unless they said --yes.
async fn from_template_run(
    client: &mut Client,
    id: &str,
    template: &str,
    set: &[(String, String)],
    yes: bool,
    json: bool,
    prompt: &mut impl Prompt,
) -> Result<String> {
    let data = client.call("automations.templates", json!({})).await?;
    let t = render::template_named(&data, template, "automations")?.clone();
    if prompt.interactive() && set.is_empty() {
        eprintln!("{} — {}", paint(Role::Heading, &full(&t["label"])), full(&t["description"]));
    }
    let values = crate::workspaces::answers(&t["questions"], set, prompt, &crate::workspaces::Paths::from_env())?;
    let mut params = json!({"id": id, "template": template, "values": values, "preview": true});
    let shown = client
        .call("automations.new", params.clone())
        .await
        .map_err(|e| crate::workspaces::how_to_answer(e, prompt))?;
    let rules = shown["rules"].as_array().cloned().unwrap_or_default();
    // Nobody to ask (a script) and no --yes: refuse, never make rules unseen (ADR 0025 §6a,
    // #254's review).
    if !yes && !prompt.interactive() {
        return Err(Error::invalid_params(format!(
            "refusing to make {} rule{} without confirmation; pass --yes",
            rules.len(),
            if rules.len() == 1 { "" } else { "s" }
        )));
    }
    if !yes {
        for r in &rules {
            eprintln!(
                "
── {} ──
{}",
                full(&r["id"]),
                r["text"].as_str().unwrap_or_default().trim_end()
            );
        }
        let question = match rules.len() {
            1 => "
Make this rule? It's on as soon as it's made."
                .to_owned(),
            n => format!(
                "
Make these {n} rules? They're on as soon as they're made."
            ),
        };
        if !prompt.confirm(&question) {
            return Ok("nothing made".into());
        }
    }
    params["preview"] = json!(false);
    let made = client.call("automations.new", params).await?;
    if json {
        return Ok(render::json(&made));
    }
    let lines: Vec<String> =
        made["rules"].as_array().into_iter().flatten().map(|r| format!("  {}", summary(r))).collect();
    Ok(format!(
        "✓ made from {template}, and on:
{}
  switch one off: shimmer automations disable RULE",
        lines.join("\n")
    ))
}

/// One template as `peek` shows it: what it does, what to know, its questions, its rules.
fn in_full(t: &Value, categories: &[Value]) -> String {
    let category = categories.iter().find(|c| c["id"] == t["category"]);
    let mut out = format!(
        "{}  {}
{}
",
        paint(Role::Name, &full(&t["id"])),
        full(&t["label"]),
        full(&t["description"])
    );
    if let Some(c) = category {
        out.push_str(&format!(
            "category: {} ({})
",
            full(&c["label"]),
            full(&c["id"])
        ));
    }
    let ops: Vec<String> = t["ops"].as_array().into_iter().flatten().map(render::cell).collect();
    out.push_str(&format!(
        "runs: {}
",
        ops.join(", ")
    ));
    out.push_str(&format!(
        "
{}
",
        paint(Role::Heading, "Good to know")
    ));
    for g in t["good_to_know"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  · {}
",
            full(g)
        ));
    }
    out.push_str(&format!(
        "
{}
",
        paint(Role::Heading, "Questions")
    ));
    for q in t["questions"].as_array().into_iter().flatten() {
        let mut how = full(&q["kind"]);
        if q["required"] == true {
            how.push_str(", required");
        }
        if let Some(d) = q["default"].as_str() {
            how.push_str(&format!(", default {d}"));
        }
        if let Some(c) = q["choices"].as_array() {
            how.push_str(&format!(": {}", c.iter().map(render::cell).collect::<Vec<_>>().join(", ")));
        }
        out.push_str(&format!(
            "  {}  {} ({how})
",
            full(&q["name"]),
            full(&q["prompt"])
        ));
    }
    let rules = t["rules"].as_array().map(Vec::as_slice).unwrap_or_default();
    out.push_str(&format!(
        "
{}
",
        paint(Role::Heading, if rules.len() == 1 { "The rule it makes" } else { "The rules it makes" })
    ));
    for r in rules {
        let suffix = r["suffix"].as_str().unwrap_or_default();
        let name = if suffix.is_empty() { "RULE".to_owned() } else { format!("RULE-{suffix}") };
        out.push_str(&format!(
            "
── {name} ──
{}
",
            r["text"].as_str().unwrap_or_default().trim_end()
        ));
    }
    out.push_str(&format!(
        "
${{NAME}} is your answer; {{field}} is filled from the event.
make it: shimmer automations new RULE --from {}",
        full(&t["id"])
    ));
    out
}

/// A value in full, for sentences; `render::cell` shortens, for table cells.
fn full(v: &Value) -> String {
    match v {
        Value::Null => "-".to_owned(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn read(file: &Path) -> Result<String> {
    std::fs::read_to_string(file).map_err(|e| Error::invalid_params(format!("can't read {}: {e}", file.display())))
}

async fn data_text(client: &mut Client, id: &str) -> Result<String> {
    let data = client.call("automations.show", json!({ "id": id })).await?;
    Ok(data["text"].as_str().unwrap_or_default().to_owned())
}

/// Edit a copy of the rule in the editor and send it with `automations.edit`; a refused rule can
/// be edited again with your text kept (ADR 0035 §8). True when something was saved.
async fn edit_loop(client: &mut Client, id: &str, text: String, prompt: &mut impl Prompt) -> Result<bool> {
    let path = new_private_file(id, &text)?;
    loop {
        editor::open(&path)?;
        let edited = read(&path)?;
        if edited == text {
            let _ = std::fs::remove_file(&path);
            return Ok(false);
        }
        match client.call("automations.edit", json!({"id": id, "text": edited})).await {
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
            // Whatever went wrong (the rule removed meanwhile, the daemon restarted), the edit isn't
            // lost: say where it is (#253's review).
            Err(e) => {
                return Err(Error::new(
                    e.code,
                    format!("{}; {id} not saved, your text is in {}", e.message, path.display()),
                ))
            }
        }
    }
}

/// A new file in the temp folder holding `text`, readable only by you, at a name nobody else can
/// have made first: it's created with `create_new`, so a file or link someone planted there is
/// refused rather than written through (#253's review).
fn new_private_file(id: &str, text: &str) -> Result<std::path::PathBuf> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let path = std::env::temp_dir().join(format!("shimmer-rule-{id}-{}-{stamp}.toml", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| Error::internal(format!("can't create {}: {e}", path.display())))?;
    f.write_all(text.as_bytes()).map_err(|e| Error::internal(format!("can't write {}: {e}", path.display())))?;
    Ok(path)
}

/// `id` (label): what the rule reacts to.
fn summary(rule: &Value) -> String {
    format!("{} ({}): on {}", full(&rule["id"]), full(&rule["label"]), full(&rule["on"]))
}

/// The ops a rule runs: `notify.send, workspaces.activate (after 3d)`, each cut to a table cell.
fn does(rule: &Value) -> String {
    does_with(rule, render::cell)
}

/// [`does`], each op shown with `show_op`: `full` where nothing else is cut (`show`, #254's review).
fn does_with(rule: &Value, show_op: fn(&Value) -> String) -> String {
    rule["actions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| match a["after_s"].as_u64() {
            Some(s) => format!("{} (after {})", show_op(&a["op"]), when::format_duration(s)),
            None => show_op(&a["op"]),
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
        // on/off under STATE, coloured like every other feature's states (ADR 0038 §6).
        let header = ["RULE", "STATE", "REACTS TO", "DOES", "LAST FIRED", ""].map(String::from);
        table(&header, &rows)
    };
    out.push_str(&problems(&data["problems"]));
    out
}

/// `list --removed`: each removed rule, and how to bring it back or delete it for good.
fn removed_list(data: &Value) -> String {
    let all = data["rules"].as_array().cloned().unwrap_or_default();
    if all.is_empty() {
        return "no removed rules".into();
    }
    let rows: Vec<Vec<String>> = all.iter().map(|r| vec![render::cell(&r["id"]), render::cell(&r["label"])]).collect();
    let mut out = table(&["RULE".into(), "LABEL".into()], &rows);
    out.push_str(
        "\nbring one back: shimmer automations restore RULE; delete for good: shimmer automations purge [RULE]",
    );
    out
}

/// Rule files that can't be used, under a list.
fn problems(v: &Value) -> String {
    let lines: Vec<String> = v
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| format!("  ✗ {}: {}", full(&p["file"]), full(&p["error"])))
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
    let mut out =
        format!("{}  ({})\n", paint(Role::Name, &full(&r["id"])), if r["enabled"] == true { "on" } else { "off" });
    let mut line = |label: &str, value: String| out.push_str(&format!("  {}{value}\n", crate::style::label(label, 12)));
    line("label", full(&r["label"]));
    line("reacts to", full(&r["on"]));
    line("does", does_with(r, full));
    line("last fired", ago(&r["last_fired"], now, "never"));
    if r["loop_stopped_at"].is_string() {
        line(
            "⚠ stopped",
            format!(
                "a loop {} (see: shimmer automations history {})",
                ago(&r["loop_stopped_at"], now, ""),
                full(&r["id"])
            ),
        );
    }
    format!("{out}\n{}", r["text"].as_str().unwrap_or_default().trim_end())
}

fn tested(data: &Value) -> String {
    let id = full(&data["rule"]);
    if data["matches"] != true {
        return format!("{id} doesn't react to that event (check its 'on' and 'when')");
    }
    let mut out = format!("{id} reacts, and would:");
    for a in data["actions"].as_array().into_iter().flatten() {
        let params = serde_json::to_string(&a["params"]).unwrap_or_default();
        out.push_str(&match a["after_s"].as_u64() {
            Some(s) => format!("\n  schedule {} {params} in {}", full(&a["op"]), when::format_duration(s)),
            None => format!("\n  queue {} {params}", full(&a["op"])),
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
    let rule = full(&e["rule"]);
    let topic = full(&e["event"]["topic"]);
    if e["kind"] == "stopped" {
        let why =
            if e["reason"] == "cycle" { "it was already in the chain (a loop)" } else { "the chain was too deep" };
        return format!("{when}  ⚠ {rule} stopped on {topic}: {why}");
    }
    let mut did = Vec::new();
    for q in e["queued"].as_array().into_iter().flatten() {
        did.push(format!("queued {}", full(&q["op"])));
    }
    for s in e["scheduled"].as_array().into_iter().flatten() {
        let at = when::timestamp(&s["at"]).map_or("later".into(), |at| when::relative(at, now));
        did.push(format!("scheduled {} {at}", full(&s["op"])));
    }
    for f in e["failed"].as_array().into_iter().flatten() {
        did.push(format!("✗ {} failed: {}", full(&f["op"]), full(&f["error"])));
    }
    format!("{when}  {rule} on {topic}: {}", did.join(", "))
}

fn status(data: &Value, now: DateTime<Utc>) -> String {
    let mut out = format!(
        "rules: {} ({} on)\ndepth: {} (shimmer automations depth N)",
        full(&data["rules"]),
        full(&data["enabled"]),
        full(&data["depth"])
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
        assert_eq!(p(""), Ok(AutomationsCmd::List { removed: false }));
        assert_eq!(p("list --removed"), Ok(AutomationsCmd::List { removed: true }));
        assert_eq!(p("rename a b"), Ok(AutomationsCmd::Rename { id: "a".into(), to: "b".into() }));
        let copy = AutomationsCmd::Copy { id: "a".into(), to: "b".into(), label: Some("B".into()) };
        assert_eq!(p("copy a b --label B"), Ok(copy));
        assert!(p("rename a").unwrap_err().contains("usage: shimmer automations rename RULE NEW"));
        assert_eq!(p("show solved"), Ok(AutomationsCmd::Show { id: "solved".into() }));
        assert_eq!(p("new x --file r.toml"), Ok(AutomationsCmd::New { id: "x".into(), file: Some("r.toml".into()) }));
        assert_eq!(p("edit x"), Ok(AutomationsCmd::Edit { id: "x".into(), file: None }));
        assert_eq!(p("test x --event e.json"), Ok(AutomationsCmd::Test { id: "x".into(), event: "e.json".into() }));
        assert_eq!(p("purge"), Ok(AutomationsCmd::Purge { id: None, yes: false }));
        assert_eq!(p("purge x --yes"), Ok(AutomationsCmd::Purge { id: Some("x".into()), yes: true }));
        assert!(p("remove x --yes").is_err(), "--yes is purge's only");
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
            ("depth zero", "from 1 up"),
            ("history --since soon", "isn't a date"),
            ("list x", "takes no arguments"),
            ("purge a b", "usage: shimmer automations purge [REMOVED] [--yes]"),
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
            "problems": [{"file": "rules/bad.toml", "error": "on: no event 'x'"}]});
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
        let removed = removed_list(&json!({"rules": [{"id": "old", "label": "Old one"}]}));
        assert!(
            removed.contains("old") && removed.contains("Old one") && removed.contains("automations restore"),
            "{removed}"
        );
        assert_eq!(removed_list(&json!({"rules": []})), "no removed rules");
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

    /// The edit file is new, private, and never written through a planted file (#253's review).
    #[test]
    fn the_edit_file_is_private_and_never_reuses_a_planted_one() {
        use std::os::unix::fs::PermissionsExt as _;
        let path = new_private_file("x", "[rule]").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[rule]");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_file(&path);
    }
}
