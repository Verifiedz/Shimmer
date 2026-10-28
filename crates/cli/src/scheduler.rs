//! `shimmer scheduler …` (ADR 0015): the triggers that decide *when* work happens (CLAUDE.md
//! §1.2, §6.3). A trigger only enqueues a task; `shimmer queue` shows what then runs.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use shimmer_core::Result;

use crate::client::Client;
use crate::flags::Flags;
use crate::render::{self, table};
use crate::style::{paint, Role};
use crate::when;
use crate::workspaces::Prompt;

pub const USAGE: &str = "usage: shimmer scheduler <command>

commands:
  add WHEN [--catch-up WHAT] [--lane LANE] COMMAND…
                                        run COMMAND on a schedule, e.g. shimmer scheduler add
                                        weekdays 09:00 workspaces activate site (scheduler add --help)
  list [--all] [--removed]              the schedules you added: when each runs next, last ran,
                                        and its state; --all also modules' own (notify's, …);
                                        --removed the ones you removed, to restore or purge
  show TRIGGER                          one trigger in full
  disable TRIGGER                       stop it firing until enabled
  enable TRIGGER                        fire again, from now on (time spent off isn't 'missed')
  edit TRIGGER WHEN [--catch-up WHAT] [--lane LANE] COMMAND…
                                        change one you added, keeping its id and whether it's on
                                        (same words as add)
  remove TRIGGER                        remove one you added; restore brings it back (a module's
                                        own can only be disabled)
  restore REMOVED                       bring a removed schedule back; it runs again from now on
                                        (time spent removed isn't 'missed')
  purge [REMOVED] [--yes]               delete a removed schedule for good, or every removed one
                                        (asks first)

A trigger enqueues a task when it fires; 'shimmer queue' shows it running. TRIGGER is the id as
'scheduler list' shows it: its ending is enough (at least 4 characters). Add --json to any command for raw output.";

#[derive(Clone, Debug, PartialEq)]
pub enum SchedulerCmd {
    Help,
    /// `all`: modules' own triggers too (#244). `removed`: the removed ones instead (ADR 0038).
    List {
        all: bool,
        removed: bool,
    },
    Show {
        id: String,
    },
    /// `scheduler add WHEN … COMMAND…` (ADR 0031 §6).
    Add(crate::schedule::Planned),
    /// `scheduler add` alone, or `scheduler add --help`.
    AddHelp,
    /// `scheduler edit TRIGGER WHEN … COMMAND…`: a new schedule for the same id (ADR 0038 §6).
    Edit {
        id: String,
        plan: crate::schedule::Planned,
    },
    Disable {
        id: String,
    },
    Enable {
        id: String,
    },
    Remove {
        id: String,
    },
    Restore {
        id: String,
    },
    /// `id`: one removed trigger; `None` is every one (ADR 0038).
    Purge {
        id: Option<String>,
        yes: bool,
    },
}

/// What `--catch-up` takes, as typed (shell completion offers these, #188).
pub const CATCH_UP: &[&str] = &["skip", "run-once", "backfill"];

/// `words` are what follows `shimmer scheduler`.
pub fn parse(words: Vec<String>) -> std::result::Result<SchedulerCmd, String> {
    let mut words = words.into_iter();
    let Some(sub) = words.next() else { return Ok(SchedulerCmd::Help) };
    // The schedule in words, then the command as typed (ADR 0031 §6): its own parser.
    if sub == "add" {
        return match crate::schedule::parse(words.collect())? {
            crate::schedule::ScheduleCmd::Add(plan) => Ok(SchedulerCmd::Add(plan)),
            crate::schedule::ScheduleCmd::Help => Ok(SchedulerCmd::AddHelp),
        };
    }
    // `edit TRIGGER` then the same words as `add` (ADR 0038 §6).
    if sub == "edit" {
        let usage = "usage: shimmer scheduler edit TRIGGER WHEN [--catch-up WHAT] [--lane LANE] COMMAND…";
        let id = words.next().filter(|w| !w.starts_with('-')).ok_or(usage)?;
        return match crate::schedule::parse(words.collect())? {
            crate::schedule::ScheduleCmd::Add(plan) => Ok(SchedulerCmd::Edit { id, plan }),
            crate::schedule::ScheduleCmd::Help => Err(usage.into()),
        };
    }
    let switches: &[&str] = match sub.as_str() {
        "list" => &["--all", "--removed"],
        "purge" => &["--yes"],
        _ => &[],
    };
    let flags = Flags::split(words.collect(), &[], switches).map_err(|e| format!("scheduler {sub}: {e}"))?;
    let id = || match flags.positional.as_slice() {
        [id] => Ok(id.clone()),
        _ if sub == "restore" => Err("usage: shimmer scheduler restore REMOVED".to_owned()),
        _ => Err(format!("usage: shimmer scheduler {sub} TRIGGER")),
    };
    match sub.as_str() {
        "help" => Ok(SchedulerCmd::Help),
        "list" if flags.positional.is_empty() => {
            Ok(SchedulerCmd::List { all: flags.has("--all"), removed: flags.has("--removed") })
        }
        "list" => Err("'scheduler list' takes no arguments".into()),
        "show" => Ok(SchedulerCmd::Show { id: id()? }),
        "disable" => Ok(SchedulerCmd::Disable { id: id()? }),
        "enable" => Ok(SchedulerCmd::Enable { id: id()? }),
        "remove" => Ok(SchedulerCmd::Remove { id: id()? }),
        "restore" => Ok(SchedulerCmd::Restore { id: id()? }),
        "purge" => match flags.positional.as_slice() {
            [] => Ok(SchedulerCmd::Purge { id: None, yes: flags.has("--yes") }),
            [id] => Ok(SchedulerCmd::Purge { id: Some(id.clone()), yes: flags.has("--yes") }),
            _ => Err("usage: shimmer scheduler purge [REMOVED] [--yes]".into()),
        },
        other => Err(format!("unknown scheduler command '{other}'; see 'shimmer scheduler --help'")),
    }
}

pub async fn run(client: &mut Client, cmd: &SchedulerCmd, json: bool, prompt: &mut dyn Prompt) -> Result<String> {
    let now = Utc::now();
    let one = |id: &str, op: &'static str| (op, json!({ "trigger_id": id }));
    let (op, params) = match cmd {
        SchedulerCmd::Help => return Ok(USAGE.into()),
        SchedulerCmd::List { removed: true, .. } => {
            let data = client.call("scheduler.list", json!({"removed": true})).await?;
            if json {
                return Ok(render::json(&data));
            }
            let triggers = data["triggers"].as_array().cloned().unwrap_or_default();
            if triggers.is_empty() {
                return Ok("no removed schedules".into());
            }
            let schemas = schemas_for(client, &triggers).await;
            return Ok(list(&data, now, &schemas, true));
        }
        SchedulerCmd::List { all, .. } => {
            let data = client.call("scheduler.list", json!({})).await?;
            // --json stays complete, so scripts see every trigger either way.
            if json {
                return Ok(render::json(&data));
            }
            let triggers = data["triggers"].as_array().cloned().unwrap_or_default();
            let schemas = schemas_for(client, &triggers).await;
            return Ok(list(&data, now, &schemas, *all));
        }
        SchedulerCmd::Show { id } => {
            let data = client.call("scheduler.show", json!({"trigger_id": id})).await?;
            if json {
                return Ok(render::json(&data));
            }
            let schemas = schemas_for(client, std::slice::from_ref(&data)).await;
            return Ok(trigger(&data, now, &schemas));
        }
        SchedulerCmd::AddHelp => return Ok(crate::schedule::USAGE.into()),
        SchedulerCmd::Add(plan) => {
            let cmd = crate::schedule::ScheduleCmd::Add(plan.clone());
            return crate::schedule::run(client, &cmd, json, prompt).await;
        }
        SchedulerCmd::Edit { id, plan } => return crate::schedule::edit(client, id, plan, json, prompt).await,
        SchedulerCmd::Disable { id } => one(id, "scheduler.disable"),
        SchedulerCmd::Enable { id } => one(id, "scheduler.enable"),
        SchedulerCmd::Remove { id } => one(id, "scheduler.remove"),
        SchedulerCmd::Restore { id } => {
            let data = client.call("scheduler.restore", json!({"trigger_id": id})).await?;
            if json {
                return Ok(render::json(&data));
            }
            let shown = render::short_id(data["id"].as_str().unwrap_or("?"));
            let what = format!("{shown} ({}, {})", render::cell(&data["op"]), schedule(&data["schedule"]));
            return Ok(format!("✓ restored {what}: it runs again from now on"));
        }
        SchedulerCmd::Purge { id, yes } => return purge(client, id.as_deref(), *yes, json, prompt).await,
    };
    // Find the trigger first, then act on its full id, and say which one it was and what it runs:
    // never just echo what was typed (#183's review). A removed trigger can't be looked up after.
    let trigger = match client.call("scheduler.show", params.clone()).await {
        Ok(t) => t,
        // A file the daemon couldn't read has no schedule to show (#263 finding 5): removing it by
        // the id `scheduler list` gave it moves the file aside, and the daemon says so.
        Err(e) if e.code == shimmer_core::ErrorCode::NotFound && op == "scheduler.remove" => {
            let data = client.call(op, params).await.map_err(|_| e)?;
            if json {
                return Ok(render::json(&data));
            }
            let id = render::cell(&data["trigger_id"]);
            return Ok(format!(
                "✓ moved {id}'s unreadable file to {}: fix it there, then shimmer scheduler restore {id}",
                render::cell(&data["file"])
            ));
        }
        Err(e) => return Err(e),
    };
    let full = trigger["id"].as_str().unwrap_or("?").to_string();
    let data = client.call(op, json!({ "trigger_id": full })).await?;
    if json {
        return Ok(render::json(&data));
    }
    let shown = render::short_id(&full);
    let what = format!("{shown} ({}, {})", render::cell(&trigger["op"]), schedule(&trigger["schedule"]));
    Ok(match op {
        "scheduler.disable" => format!("✓ {what} is off: it won't fire until 'shimmer scheduler enable {shown}'"),
        "scheduler.enable" => format!("✓ {what} is on: it fires again from now on"),
        _ => format!("✓ removed {what} (undo: shimmer scheduler restore {shown})"),
    })
}

/// `scheduler purge [TRIGGER]` (ADR 0038): the daemon says how many go; ask, then send that count
/// back.
async fn purge(
    client: &mut Client,
    id: Option<&str>,
    yes: bool,
    json: bool,
    prompt: &mut dyn Prompt,
) -> Result<String> {
    let mut params = json!({});
    if let Some(id) = id {
        params["trigger_id"] = json!(id);
    }
    let data = match client.call("scheduler.purge", params.clone()).await {
        Err(e) if e.code == shimmer_core::ErrorCode::ConfirmationRequired => {
            let what = match id {
                Some(id) => format!("the removed schedule '{id}'"),
                None => "every removed schedule".to_owned(),
            };
            if !crate::records::confirm_purge(prompt, yes, &what, &e.message)? {
                return Ok("nothing purged".into());
            }
            params["confirm"] = e.detail.unwrap_or_default();
            client.call("scheduler.purge", params).await?
        }
        other => other?,
    };
    if json {
        return Ok(render::json(&data));
    }
    Ok(match data["purged"].as_u64().unwrap_or(0) {
        0 => "no removed schedules to purge".to_owned(),
        n => format!("✓ {n} removed schedule(s) deleted for good"),
    })
}

/// Every trigger in one table, or a line saying there are none.
/// The schedules a person added, and modules' one-time ones (an automation's delayed action is
/// yours to see and remove, ADR 0035 §4); with `all`, modules' declared ones too, and a FROM column
/// to tell them apart (#244). Those are never secret: a footer says how many `--all` would add.
fn list(data: &Value, now: DateTime<Utc>, schemas: &Schemas, all: bool) -> String {
    let every = data["triggers"].as_array().cloned().unwrap_or_default();
    let (triggers, hidden): (Vec<Value>, usize) = if all {
        (every, 0)
    } else {
        let mine: Vec<Value> = every.iter().filter(|t| t["source"]["type"] != "module").cloned().collect();
        let hidden = every.len() - mine.len();
        (mine, hidden)
    };
    let mut footer = match hidden {
        0 => String::new(),
        1 => format!("\n{}", paint(Role::Hint, "+ 1 built-in schedule (shimmer scheduler list --all)")),
        n => format!("\n{}", paint(Role::Hint, &format!("+ {n} built-in schedules (shimmer scheduler list --all)"))),
    };
    footer.push_str(&broken(data));
    if triggers.is_empty() && !footer.is_empty() && hidden == 0 {
        return footer.trim_start().to_owned();
    }
    if triggers.is_empty() {
        return format!(
            "no triggers yet; add one with: shimmer scheduler add WHEN COMMAND (shimmer scheduler add --help){footer}"
        );
    }
    let ids: Vec<String> = triggers.iter().map(|t| render::cell(&t["id"])).collect();
    let rows: Vec<Vec<String>> = triggers
        .iter()
        .zip(render::short_ids(&ids))
        .map(|(t, id)| {
            vec![
                id,
                command_of(&t["op"], &t["params"], schemas),
                schedule(&t["schedule"]),
                catch_up(&t["catch_up"]),
                when::timestamp(&t["next_due"]).map_or("-".into(), |at| when::relative(at, now)),
                when::timestamp(&t["last_run"]).map_or("never".into(), |at| when::relative(at, now)),
                state(t),
                source(&t["source"]),
            ]
        })
        .map(|mut row: Vec<String>| {
            // Without --all every row is yours: no FROM column to say so.
            if !all {
                row.pop();
            }
            row
        })
        .collect();
    let mut header: Vec<String> =
        ["ID", "COMMAND", "SCHEDULE", "CATCH-UP", "NEXT", "LAST", "STATE", "FROM"].map(String::from).to_vec();
    if !all {
        header.pop();
    }
    format!("{}{footer}", table(&header, &rows))
}

/// Schedule files the daemon couldn't read (#263 finding 5), one warning line each, after the
/// table: a schedule never silently stops existing. Empty when there are none.
fn broken(data: &Value) -> String {
    let broken = data["broken"].as_array().cloned().unwrap_or_default();
    broken
        .iter()
        .map(|b| {
            format!(
                "\n{}",
                paint(
                    Role::Warning,
                    &format!(
                        "! {} can't be read: {} ({}). Fix it and restart (shimmer shutdown), or move it aside: shimmer scheduler remove {}",
                        render::cell(&b["id"]),
                        render::cell(&b["problem"]),
                        render::cell(&b["file"]),
                        render::cell(&b["id"]),
                    )
                )
            )
        })
        .collect()
}

/// One trigger as label / value lines.
fn trigger(t: &Value, now: DateTime<Utc>, schemas: &Schemas) -> String {
    let time = |v: &Value, none: &str| {
        when::timestamp(v).map_or(none.to_string(), |at| format!("{} ({})", when::local(at), when::relative(at, now)))
    };
    let id = paint(Role::Name, &render::cell(&t["id"]));
    let mut out = format!("{id}  ({})\n", crate::style::state(&state(t)));
    let mut line = |label: &str, value: String| out.push_str(&format!("  {}{value}\n", crate::style::label(label, 10)));
    line("command", command_of(&t["op"], &t["params"], schemas));
    line("op", render::cell(&t["op"]));
    line("params", serde_json::to_string(&t["params"]).unwrap_or_default());
    line("schedule", schedule(&t["schedule"]));
    line("catch-up", catch_up(&t["catch_up"]));
    line("lane", if t["lane"].is_null() { "the op's own".into() } else { render::cell(&t["lane"]) });
    line("next run", time(&t["next_due"], "-"));
    line("last run", time(&t["last_run"], "never"));
    line("from", source(&t["source"]));
    match t["waiting"].as_u64() {
        Some(1) => line("waiting", "1 run in the queue".into()),
        Some(n) if n > 1 => line("waiting", format!("{n} runs in the queue")),
        _ => {}
    }
    if let Some(cut) = t["interrupted"].as_object() {
        line(
            "cut off",
            paint(
                Role::Warning,
                &format!(
                    "the run for {} was stopped mid-way by a shutdown and not run again (task {})",
                    time(&cut["scheduled_for"].clone(), "?"),
                    render::cell(&cut["task_id"])
                ),
            ),
        );
    }
    out.trim_end().to_string()
}

/// The collections' definitions, by id, for reading records schedules back (their fields' types).
pub type Schemas = std::collections::BTreeMap<String, Value>;

/// What a trigger runs, as the command you'd type (ADR 0031 §3): `workspaces activate site`,
/// `records complete habits/stretch`. **Exactly, or not at all** (#186's review): a candidate is
/// rebuilt from op + params, then parsed back with the same parsers running a command uses, and is
/// shown only if that gives back exactly this op + params. Anything else shows as op + params.
/// Presentation only: the daemon stores, and `--json` shows, op + params.
fn command_of(op: &Value, params: &Value, schemas: &Schemas) -> String {
    let op = render::cell(op);
    let mut params = params.clone();
    // Stamped by the scheduler on each firing (ADR 0004), never typed.
    if let Some(p) = params.as_object_mut() {
        p.remove("scheduled_for");
    }
    for words in candidates(&op, &params) {
        if parses_back_to(&words, &op, &params, schemas) {
            return words.iter().map(|w| shell_word(w)).collect::<Vec<_>>().join(" ");
        }
    }
    format!("{op} {}", serde_json::to_string(&params).unwrap_or_default())
}

/// Commands that might send `op` + `params`, most natural first: for a `records.list` filter,
/// `--FIELD VALUE` first, then the same as `--filter "FIELD=VALUE"` (a field named like an
/// option, #178). Unchecked: [`parses_back_to`] decides.
fn candidates(op: &str, params: &Value) -> Vec<Vec<String>> {
    let Some(target) = crate::packs::pack::TARGETS.iter().find(|t| t.op == op) else { return Vec::new() };
    let Some(p) = params.as_object() else { return Vec::new() };
    let base: Vec<String> = target.words.iter().map(|w| w.to_string()).collect();
    if op == "notify.send" {
        let mut words = base;
        let Some(title) = p.get("title").and_then(Value::as_str) else { return Vec::new() };
        words.push(title.to_owned());
        for key in ["body", "priority"] {
            if let Some(v) = p.get(key).and_then(Value::as_str) {
                words.extend([format!("--{key}"), v.to_owned()]);
            }
        }
        return vec![words];
    }
    if op.starts_with("workspaces.") {
        let mut words = base;
        words.extend(p.get("id").and_then(Value::as_str).map(String::from));
        return vec![words];
    }
    if !op.starts_with("records.") {
        return Vec::new();
    }
    [false, true]
        .into_iter()
        .filter_map(|as_filter| {
            let mut words = base.clone();
            match (p.get("collection").and_then(Value::as_str), p.get("id").and_then(Value::as_str)) {
                (Some(c), Some(id)) => words.push(format!("{c}/{id}")),
                (Some(c), None) => words.push(c.into()),
                _ => return None,
            }
            for (key, value) in p {
                match key.as_str() {
                    "collection" | "id" => {}
                    "fields" => {
                        for (name, v) in value.as_object()? {
                            field_words(&mut words, name, v)?;
                        }
                    }
                    "filter" => {
                        for (name, v) in value.as_object()? {
                            filter_words(&mut words, name, v, as_filter)?;
                        }
                    }
                    // A true switch is the bare flag; false is never sent.
                    "clear_stamp" | "force" if value == &Value::Bool(true) => words.push(flag(key)),
                    "search" => words.extend([flag(key), value.as_str()?.to_owned()]),
                    "limit" | "offset" => words.extend([flag(key), value.as_u64()?.to_string()]),
                    "sort" => {
                        for k in value.as_array()? {
                            words.extend(["--sort".to_owned(), k.as_str()?.to_owned()]);
                        }
                    }
                    _ => return None,
                }
            }
            Some(words)
        })
        .collect()
}

fn flag(name: &str) -> String {
    format!("--{}", name.replace('_', "-"))
}

/// A field's change as the flags that make it: a value is `--F V` (a list as its items joined by
/// `, `), `null` is `--unset F`, a list patch is `--add F ITEM` / `--remove F ITEM` per item.
fn field_words(words: &mut Vec<String>, name: &str, v: &Value) -> Option<()> {
    match v {
        Value::Null => words.extend(["--unset".to_owned(), name.to_owned()]),
        Value::Object(patch) => {
            for (key, items) in patch {
                let which = match key.as_str() {
                    "add" => "--add",
                    "remove" => "--remove",
                    _ => return None,
                };
                for item in items.as_array()? {
                    words.extend([which.to_owned(), name.to_owned(), item.as_str()?.to_owned()]);
                }
            }
        }
        other => words.extend([flag(name), plain(other)?]),
    }
    Some(())
}

/// One `records.list` filter entry as flags: a plain value is `--F V` (or `--filter "F=V"`), an
/// operator a `--filter` condition, `set` is `--has F` / `--missing F`, `or` one `--or` each.
fn filter_words(words: &mut Vec<String>, name: &str, v: &Value, as_filter: bool) -> Option<()> {
    const OPS: [(&str, &str); 7] =
        [("eq", "="), ("ne", "!="), ("lt", "<"), ("lte", "<="), ("gt", ">"), ("gte", ">="), ("contains", "~")];
    let condition = |name: &str, op: &str, v: &Value| -> Option<String> {
        if op == "in" {
            let items: Option<Vec<String>> = v.as_array()?.iter().map(plain).collect();
            return Some(format!("{name}={}", items?.join("|")));
        }
        if op == "has" {
            return Some(format!("{name} has {}", plain(v)?));
        }
        let (_, sym) = OPS.iter().find(|(o, _)| *o == op)?;
        Some(format!("{name}{sym}{}", plain(v)?))
    };
    match (name, v) {
        ("or", Value::Array(alternatives)) => {
            for alt in alternatives {
                let (field, ops) = alt.as_object()?.iter().next()?;
                let (op, value) = ops.as_object()?.iter().next()?;
                words.extend(["--or".to_owned(), condition(field, op, value)?]);
            }
        }
        (_, Value::Object(ops)) => {
            for (op, value) in ops {
                match (op.as_str(), value) {
                    ("set", Value::Bool(true)) => words.extend(["--has".to_owned(), name.to_owned()]),
                    ("set", Value::Bool(false)) => words.extend(["--missing".to_owned(), name.to_owned()]),
                    _ => words.extend(["--filter".to_owned(), condition(name, op, value)?]),
                }
            }
        }
        _ if as_filter => words.extend(["--filter".to_owned(), condition(name, "eq", v)?]),
        _ => words.extend([flag(name), plain(v)?]),
    }
    Some(())
}

/// A scalar or a list of strings as typed text; anything else can't be.
fn plain(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Array(items) => {
            let items: Option<Vec<&str>> = items.iter().map(Value::as_str).collect();
            Some(items?.join(", "))
        }
        _ => None,
    }
}

/// Does typing `words` send exactly `op` + `params`? Parsed and turned into a request by the very
/// functions running the command uses (`records::request`, `schedule::workspaces_op`).
fn parses_back_to(words: &[String], op: &str, params: &Value, schemas: &Schemas) -> bool {
    let Some((first, rest)) = words.split_first() else { return false };
    let got = match first.as_str() {
        "records" => crate::records::parse(rest.to_vec()).ok().and_then(|cmd| {
            let schema = cmd.collection().and_then(|c| schemas.get(c)).cloned().unwrap_or(Value::Null);
            crate::records::request(&cmd, &schema).ok()
        }),
        "workspaces" => {
            crate::workspaces::parse(rest.to_vec()).ok().and_then(|cmd| crate::schedule::workspaces_op(&cmd))
        }
        "notify" => crate::notify::parse(rest.to_vec()).ok().as_ref().and_then(crate::notify::scheduled_send),
        _ => None,
    };
    got.is_some_and(|(got_op, got_params)| got_op == op && &got_params == params)
}

/// `text` as one word a shell reads back as it is: bare when it's safe, else in single quotes.
pub(crate) fn shell_word(text: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !text.is_empty() && text.chars().all(safe) {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// The definitions of the collections `triggers` touch, fetched once, for [`command_of`]. Empty
/// (each records command then shows as op + params) when there are none or the call fails.
pub async fn schemas_for(client: &mut Client, triggers: &[Value]) -> Schemas {
    if !triggers.iter().any(|t| t["op"].as_str().is_some_and(|op| op.starts_with("records."))) {
        return Schemas::new();
    }
    let Ok(all) = client.call("records.collections", json!({})).await else { return Schemas::new() };
    all["collections"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| Some((c["id"].as_str()?.to_owned(), c.clone())))
        .collect()
}

/// `every 2d`, `weekdays 09:00 (local)`, `cron 0 9 * * 1 (UTC)` or `once 2026-10-10 09:00:00`.
fn schedule(v: &Value) -> String {
    if let Some(secs) = v["every"].as_u64() {
        format!("every {}", when::format_duration(secs))
    } else if let Some(expr) = v["cron"].as_str() {
        format!("cron {expr} (UTC)")
    } else if let Some(expr) = v["cron_local"].as_str() {
        local_words(expr).unwrap_or_else(|| format!("cron {expr} (local)"))
    } else if let Some(at) = when::timestamp(&v["once"]) {
        format!("once {}", when::local(at))
    } else {
        render::cell(v)
    }
}

/// A local cron as `shimmer schedule` writes it, when it is one of its forms: `0 9 * * 1-5` →
/// `weekdays 09:00 (local)`.
fn local_words(expr: &str) -> Option<String> {
    const NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
    let [minute, hour, "*", "*", days] = expr.split_whitespace().collect::<Vec<_>>()[..] else { return None };
    let (minute, hour): (u32, u32) = (minute.parse().ok()?, hour.parse().ok()?);
    let days = match days {
        "*" => "daily".to_owned(),
        "1-5" => "weekdays".to_owned(),
        "0,6" => "weekends".to_owned(),
        list => list
            .split(',')
            .map(|d| d.parse::<usize>().ok().and_then(|i| NAMES.get(i)).copied())
            .collect::<Option<Vec<_>>>()?
            .join(","),
    };
    (hour < 24 && minute < 60).then(|| format!("{days} {hour:02}:{minute:02} (local)"))
}

/// The protocol's `run_once` as the CLI spells it.
fn catch_up(v: &Value) -> String {
    render::cell(v).replace('_', "-")
}

fn state(t: &Value) -> String {
    if t["done"].as_bool() == Some(true) {
        "done".into()
    } else if t["enabled"].as_bool() == Some(false) {
        "off".into()
    } else {
        "on".into()
    }
}

/// `you`, `module <id>` for one a module declared, or `<id>: <label>` for a one-time schedule a
/// module added (ADR 0035 §5.6). Module ones can be disabled, not removed.
fn source(v: &Value) -> String {
    match v["type"].as_str() {
        Some("user") => "you".into(),
        Some("module") => format!("module {}", v["id"].as_str().unwrap_or("?")),
        Some("added") => format!("{}: {}", v["id"].as_str().unwrap_or("?"), v["label"].as_str().unwrap_or("")),
        _ => render::cell(v),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    /// ADR 0035 §4 (Wasay on #251): an automation's delayed action shows by default, beside yours;
    /// only modules' declared schedules wait for `--all`.
    #[test]
    fn the_default_list_shows_yours_and_automations_delayed_actions() {
        let now = Utc::now();
        let t = |id: &str, source: Value| {
            json!({"id": id, "source": source, "schedule": {"every": 3600}, "op": "records.list",
                   "params": {}, "catch_up": "skip", "next_due": null, "last_run": null, "enabled": true, "done": false})
        };
        let data = json!({"triggers": [
            t("usr-1", json!({"type": "user"})),
            t("aut-1", json!({"type": "added", "module": "automations", "label": "Redo two-sum"})),
            t("notify.flush", json!({"type": "module", "id": "notify"}))]});
        let out = list(&data, now, &schemas(), false);
        assert!(out.contains("usr-1") && out.contains("aut-1"), "{out}");
        assert!(!out.contains("notify.flush") && out.contains("+ 1 built-in schedule"), "{out}");
    }

    fn p(s: &[&str]) -> std::result::Result<SchedulerCmd, String> {
        parse(s.iter().map(|w| w.to_string()).collect())
    }

    #[test]
    fn add_takes_the_schedule_in_words_then_the_command() {
        // ADR 0031 §6: one way to schedule from the CLI.
        let Ok(SchedulerCmd::Add(plan)) =
            p(&["add", "weekdays", "09:00", "--catch-up", "skip", "workspaces", "activate", "site"])
        else {
            panic!("not an add")
        };
        assert_eq!(plan.when.schedule, json!({"cron_local": "0 9 * * 1-5"}));
        assert_eq!(schedule(&plan.when.schedule), "weekdays 09:00 (local)");
        assert_eq!(plan.command, ["workspaces", "activate", "site"]);
        assert_eq!(p(&["add"]), Ok(SchedulerCmd::AddHelp));
        let Ok(SchedulerCmd::Edit { id, plan }) =
            p(&["edit", "usr-1", "daily", "09:00", "--catch-up", "skip", "workspaces", "stop", "site"])
        else {
            panic!("edit parses as add's words after the trigger")
        };
        assert_eq!((id.as_str(), plan.command.join(" ")), ("usr-1", "workspaces stop site".into()));
        assert!(p(&["edit"]).unwrap_err().contains("scheduler edit TRIGGER WHEN"));
        assert!(p(&["edit", "--catch-up"]).unwrap_err().contains("scheduler edit TRIGGER WHEN"));
        assert_eq!(p(&["add", "--help"]), Ok(SchedulerCmd::AddHelp));
        // The raw form went: it says what to type instead.
        let raw = p(&["add", "records.list", "{}", "--every", "1d", "--catch-up", "skip"]).unwrap_err();
        assert!(raw.contains("shimmer call scheduler.add"), "{raw}");
    }

    #[test]
    fn simple_commands_parse() {
        assert_eq!(p(&[]), Ok(SchedulerCmd::Help));
        assert_eq!(p(&["list"]), Ok(SchedulerCmd::List { all: false, removed: false }));
        assert_eq!(p(&["list", "--all"]), Ok(SchedulerCmd::List { all: true, removed: false }));
        assert_eq!(p(&["list", "--removed"]), Ok(SchedulerCmd::List { all: false, removed: true }));
        assert_eq!(p(&["restore", "usr-1"]), Ok(SchedulerCmd::Restore { id: "usr-1".into() }));
        assert!(p(&["restore"]).unwrap_err().contains("REMOVED"));
        assert_eq!(p(&["purge"]), Ok(SchedulerCmd::Purge { id: None, yes: false }));
        assert_eq!(p(&["purge", "usr-1", "--yes"]), Ok(SchedulerCmd::Purge { id: Some("usr-1".into()), yes: true }));
        assert!(p(&["remove", "usr-1", "--yes"]).is_err(), "--yes is purge's only");
        assert!(p(&["show", "x", "--all"]).is_err(), "--all is list's only");
        for (word, want) in [
            ("show", SchedulerCmd::Show { id: "usr-1".into() }),
            ("disable", SchedulerCmd::Disable { id: "usr-1".into() }),
            ("enable", SchedulerCmd::Enable { id: "usr-1".into() }),
            ("remove", SchedulerCmd::Remove { id: "usr-1".into() }),
        ] {
            assert_eq!(p(&[word, "usr-1"]), Ok(want));
            assert!(p(&[word]).unwrap_err().contains("TRIGGER"), "{word}");
        }
        assert!(p(&["frob"]).unwrap_err().contains("unknown scheduler command"));
    }

    fn sample(now: DateTime<Utc>) -> Value {
        json!({"triggers": [
            {"id": "usr-1", "source": {"type": "user"}, "schedule": {"every": 172_800}, "op": "records.list",
             "params": {"collection": "leetcode", "filter": {}}, "catch_up": "run_once", "lane": null, "fallback": null,
             "next_due": (now + Duration::seconds(7_201)).to_rfc3339(), "last_run": null, "enabled": true, "done": false},
            {"id": "digest", "source": {"type": "module", "id": "notify"}, "schedule": {"cron": "0 9 * * 1"},
             "op": "notify.digest", "params": {}, "catch_up": "skip", "lane": "notify", "fallback": null,
             "next_due": null, "last_run": (now - Duration::seconds(180)).to_rfc3339(), "enabled": false, "done": false}
        ]})
    }

    #[test]
    fn list_shows_each_trigger_readably() {
        let now = Utc::now();
        let out = list(&sample(now), now, &Schemas::new(), true);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["ID", "COMMAND", "SCHEDULE", "CATCH-UP", "NEXT", "LAST", "STATE", "FROM"]
        );
        let row = |i: usize| lines[i].split("  ").map(str::trim).filter(|c| !c.is_empty()).collect::<Vec<_>>();
        assert_eq!(row(1), ["usr-1", "records list leetcode", "every 2d", "run-once", "in 2h", "never", "on", "you"]);
        assert_eq!(
            row(2),
            ["digest", "notify.digest {}", "cron 0 9 * * 1 (UTC)", "skip", "-", "3m ago", "off", "module notify"]
        );
        assert!(list(&json!({"triggers": []}), now, &Schemas::new(), false).starts_with("no triggers yet"));
    }

    #[test]
    fn modules_own_schedules_are_one_footer_line_away() {
        crate::style::init(true, true, &Default::default());
        let now = Utc::now();
        let out = list(&sample(now), now, &Schemas::new(), false);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["ID", "COMMAND", "SCHEDULE", "CATCH-UP", "NEXT", "LAST", "STATE"],
            "no FROM column: every row is yours"
        );
        assert!(lines[1].starts_with("usr-1") && !out.contains("notify.digest"), "{out}");
        assert_eq!(lines.last().unwrap(), &"+ 1 built-in schedule (shimmer scheduler list --all)");
        // Only modules' own: still says there are some.
        let modules_only = json!({"triggers": [sample(now)["triggers"][1].clone()]});
        assert_eq!(
            list(&modules_only, now, &Schemas::new(), false),
            "no triggers yet; add one with: shimmer scheduler add WHEN COMMAND (shimmer scheduler add --help)\n+ 1 built-in schedule (shimmer scheduler list --all)"
        );
        assert!(!list(&sample(now), now, &Schemas::new(), true).contains("built-in schedule"), "--all hides nothing");
    }

    #[test]
    fn a_modules_one_time_schedule_shows_who_added_it_and_why() {
        let added = json!({"type": "added", "id": "automations", "label": "follow up on acme (rule nudge)"});
        assert_eq!(source(&added), "automations: follow up on acme (rule nudge)");
    }

    fn schemas() -> Schemas {
        let field = |name: &str, kind: &str| json!({"name": name, "type": kind});
        let lc = json!({"id": "lc", "fields": [
            field("title", "string"), field("number", "int"), field("url", "string"),
            {"name": "difficulty", "type": "enum", "values": ["easy", "medium", "hard"]},
            field("topic", "list"), field("last_solved", "date"), field("solved_without_help", "bool")]});
        // `limit` is also a `list` option (#178): it can only be reached with --filter.
        let jobs = json!({"id": "jobs", "fields": [
            field("position", "string"), field("apply_by", "date"), field("limit", "int"),
            {"name": "stage", "type": "enum", "values": ["saved", "applied", "offer"]}]});
        Schemas::from([("lc".to_owned(), lc), ("jobs".to_owned(), jobs)])
    }

    /// Words as a shell splits them: spaces separate, single quotes keep (`'\''` is a quote).
    fn shell_split(text: &str) -> Vec<String> {
        let (mut out, mut word, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' if !quoted => word.extend(chars.next()),
                '\'' => {
                    quoted = !quoted;
                    any = true;
                }
                ' ' if !quoted => {
                    if any || !word.is_empty() {
                        out.push(std::mem::take(&mut word));
                    }
                    any = false;
                }
                c => word.push(c),
            }
        }
        if any || !word.is_empty() {
            out.push(word);
        }
        out
    }

    /// What typing `line` sends: op + params, by the parsers running the command uses.
    fn sent(line: &str) -> (&'static str, Value) {
        let words = shell_split(line);
        let (op, params) = match words[0].as_str() {
            "records" => {
                let cmd = crate::records::parse(words[1..].to_vec()).unwrap();
                let schema = cmd.collection().and_then(|c| schemas().get(c).cloned()).unwrap_or(Value::Null);
                crate::records::request(&cmd, &schema).unwrap()
            }
            "notify" => crate::notify::scheduled_send(&crate::notify::parse(words[1..].to_vec()).unwrap()).unwrap(),
            _ => crate::schedule::workspaces_op(&crate::workspaces::parse(words[1..].to_vec()).unwrap()).unwrap(),
        };
        (op, params)
    }

    #[test]
    fn every_schedulable_command_reads_back_as_itself() {
        // #186's review: parse what's typed, rebuild it from the op + params it sends, and typing the
        // rebuilt text must send exactly the same. Commands added later belong in this list.
        for line in [
            "records add lc --title 'Two Sum' --number 1 --difficulty easy --topic 'array, hash'",
            "records add lc two-sum --title Hi",
            "records update lc/two-sum --unset url",
            "records update lc/two-sum --add topic array --remove topic graph",
            "records update lc/two-sum --url 'https://x.y/z?a=1&b=2' --solved-without-help true",
            "records update lc/two-sum --title 'It'\\''s fine'",
            "records complete lc/two-sum --last-solved 2026-10-01",
            "records complete lc/two-sum",
            "records reopen lc/two-sum --clear-stamp",
            "records reopen lc/two-sum",
            "records remove lc/two-sum --force",
            "records restore lc/two-sum",
            "workspaces activate site",
            "workspaces stop site",
            "workspaces cleanup site",
            "notify send 'Standup in 15'",
            "notify send Stretch --body 'stand up, walk' --priority low",
            // Through `scheduler add`: list filters, options, and a field named like an option.
            "records list jobs --stage offer",
            "records list jobs --filter limit=5",
            "records list lc --filter 'number>=3' --sort -number --limit 5 --search sum --has url --missing topic",
            "records list jobs --or stage=offer --or 'apply_by<=2026-11-01'",
        ] {
            let (op, params) = sent(line);
            let shown = command_of(&json!(op), &params, &schemas());
            assert!(!shown.starts_with(op), "{line}\n  fell back to op + params: {shown}");
            assert_eq!(sent(&shown), (op, params.clone()), "{line}\n  shown as: {shown}");
        }
    }

    #[test]
    fn a_trigger_reads_as_the_command_it_runs() {
        let c = |op: &str, params: Value| command_of(&json!(op), &params, &schemas());
        assert_eq!(c("workspaces.activate", json!({"id": "site"})), "workspaces activate site");
        assert_eq!(
            c("records.complete", json!({"collection": "lc", "id": "two-sum", "scheduled_for": "x"})),
            "records complete lc/two-sum",
            "scheduled_for is the scheduler's stamp, never typed"
        );
        // The cases from #186's review.
        let update =
            |fields: Value| c("records.update", json!({"collection": "lc", "id": "two-sum", "fields": fields}));
        assert_eq!(update(json!({"url": null})), "records update lc/two-sum --unset url");
        assert_eq!(update(json!({"topic": {"add": ["array"]}})), "records update lc/two-sum --add topic array");
        assert_eq!(
            c("records.reopen", json!({"collection": "lc", "id": "two-sum", "clear_stamp": true})),
            "records reopen lc/two-sum --clear-stamp"
        );
        assert_eq!(
            c("records.remove", json!({"collection": "lc", "id": "two-sum", "force": true})),
            "records remove lc/two-sum --force"
        );
        assert_eq!(
            c("records.add", json!({"collection": "lc", "fields": {"title": "Two Sum"}})),
            "records add lc --title 'Two Sum'",
            "quoted for a shell"
        );
    }

    #[test]
    fn what_cant_be_typed_exactly_shows_as_op_and_params() {
        let c = |op: &str, params: Value| command_of(&json!(op), &params, &schemas());
        assert_eq!(c("demo.echo", json!({"x": 1})), "demo.echo {\"x\":1}", "no command");
        // An operator `list` has no flag for, and a field the collection doesn't have.
        let odd = json!({"collection": "lc", "filter": {"number": {"between": [1, 2]}}});
        assert!(c("records.list", odd).starts_with("records.list {"), "unknown operator");
        let unknown = json!({"collection": "nosuch", "id": "x", "fields": {"n": 1}});
        assert!(c("records.update", unknown).starts_with("records.update {"), "no schema: can't be sure of types");
    }

    #[test]
    fn local_schedules_read_as_they_were_written() {
        let s = |expr: &str| schedule(&json!({"cron_local": expr}));
        assert_eq!(s("0 9 * * 1-5"), "weekdays 09:00 (local)");
        assert_eq!(s("30 18 * * *"), "daily 18:30 (local)");
        assert_eq!(s("5 8 * * 0,6"), "weekends 08:05 (local)");
        assert_eq!(s("0 7 * * 1,3,5"), "mon,wed,fri 07:00 (local)");
        assert_eq!(s("*/15 9 * * 1"), "cron */15 9 * * 1 (local)", "anything else as cron");
    }

    #[test]
    fn show_reads_naturally() {
        let now = Utc::now();
        let data = sample(now);
        let usr1 = &data["triggers"][0];
        assert_eq!(usr1["id"], "usr-1");
        let out = trigger(usr1, now, &schemas());
        assert!(out.starts_with("usr-1  (on)\n  command   records list leetcode\n  op        records.list\n  params    {\"collection\":\"leetcode\",\"filter\":{}}\n  schedule  every 2d"), "{out}");
        assert!(
            out.contains("  lane      the op's own") && out.contains("(in 2h)") && out.contains("  last run  never"),
            "{out}"
        );
    }

    /// A daemon that knows workspace `site`, accepts `scheduler.add`, and fails every other
    /// request, as one would if `scheduler.show` broke right after the trigger was created.
    async fn add_only_daemon(sock: &std::path::Path) {
        use shimmer_proto::{decode_client, encode, ClientFrame, ServerFrame};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::UnixListener::bind(sock).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (rd, mut wr) = stream.into_split();
            let mut lines = BufReader::new(rd).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let frame = match decode_client(&line).unwrap() {
                    ClientFrame::Hello { .. } => ServerFrame::welcome("test", "s1"),
                    ClientFrame::Request { id, op, .. } if op == "scheduler.add" => {
                        ServerFrame::ok(id, json!({"trigger_id": "usr-new"}))
                    }
                    ClientFrame::Request { id, op, .. } if op == "workspaces.show" => {
                        ServerFrame::ok(id, json!({"id": "site", "state": "ready"}))
                    }
                    ClientFrame::Request { id, op, .. } => {
                        ServerFrame::err(id, shimmer_core::Error::unavailable(format!("{op} broke")))
                    }
                    _ => continue,
                };
                wr.write_all(encode(&frame).unwrap().as_bytes()).await.unwrap();
            }
        });
    }

    #[tokio::test]
    async fn add_still_succeeds_and_says_why_when_its_details_cant_be_fetched() {
        // Wasay's review of #169: the trigger exists either way, so the command succeeds (a
        // script must not retry and add it twice), and says why it shows only the id.
        let dir = tempfile::TempDir::new().unwrap();
        let sock = dir.path().join("s.sock");
        add_only_daemon(&sock).await;
        let mut client = Client::connect(&sock).await.unwrap();
        let cmd = p(&["add", "every", "1d", "--catch-up", "skip", "workspaces", "activate", "site"]).unwrap();
        let out = run(&mut client, &cmd, false, &mut crate::workspaces::Terminal).await.unwrap();
        assert!(out.starts_with("✓ scheduled workspaces activate site · every 1d · catch-up skip"), "{out}");
        assert!(out.contains("first run: see 'shimmer scheduler show'") && out.contains("id usr-new"), "{out}");
    }
}
