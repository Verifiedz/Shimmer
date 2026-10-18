//! `shimmer notify …` (ADR 0032 §7): the inbox, sending, do-not-disturb and notify's settings.
//! Each command is one `notify.*` op; this file parses, asks when something deletes in bulk,
//! and draws the answers.

use std::fmt::Write as _;

use chrono::{DateTime, Local, NaiveTime, TimeZone, Utc};
use serde_json::{json, Map, Value};
use shimmer_core::{Error, ErrorCode, Result};

use crate::client::Client;
use crate::flags::Flags;
use crate::render;
use crate::style::{paint, Role};
use crate::when;
use crate::workspaces::Prompt;

pub const USAGE: &str = "usage: shimmer notify <command>

commands:
  list [--limit N]                     your messages, newest first, one line each
       [--unread] [--read] [--failed] [--held]
       [--from alerts|schedules|fallbacks|automations|send]
       [--priority low|normal|high] [--since DATE] [--before DATE] [--text WORDS]
  show ID                              one message in full (marks it read)
  count [list options]                 how many unread (or match), e.g. for a status bar
  mark-read [ID]… [--all] [list options]
                                       mark messages read
  mark-unread [ID]… [--all] [list options]
                                       mark messages unread again
  remove [ID]… [--all] [--yes] [list options]
                                       delete messages; asks first unless given ids
  send TITLE [--body TEXT] [--priority low|normal|high]
                                       send a message now
  test                                 check that messages reach you
  resend [ID]… [list options]          try messages again, e.g. shimmer notify resend --failed
  pause [DURATION|until TIME] [--all]  hold messages until you resume, e.g. pause 2h, pause until
                                       14:00; high priority still comes through unless --all
  resume                               stop holding; what waited arrives as one summary
  quiet [RANGE|off] [--weekdays RANGE] [--weekends RANGE] [--days DAYS RANGE]
                                       quiet hours, e.g. quiet 22:00-08:00, quiet --weekends 00:00-10:00
  status                               notify at a glance
  builtin-alerts [on|off] [NAME]       the built-in alerts: workspace-dirty, scheduled-task-failed
  cap [N]                              how many messages the inbox keeps (default 2000)

DATE is 2026-10-01 or \"2026-10-01 14:00\", in your local time; DAYS is like mon,wed,fri.
An ID can be the ending list shows. Messages are files in data/notify/inbox/.";

/// The filters every inbox command shares, as the ops take them (ADR 0032 §6).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filters(Map<String, Value>);

const FILTER_OPTIONS: [&str; 5] = ["--from", "--priority", "--since", "--before", "--text"];
const FILTER_SWITCHES: [&str; 4] = ["--unread", "--read", "--failed", "--held"];

impl Filters {
    fn from_flags(f: &Flags) -> std::result::Result<Self, String> {
        let mut m = Map::new();
        for sw in ["--unread", "--read"] {
            if f.has(sw) {
                m.insert(sw.trim_start_matches('-').into(), json!(true));
            }
        }
        match (f.has("--failed"), f.has("--held")) {
            (true, true) => return Err("--failed and --held match nothing together; pick one".into()),
            (true, false) => drop(m.insert("state".into(), json!("failed"))),
            (false, true) => drop(m.insert("state".into(), json!("held"))),
            _ => {}
        }
        for opt in FILTER_OPTIONS {
            if let Some(v) = f.value(opt) {
                m.insert(opt.trim_start_matches('-').into(), json!(v));
            }
        }
        Ok(Self(m))
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn params(&self) -> Value {
        Value::Object(self.0.clone())
    }
}

/// Which messages: ids (or their endings), `--all`, or filters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Selection {
    pub ids: Vec<String>,
    pub all: bool,
    pub filters: Filters,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PauseFor {
    UntilResume,
    For(u64),
    Until(NaiveTime),
}

#[derive(Clone, Debug, PartialEq)]
pub enum QuietSet {
    Show,
    Off,
    /// Day word (`every_day`, `weekdays`, `mon`…) → range.
    Set(Vec<(String, String)>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum NotifyCmd {
    Help,
    List { filters: Filters, limit: Option<u64> },
    Show { id: String },
    Count { filters: Filters },
    Mark { read: bool, selection: Selection },
    Remove { selection: Selection, yes: bool },
    Send { title: String, body: Option<String>, priority: Option<String> },
    Test,
    Resend { selection: Selection },
    Pause { until: PauseFor, all: bool },
    Resume,
    Quiet(QuietSet),
    Status,
    BuiltinAlerts(Option<(bool, String)>),
    Cap(Option<u64>),
}

/// `rest` is what follows `shimmer notify`.
pub fn parse(rest: Vec<String>) -> std::result::Result<NotifyCmd, String> {
    let mut words = rest.into_iter();
    let Some(verb) = words.next() else { return Ok(NotifyCmd::Help) };
    let rest: Vec<String> = words.collect();
    let filtered = |extra_opts: &[&'static str], extra_sw: &[&'static str], rest: Vec<String>| {
        let opts: Vec<&'static str> = FILTER_OPTIONS.iter().chain(extra_opts).copied().collect();
        let sws: Vec<&'static str> = FILTER_SWITCHES.iter().chain(extra_sw).copied().collect();
        let f = Flags::split(rest, &opts, &sws)?;
        let filters = Filters::from_flags(&f)?;
        Ok::<_, String>((f, filters))
    };
    let none = |rest: &[String]| match rest.first() {
        Some(w) => Err(format!("'notify {verb}' takes nothing after it, not '{w}'")),
        None => Ok(()),
    };
    let selection = |rest: Vec<String>, what: &str| -> std::result::Result<(Selection, Flags), String> {
        let (f, filters) = filtered(&[], &["--all", "--yes"], rest)?;
        let s = Selection { ids: f.positional.clone(), all: f.has("--all"), filters };
        let ways = usize::from(!s.ids.is_empty()) + usize::from(s.all) + usize::from(!s.filters.is_empty());
        match ways {
            0 => Err(format!("say which messages to {what}: ids, --all, or filters such as --read")),
            1 => Ok((s, f)),
            _ => Err(format!("{what} takes ids, --all or filters: one of them, not several")),
        }
    };
    match verb.as_str() {
        "help" => none(&rest).map(|()| NotifyCmd::Help),
        "list" => {
            let (f, filters) = filtered(&["--limit"], &[], rest)?;
            if let Some(w) = f.positional.first() {
                return Err(format!("'notify list' takes filters, not '{w}' (see shimmer notify --help)"));
            }
            let limit =
                f.value("--limit").map(|n| n.parse::<u64>().map_err(|_| format!("--limit is a number, not '{n}'")));
            Ok(NotifyCmd::List { filters, limit: limit.transpose()? })
        }
        "show" => match rest.as_slice() {
            [id] if !id.starts_with('-') => Ok(NotifyCmd::Show { id: id.clone() }),
            [] => Err("'notify show' needs an ID (see shimmer notify list)".into()),
            _ => Err("'notify show' takes one ID".into()),
        },
        "count" => {
            let (f, filters) = filtered(&[], &[], rest)?;
            none(&f.positional).map(|()| NotifyCmd::Count { filters })
        }
        "mark-read" | "mark-unread" => {
            let (selection, f) = selection(rest, "mark")?;
            if f.has("--yes") {
                return Err(format!("'notify {verb}' doesn't ask, so it takes no --yes"));
            }
            Ok(NotifyCmd::Mark { read: verb == "mark-read", selection })
        }
        "remove" => {
            let (selection, f) = selection(rest, "remove")?;
            Ok(NotifyCmd::Remove { selection, yes: f.has("--yes") })
        }
        "send" => {
            let f = Flags::split(rest, &["--body", "--priority"], &[])?;
            let title = match f.positional.as_slice() {
                [t] => t.clone(),
                [] => return Err("'notify send' needs a title, e.g. shimmer notify send \"Build finished\"".into()),
                _ => return Err("put the title in quotes: shimmer notify send \"Build finished\"".into()),
            };
            Ok(NotifyCmd::Send {
                title,
                body: f.value("--body").map(String::from),
                priority: f.value("--priority").map(String::from),
            })
        }
        "test" => none(&rest).map(|()| NotifyCmd::Test),
        "resend" => {
            let (selection, f) = selection(rest, "resend")?;
            if f.has("--yes") || selection.all {
                return Err("'notify resend' takes ids or filters, e.g. shimmer notify resend --failed".into());
            }
            Ok(NotifyCmd::Resend { selection })
        }
        "pause" => {
            let f = Flags::split(rest, &[], &["--all"])?;
            let until = match f.positional.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
                [] => PauseFor::UntilResume,
                ["until", time] => PauseFor::Until(
                    NaiveTime::parse_from_str(time, "%H:%M")
                        .map_err(|_| format!("'{time}' isn't a time: use HH:MM, e.g. 14:00"))?,
                ),
                [d] => PauseFor::For(when::parse_duration(d)?),
                _ => return Err("pause for a while (pause 2h) or until a time (pause until 14:00)".into()),
            };
            Ok(NotifyCmd::Pause { until, all: f.has("--all") })
        }
        "resume" => none(&rest).map(|()| NotifyCmd::Resume),
        "quiet" => parse_quiet(rest).map(NotifyCmd::Quiet),
        "status" => none(&rest).map(|()| NotifyCmd::Status),
        "builtin-alerts" => match rest.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            [] => Ok(NotifyCmd::BuiltinAlerts(None)),
            [state @ ("on" | "off"), name] => Ok(NotifyCmd::BuiltinAlerts(Some((*state == "on", name.to_string())))),
            _ => Err("'notify builtin-alerts' lists them, or: builtin-alerts on|off NAME".into()),
        },
        "cap" => match rest.as_slice() {
            [] => Ok(NotifyCmd::Cap(None)),
            [n] => n.parse().map(|n| NotifyCmd::Cap(Some(n))).map_err(|_| format!("the cap is a number, not '{n}'")),
            _ => Err("'notify cap' takes one number".into()),
        },
        other => Err(format!("unknown notify command '{other}'")),
    }
}

/// `quiet [RANGE|off] [--weekdays R] [--weekends R] [--days DAYS R]…`.
fn parse_quiet(rest: Vec<String>) -> std::result::Result<QuietSet, String> {
    const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    let mut set = Vec::new();
    let mut words = rest.into_iter();
    let mut off = false;
    while let Some(w) = words.next() {
        let mut value = |flag: &str| words.next().ok_or_else(|| format!("{flag} needs a range like 22:00-08:00"));
        match w.as_str() {
            "off" => off = true,
            "--weekdays" => set.push(("weekdays".to_owned(), value("--weekdays")?)),
            "--weekends" => set.push(("weekends".to_owned(), value("--weekends")?)),
            "--days" => {
                let days = value("--days")?;
                let range = value("--days DAYS")?;
                for d in days.split(',') {
                    if !DAYS.contains(&d) {
                        return Err(format!("'{d}' isn't a day: use mon, tue, wed, thu, fri, sat or sun"));
                    }
                    set.push((d.to_owned(), range.clone()));
                }
            }
            flag if flag.starts_with('-') => return Err(format!("unknown option '{flag}'")),
            range => set.push(("every_day".to_owned(), range.to_owned())),
        }
    }
    match (off, set.is_empty()) {
        (true, true) => Ok(QuietSet::Off),
        (true, false) => Err("'quiet off' clears them all; set new ones without 'off'".into()),
        (false, true) => Ok(QuietSet::Show),
        (false, false) => Ok(QuietSet::Set(set)),
    }
}

/// The op a scheduled `notify send` sends (ADR 0032 §7): marked `from: schedule`, so the inbox
/// can say where it came from. `None` for any other notify command: only sending is one change
/// worth scheduling.
pub fn scheduled_send(cmd: &NotifyCmd) -> Option<(&'static str, Value)> {
    let NotifyCmd::Send { title, body, priority } = cmd else { return None };
    let mut params = json!({"title": title, "from": "schedule"});
    if let Some(b) = body {
        params["body"] = json!(b);
    }
    if let Some(p) = priority {
        params["priority"] = json!(p);
    }
    Some(("notify.send", params))
}

// ---------------------------------------------------------------- running

pub async fn run(client: &mut Client, cmd: &NotifyCmd, json: bool, prompt: &mut dyn Prompt) -> Result<String> {
    let out = |data: Value, text: &dyn Fn(&Value) -> String| if json { render::json(&data) } else { text(&data) };
    match cmd {
        NotifyCmd::Help => Ok(USAGE.into()),
        NotifyCmd::List { filters, limit } => {
            let mut params = filters.params();
            if let Some(n) = limit {
                params["limit"] = json!(n);
            }
            let data = client.call("notify.list", params).await?;
            Ok(out(data, &|d| list(d, !filters.is_empty(), Utc::now())))
        }
        NotifyCmd::Show { id } => {
            let m = client.call("notify.show", json!({"id": id})).await?;
            if m["read"] == false {
                client.call("notify.mark_read", json!({"ids": [m["id"]]})).await?;
            }
            Ok(out(m, &|m| show(m, Utc::now())))
        }
        NotifyCmd::Count { filters } => {
            // With no filter, the unread count: what a status bar wants.
            let params = if filters.is_empty() { json!({"unread": true}) } else { filters.params() };
            let data = client.call("notify.count", params).await?;
            Ok(out(data, &|d| d["count"].to_string()))
        }
        NotifyCmd::Mark { read, selection } => {
            let op = if *read { "notify.mark_read" } else { "notify.mark_unread" };
            let params = selection_params(selection);
            let data = client.call(op, params).await?;
            let word = if *read { "read" } else { "unread" };
            Ok(out(data, &|d| match d["marked"].as_u64().unwrap_or(0) {
                0 => format!("nothing to mark {word}"),
                1 => format!("✓ marked 1 message {word}"),
                n => format!("✓ marked {n} messages {word}"),
            }))
        }
        NotifyCmd::Remove { selection, yes } => remove(client, selection, *yes, json, prompt).await,
        NotifyCmd::Send { title, body, priority } => {
            let mut params = json!({"title": title});
            if let Some(b) = body {
                params["body"] = json!(b);
            }
            if let Some(p) = priority {
                params["priority"] = json!(p);
            }
            let data = client.call("notify.send", params).await?;
            Ok(out(data, &|_| format!("✓ sent \"{title}\" (shimmer notify list)")))
        }
        NotifyCmd::Test => {
            let result = queued_result(client, "notify.test", json!({})).await?;
            Ok(out(result, &test))
        }
        NotifyCmd::Resend { selection } => {
            let params = selection_params(selection);
            let result = queued_result(client, "notify.resend", params).await?;
            Ok(out(result, &resent))
        }
        NotifyCmd::Pause { until, all } => {
            let until_time = match until {
                PauseFor::UntilResume => None,
                PauseFor::For(secs) => Some(Utc::now() + chrono::Duration::seconds(*secs as i64)),
                PauseFor::Until(t) => Some(next_local(*t, Local::now())),
            };
            let data = client.call("notify.pause", json!({"until": until_time, "all": all})).await?;
            Ok(out(data, &|_| {
                let till = until_time.map_or("until you resume".to_owned(), |t| format!("until {}", local_time(t)));
                let high = if *all { "everything waits" } else { "high priority still comes through" };
                format!("✓ paused {till} ({high})")
            }))
        }
        NotifyCmd::Resume => {
            let data = client.call("notify.resume", json!({})).await?;
            Ok(out(data, &|d| match (d["was_paused"] == true, d["held"].as_u64().unwrap_or(0)) {
                (false, _) => "notify wasn't paused".into(),
                (true, 0) => "✓ resumed; nothing was waiting".into(),
                (true, n) => {
                    format!("✓ resumed; {n} waiting {} arriving as one summary", plural(n, "message", "messages"))
                }
            }))
        }
        NotifyCmd::Quiet(set) => {
            let params = match set {
                QuietSet::Show => json!({}),
                QuietSet::Off => json!({"off": true}),
                QuietSet::Set(pairs) => Value::Object(pairs.iter().map(|(k, v)| (k.clone(), json!(v))).collect()),
            };
            let data = client.call("notify.quiet", params).await?;
            Ok(out(data, &|d| quiet(d, set != &QuietSet::Show)))
        }
        NotifyCmd::Status => {
            let data = client.call("notify.status", json!({})).await?;
            Ok(out(data, &|d| status(d, Utc::now())))
        }
        NotifyCmd::BuiltinAlerts(change) => {
            let params = match change {
                None => json!({}),
                Some((on, name)) => json!({"name": name, "on": on}),
            };
            let data = client.call("notify.builtin_alerts", params).await?;
            Ok(out(data, &alerts))
        }
        NotifyCmd::Cap(n) => {
            let params = n.map_or(json!({}), |n| json!({"max": n}));
            let data = client.call("notify.cap", params).await?;
            Ok(out(data, &|d| cap(d, n.is_some())))
        }
    }
}

/// A queued op run to its end: its result. The stream is subscribed first, so its finish can't
/// be missed (as `--wait` does, #142).
async fn queued_result(client: &mut Client, op: &str, params: Value) -> Result<Value> {
    client.subscribe(&["queue.task.*"]).await?;
    let handle = client.call(op, params).await?;
    let task = handle["task_id"].as_str().ok_or_else(|| Error::internal("no task_id in the queued response"))?;
    match crate::follow::follow(client, task, false, "shimmer notify list").await? {
        crate::follow::Followed::Finished(v) | crate::follow::Followed::ResultMissed(v) => Ok(v),
    }
}

/// The params naming `s`'s messages. Ids go as typed: the daemon finds the message an ending
/// means, or says it's too short or ends several (ADR 0031 §4).
fn selection_params(s: &Selection) -> Value {
    if !s.ids.is_empty() {
        return json!({"ids": s.ids});
    }
    if s.all {
        return json!({"all": true});
    }
    s.filters.params()
}

/// `remove`: named ids at once; anything bulk says how many and asks first.
async fn remove(client: &mut Client, s: &Selection, yes: bool, json: bool, prompt: &mut dyn Prompt) -> Result<String> {
    let mut params = selection_params(s);
    if yes {
        params["confirm"] = json!(true);
    }
    let data = match client.call("notify.remove", params.clone()).await {
        Ok(d) => d,
        Err(e) if e.code == ErrorCode::ConfirmationRequired => {
            let detail = e.detail.clone().unwrap_or_default();
            let (count, unread) = (detail["count"].as_u64().unwrap_or(0), detail["unread"].as_u64().unwrap_or(0));
            if count == 0 {
                return Ok(if json { render::json(&json!({"removed": 0})) } else { "nothing to remove".into() });
            }
            if !prompt.interactive() {
                return Err(Error::invalid_params(format!(
                    "this removes {count} {} ({unread} unread); add --yes to confirm",
                    plural(count, "message", "messages")
                )));
            }
            let unread_note = if unread > 0 { format!(", {unread} of them unread") } else { String::new() };
            if !prompt.confirm(&format!("Remove {count} {}{unread_note}? [y/N]", plural(count, "message", "messages")))
            {
                return Ok("nothing removed".into());
            }
            params["confirm"] = json!(true);
            client.call("notify.remove", params).await?
        }
        Err(e) => return Err(e),
    };
    if json {
        return Ok(render::json(&data));
    }
    let n = data["removed"].as_u64().unwrap_or(0);
    Ok(if n == 0 {
        "nothing to remove".into()
    } else {
        format!("✓ removed {n} {}", plural(n, "message", "messages"))
    })
}

// ---------------------------------------------------------------- drawing (pure)

/// The longest message text on one line of `list`; `show` has it all.
const LINE: usize = 72;

fn plural<'a>(n: u64, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 {
        one
    } else {
        many
    }
}

fn from(source: &str) -> &str {
    match source.split(':').next().unwrap_or(source) {
        "alert" => "alert",
        "automation" => "automation",
        "schedule" => "schedule",
        "fallback" => "fallback",
        _ => "send",
    }
}

fn line_text(m: &Value) -> String {
    let title = m["title"].as_str().unwrap_or_default();
    let body = m["body"].as_str().unwrap_or_default().replace('\n', " ");
    let text = if body.is_empty() { title.to_owned() } else { format!("{title} — {body}") };
    if text.chars().count() > LINE {
        text.chars().take(LINE - 1).chain(['…']).collect()
    } else {
        text
    }
}

/// The ending a list shows for a message: the daemon's `short`, unique across the whole inbox
/// rather than only these rows, so it always works given back (#246's review). The whole id if
/// it has none.
fn short(m: &Value) -> String {
    m["short"].as_str().or(m["id"].as_str()).unwrap_or_default().to_owned()
}

/// `notify list`: one line per message, then a footer with the counts and the next command.
fn list(data: &Value, filtered: bool, now: DateTime<Utc>) -> String {
    let items = data["items"].as_array().cloned().unwrap_or_default();
    let mut out = String::new();
    if items.is_empty() {
        out.push_str(if filtered { "no messages match" } else { "no messages yet" });
    } else {
        let rows: Vec<Vec<String>> = items
            .iter()
            .map(|m| {
                let unread = if m["read"] == false { "●" } else { " " };
                let high = if m["priority"] == "high" { "!" } else { " " };
                let at = when::timestamp(&m["at"]).map_or("-".into(), |t| when::relative(t, now));
                let state = match m["state"].as_str() {
                    Some("held") => paint(Role::Warning, "held"),
                    Some("failed") => paint(Role::Error, "failed"),
                    _ => String::new(),
                };
                let text = line_text(m);
                let text = if state.is_empty() { text } else { format!("{text}  {state}") };
                vec![
                    format!("{unread}{high}"),
                    short(m),
                    at,
                    from(m["source"].as_str().unwrap_or_default()).into(),
                    text,
                ]
            })
            .collect();
        let header = ["", "ID", "WHEN", "FROM", "MESSAGE"].map(String::from);
        out.push_str(&render::table(&header, &rows));
    }
    // The counts are the whole inbox's; on a filtered list, say how many are shown first, so
    // "3 unread" under one row doesn't read as that row's count.
    let c = &data["counts"];
    let totals: Vec<String> = [("unread", "unread"), ("held", "held"), ("failed", "failed")]
        .iter()
        .filter_map(|(key, word)| c[key].as_u64().filter(|n| *n > 0).map(|n| format!("{n} {word}")))
        .collect();
    let mut parts = Vec::new();
    match (filtered, items.is_empty(), totals.is_empty()) {
        (false, _, _) => parts.extend(totals),
        (true, true, true) => {}
        (true, true, false) => parts.push(format!("in all: {}", totals.join(", "))),
        (true, false, true) => parts.push(format!("{} shown", items.len())),
        (true, false, false) => parts.push(format!("{} shown · in all: {}", items.len(), totals.join(", "))),
    }
    if !items.is_empty() {
        parts.push(format!("see one: shimmer notify show {}", short(&items[0])));
    }
    if !parts.is_empty() {
        let _ = write!(out, "\n{}", paint(Role::Hint, &parts.join(" · ")));
    }
    if let Some(n) = data["removed_unread"].as_u64().filter(|n| *n > 0) {
        let _ = write!(
            out,
            "\n{}",
            paint(
                Role::Hint,
                &format!(
                    "the inbox cap has removed {n} unread {} (shimmer notify cap)",
                    plural(n, "message", "messages")
                )
            )
        );
    }
    out
}

/// `notify show`: one message in full.
fn show(m: &Value, now: DateTime<Utc>) -> String {
    let mut out = paint(Role::Heading, m["title"].as_str().unwrap_or_default());
    if let Some(body) = m["body"].as_str().filter(|b| !b.is_empty()) {
        let _ = write!(out, "\n{body}");
    }
    let at =
        when::timestamp(&m["at"]).map_or("-".into(), |t| format!("{} ({})", when::local(t), when::relative(t, now)));
    let deliveries: Vec<String> = m["delivery"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| match d["ok"].as_bool() {
            Some(true) => format!("{} ✓", d["medium"].as_str().unwrap_or("?")),
            _ => format!("{} ✗ {}", d["medium"].as_str().unwrap_or("?"), d["error"].as_str().unwrap_or("")),
        })
        .collect();
    let state = m["state"].as_str().unwrap_or("?");
    let state = if deliveries.is_empty() { state.to_owned() } else { format!("{state}: {}", deliveries.join(", ")) };
    let _ = write!(
        out,
        "\n\n  id        {}\n  when      {at}\n  priority  {}\n  from      {}\n  state     {state}",
        m["id"].as_str().unwrap_or_default(),
        m["priority"].as_str().unwrap_or("normal"),
        m["source"].as_str().unwrap_or("send"),
    );
    out
}

fn test(r: &Value) -> String {
    let deliveries = r["deliveries"].as_array().cloned().unwrap_or_default();
    if deliveries.is_empty() {
        return "Shimmer has no way to show messages yet, so they're kept in the inbox (shimmer notify list).\n\
                Desktop notifications come with notify's desktop support."
            .into();
    }
    let mut out = String::from("Sent \"Shimmer can reach you\" to:");
    for d in &deliveries {
        let medium = d["medium"].as_str().unwrap_or("?");
        if d["ok"] == true {
            let _ = write!(out, "\n  {medium} ✓");
        } else {
            let _ = write!(out, "\n  {medium} ✗ {}", d["error"].as_str().unwrap_or(""));
        }
    }
    out.push_str(
        "\nIf nothing appeared: check Focus / Do Not Disturb; on macOS, that notifications are allowed for\n\
         Script Editor; on Linux, that a notification daemon is running.",
    );
    out
}

fn resent(r: &Value) -> String {
    let items = r["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return "nothing to resend".into();
    }
    let delivered = items.iter().filter(|m| m["state"] == "delivered").count();
    let failed = items.len() - delivered;
    let mut out = format!("✓ resent {}: {delivered} delivered", items.len());
    if failed > 0 {
        let _ = write!(out, ", {failed} still failed (shimmer notify list --failed)");
    }
    out
}

fn quiet(d: &Value, changed: bool) -> String {
    let q = d["quiet"].as_object().cloned().unwrap_or_default();
    if q.is_empty() {
        return if changed {
            "✓ no quiet hours".into()
        } else {
            "no quiet hours (set some: shimmer notify quiet 22:00-08:00)".into()
        };
    }
    let label = |k: &str| k.replace('_', " ");
    let mut out =
        String::from(if changed { "✓ quiet hours, your local time:" } else { "quiet hours, your local time:" });
    for key in ["every_day", "weekdays", "weekends", "mon", "tue", "wed", "thu", "fri", "sat", "sun"] {
        if let Some(r) = q.get(key).and_then(Value::as_str) {
            let _ = write!(out, "\n  {:<10} {r}", label(key));
        }
    }
    if d["quiet_now"] == true {
        let _ = write!(out, "\n{}", paint(Role::Hint, "quiet now: low and normal messages wait"));
    }
    out
}

fn status(d: &Value, now: DateTime<Utc>) -> String {
    let c = &d["counts"];
    let n = |k: &str| c[k].as_u64().unwrap_or(0);
    let mut out = format!(
        "inbox      {} {} ({} unread, {} held, {} failed), keeps at most {}",
        n("total"),
        plural(n("total"), "message", "messages"),
        n("unread"),
        n("held"),
        n("failed"),
        d["cap"]
    );
    let paused = match d["paused"].as_object() {
        None => "no".to_owned(),
        Some(p) => {
            let till = when::timestamp(&p["until"].clone())
                .map_or("until you resume".into(), |t| format!("until {} ({})", local_time(t), when::relative(t, now)));
            let all = if p["all"] == true { ", everything waits" } else { ", high priority comes through" };
            paint(Role::Warning, &format!("yes, {till}{all}"))
        }
    };
    let _ = write!(out, "\npaused     {paused}");
    let q = d["quiet"].as_object().cloned().unwrap_or_default();
    let quiet = if q.is_empty() {
        "none".to_owned()
    } else {
        let ranges: Vec<String> =
            q.iter().map(|(k, v)| format!("{} {}", k.replace('_', " "), v.as_str().unwrap_or(""))).collect();
        let now_note = if d["quiet_now"] == true { " (quiet now)" } else { "" };
        format!("{}{now_note}", ranges.join(", "))
    };
    let _ = write!(out, "\nquiet      {quiet}");
    let a = &d["builtin_alerts"];
    let on = |k: &str| if a[k] == false { "off" } else { "on" };
    let _ = write!(
        out,
        "\nalerts     workspace-dirty {}, scheduled-task-failed {}",
        on("workspace_dirty"),
        on("scheduled_task_failed")
    );
    let sinks: Vec<&str> = d["sinks"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let sinks = if sinks.is_empty() { "none yet: messages stay in the inbox".to_owned() } else { sinks.join(", ") };
    let _ = write!(out, "\nreaches    {sinks}");
    if let Some(p) = d["settings_problem"].as_str() {
        let _ = write!(out, "\n{}", paint(Role::Error, &format!("settings problem: {p}")));
    }
    out
}

fn alerts(d: &Value) -> String {
    let a = &d["alerts"];
    let row = |name: &str, key: &str, what: &str| {
        let state = if a[key] == false { paint(Role::Warning, "off") } else { paint(Role::Success, "on ") };
        format!("{name:<22} {state}  {what}")
    };
    format!(
        "{}\n{}\n{}",
        row("workspace-dirty", "workspace_dirty", "a workspace couldn't launch"),
        row("scheduled-task-failed", "scheduled_task_failed", "a scheduled task failed"),
        paint(Role::Hint, "switch one: shimmer notify builtin-alerts off workspace-dirty")
    )
}

fn cap(d: &Value, changed: bool) -> String {
    let (max, now) = (d["max"].as_u64().unwrap_or(0), d["messages"].as_u64().unwrap_or(0));
    let mut out = format!("{}the inbox keeps at most {max} messages ({now} now)", if changed { "✓ " } else { "" });
    if let Some(n) = d["removed_unread"].as_u64().filter(|n| *n > 0) {
        let _ = write!(out, "; {n} unread {} removed to fit", plural(n, "was", "were"));
    }
    out
}

/// The next time `t` comes round on this machine's clock: today, or tomorrow if it's passed.
fn next_local<Tz: TimeZone>(t: NaiveTime, now: DateTime<Tz>) -> DateTime<Utc> {
    let tz = now.timezone();
    // `t` on `day`. A time the clocks skip that day (spring forward) is the first minute after the
    // gap that exists, as the scheduler does (ADR 0031 §2), never silently tomorrow (#276's review).
    let on = |day: chrono::NaiveDate| {
        let wall = day.and_time(t);
        tz.from_local_datetime(&wall).earliest().or_else(|| {
            (1..=180).find_map(|m| tz.from_local_datetime(&(wall + chrono::Duration::minutes(m))).earliest())
        })
    };
    let today = now.date_naive();
    on(today)
        .filter(|at| *at > now)
        .or_else(|| on(today + chrono::Duration::days(1)))
        .unwrap_or_else(|| now.clone() + chrono::Duration::days(1))
        .with_timezone(&Utc)
}

fn local_time(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(words: &[&str]) -> std::result::Result<NotifyCmd, String> {
        parse(words.iter().map(|s| s.to_string()).collect())
    }

    fn filters(pairs: Value) -> Filters {
        Filters(pairs.as_object().unwrap().clone())
    }

    #[test]
    fn list_and_count_take_the_filters() {
        assert_eq!(p(&["list"]), Ok(NotifyCmd::List { filters: Filters::default(), limit: None }));
        assert_eq!(
            p(&["list", "--unread", "--from", "alerts", "--since", "2026-10-01", "--limit", "5"]),
            Ok(NotifyCmd::List {
                filters: filters(json!({"unread": true, "from": "alerts", "since": "2026-10-01"})),
                limit: Some(5)
            })
        );
        assert_eq!(p(&["count", "--held"]), Ok(NotifyCmd::Count { filters: filters(json!({"state": "held"})) }));
        assert!(p(&["list", "--failed", "--held"]).unwrap_err().contains("pick one"));
        assert!(p(&["list", "inbox"]).unwrap_err().contains("takes filters"));
    }

    #[test]
    fn marking_and_removing_take_ids_all_or_filters_but_one_way() {
        let sel = |ids: &[&str], all: bool, f: Value| Selection {
            ids: ids.iter().map(|s| s.to_string()).collect(),
            all,
            filters: filters(f),
        };
        assert_eq!(
            p(&["mark-read", "a3f9k2", "b81xq0"]),
            Ok(NotifyCmd::Mark { read: true, selection: sel(&["a3f9k2", "b81xq0"], false, json!({})) })
        );
        assert_eq!(
            p(&["mark-unread", "--all"]),
            Ok(NotifyCmd::Mark { read: false, selection: sel(&[], true, json!({})) })
        );
        assert_eq!(
            p(&["remove", "--read", "--yes"]),
            Ok(NotifyCmd::Remove { selection: sel(&[], false, json!({"read": true})), yes: true })
        );
        assert!(p(&["remove"]).unwrap_err().contains("say which messages"));
        assert!(p(&["remove", "x", "--all"]).unwrap_err().contains("not several"));
        assert!(p(&["mark-read", "--all", "--yes"]).unwrap_err().contains("doesn't ask"));
        assert!(p(&["resend", "--all"]).unwrap_err().contains("ids or filters"));
    }

    #[test]
    fn send_pause_quiet_and_settings() {
        assert_eq!(
            p(&["send", "Build finished", "--body", "v1.4", "--priority", "high"]),
            Ok(NotifyCmd::Send {
                title: "Build finished".into(),
                body: Some("v1.4".into()),
                priority: Some("high".into())
            })
        );
        assert!(p(&["send", "Build", "finished"]).unwrap_err().contains("quotes"));
        assert_eq!(p(&["pause"]), Ok(NotifyCmd::Pause { until: PauseFor::UntilResume, all: false }));
        assert_eq!(p(&["pause", "2h", "--all"]), Ok(NotifyCmd::Pause { until: PauseFor::For(7200), all: true }));
        assert_eq!(
            p(&["pause", "until", "14:00"]),
            Ok(NotifyCmd::Pause { until: PauseFor::Until(NaiveTime::from_hms_opt(14, 0, 0).unwrap()), all: false })
        );
        assert!(p(&["pause", "until", "2pm"]).unwrap_err().contains("HH:MM"));
        assert_eq!(p(&["quiet"]), Ok(NotifyCmd::Quiet(QuietSet::Show)));
        assert_eq!(p(&["quiet", "off"]), Ok(NotifyCmd::Quiet(QuietSet::Off)));
        assert_eq!(
            p(&["quiet", "22:00-07:00", "--weekends", "00:00-10:00", "--days", "fri,sat", "01:00-10:00"]),
            Ok(NotifyCmd::Quiet(QuietSet::Set(vec![
                ("every_day".into(), "22:00-07:00".into()),
                ("weekends".into(), "00:00-10:00".into()),
                ("fri".into(), "01:00-10:00".into()),
                ("sat".into(), "01:00-10:00".into()),
            ])))
        );
        assert!(p(&["quiet", "--days", "someday", "01:00-02:00"]).unwrap_err().contains("isn't a day"));
        assert_eq!(
            p(&["builtin-alerts", "off", "workspace-dirty"]),
            Ok(NotifyCmd::BuiltinAlerts(Some((false, "workspace-dirty".into()))))
        );
        assert_eq!(p(&["cap", "500"]), Ok(NotifyCmd::Cap(Some(500))));
        assert!(p(&["cap", "lots"]).unwrap_err().contains("a number"));
        assert!(p(&["frobnicate"]).unwrap_err().contains("unknown notify command"));
    }

    fn msg(id: &str, title: &str, body: &str, read: bool, priority: &str, source: &str, state: &str) -> Value {
        json!({"id": id, "at": "2026-10-16T14:00:00Z", "title": title, "body": body, "read": read,
               "priority": priority, "source": source, "state": state})
    }

    #[test]
    fn list_is_one_line_each_with_a_footer() {
        crate::style::init(true, true, &Default::default());
        let now: DateTime<Utc> = "2026-10-16T14:02:00Z".parse().unwrap();
        let data = json!({
            "items": [
                msg("01JD2T00000000000000000A3F", "site couldn't launch", "Step 2/5 install failed. Log: logs/site-01JD2T.log and some more words to cut", false, "high", "alert:workspace_dirty", "failed"),
                msg("01JD2T00000000000000000B81", "Standup", "", true, "normal", "schedule", "delivered"),
            ],
            "counts": {"total": 2, "unread": 1, "held": 0, "failed": 1},
        });
        let out = list(&data, false, now);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].contains("ID") && lines[0].contains("MESSAGE"), "{out}");
        assert!(lines[1].starts_with("●!") && lines[1].contains("2m ago") && lines[1].contains("alert"), "{out}");
        assert!(
            lines[1].contains("site couldn't launch — Step 2/5")
                && lines[1].contains('…')
                && lines[1].ends_with("failed"),
            "{out}"
        );
        assert!(lines[2].starts_with("  ") && lines[2].contains("Standup") && lines[2].contains("schedule"), "{out}");
        assert!(lines[3].starts_with("1 unread · 1 failed · see one: shimmer notify show"), "{out}");
        let filtered = list(&json!({"items": [data["items"][1].clone()], "counts": data["counts"].clone()}), true, now);
        assert!(
            filtered.lines().last().unwrap().starts_with("1 shown · in all: 1 unread, 1 failed · see one:"),
            "a filtered list says what it shows, then the inbox's totals: {filtered}"
        );
        assert_eq!(list(&json!({"items": [], "counts": {}}), false, now), "no messages yet");
        assert_eq!(list(&json!({"items": [], "counts": {}}), true, now), "no messages match");
        assert_eq!(
            list(&json!({"items": [], "counts": {"unread": 2}}), true, now),
            "no messages match\nin all: 2 unread"
        );
    }

    #[test]
    fn show_has_it_all() {
        crate::style::init(true, true, &Default::default());
        let now: DateTime<Utc> = "2026-10-16T14:05:00Z".parse().unwrap();
        let mut m =
            msg("01JD2T00000000000000000A3F", "Build finished", "v1.4 is live", false, "high", "send", "failed");
        m["delivery"] = json!([{"medium": "desktop", "ok": false, "error": "no desktop session"}]);
        let out = show(&m, now);
        assert!(out.starts_with("Build finished\nv1.4 is live\n\n  id        01JD2T00000000000000000A3F"), "{out}");
        assert!(
            out.contains("(5m ago)")
                && out.contains("priority  high")
                && out.contains("state     failed: desktop ✗ no desktop session"),
            "{out}"
        );
    }

    #[test]
    fn the_settings_read_plainly() {
        crate::style::init(true, true, &Default::default());
        let now: DateTime<Utc> = "2026-10-16T14:00:00Z".parse().unwrap();
        let s = status(
            &json!({"counts": {"total": 3, "unread": 2, "held": 1, "failed": 0}, "cap": 2000, "paused": null,
                    "quiet": {"every_day": "22:00-08:00"}, "quiet_now": false,
                    "builtin_alerts": {"workspace_dirty": true, "scheduled_task_failed": false}, "sinks": [],
                    "settings_problem": null}),
            now,
        );
        assert!(s.contains("inbox      3 messages (2 unread, 1 held, 0 failed), keeps at most 2000"), "{s}");
        assert!(s.contains("paused     no") && s.contains("quiet      every day 22:00-08:00"), "{s}");
        assert!(s.contains("scheduled-task-failed off") && s.contains("none yet: messages stay in the inbox"), "{s}");
        assert_eq!(quiet(&json!({"quiet": {}}), false), "no quiet hours (set some: shimmer notify quiet 22:00-08:00)");
        assert!(quiet(&json!({"quiet": {"weekends": "00:00-10:00"}, "quiet_now": true}), true)
            .contains("  weekends   00:00-10:00"));
        assert_eq!(cap(&json!({"max": 500, "messages": 37}), true), "✓ the inbox keeps at most 500 messages (37 now)");
        assert!(test(&json!({"deliveries": []})).starts_with("Shimmer has no way to show messages yet"));
        assert!(test(&json!({"deliveries": [{"medium": "desktop", "ok": true}]})).contains("desktop ✓"));
        assert_eq!(
            resent(&json!({"items": [{"state": "delivered"}, {"state": "failed"}]})),
            "✓ resent 2: 1 delivered, 1 still failed (shimmer notify list --failed)"
        );
    }

    #[test]
    fn until_a_time_is_the_next_time_it_comes_round() {
        let now = Local.with_ymd_and_hms(2026, 10, 16, 15, 0, 0).unwrap();
        let later = next_local(NaiveTime::from_hms_opt(16, 0, 0).unwrap(), now);
        assert_eq!(later.with_timezone(&Local).format("%d %H:%M").to_string(), "16 16:00");
        let tomorrow = next_local(NaiveTime::from_hms_opt(9, 0, 0).unwrap(), now);
        assert_eq!(tomorrow.with_timezone(&Local).format("%d %H:%M").to_string(), "17 09:00");
    }

    /// New York skips 02:00-03:00 on 2026-03-08. "Until 02:30" that morning is 03:00, the first
    /// minute that exists, never 02:30 the next day (#276's review).
    #[test]
    fn until_a_time_the_clocks_skip_is_the_first_minute_after_the_gap() {
        let ny = chrono_tz::America::New_York;
        let now = ny.with_ymd_and_hms(2026, 3, 8, 1, 0, 0).unwrap();
        let at = next_local(NaiveTime::from_hms_opt(2, 30, 0).unwrap(), now).with_timezone(&ny);
        assert_eq!(at.format("%Y-%m-%d %H:%M").to_string(), "2026-03-08 03:00");
        // An ordinary time that day is unchanged.
        let at = next_local(NaiveTime::from_hms_opt(9, 0, 0).unwrap(), now).with_timezone(&ny);
        assert_eq!(at.format("%Y-%m-%d %H:%M").to_string(), "2026-03-08 09:00");
    }
}
